use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cc-scan-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path,format!("{{\"type\":\"user\",\"uuid\":\"a\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n")).unwrap();
        path
    }
    fn plan(&self, paths: Vec<PathBuf>) -> ScanPlan {
        ScanPlan {
            paths,
            root: self.0.clone(),
            project: None,
            contains: None,
            source_limit: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn limits() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 1024 * 1024,
        max_events: 1000,
        max_items: 1000,
        max_output_bytes: 1024 * 1024,
        max_sources: 100,
        max_discovery_entries: 1000,
        deadline_unix_ms: crate::snapshot::now_ms() + 30_000,
    }
}

#[test]
fn quota_stops_before_opening_later_sources() {
    let fixture = Fixture::new();
    let first = fixture.source("first.jsonl", "match");
    let missing = fixture.0.join("never-opened.jsonl");
    let store = NativeStore::new(&json!({})).unwrap();
    let mut scan = ScanSession::new(&store, limits(), Cancellation::default());
    let mut visits = 0;
    let result = scan.run(&fixture.plan(vec![first, missing]), |_, snapshot, _, _| {
        visits += 1;
        assert_eq!(snapshot.event_count, 1);
        Ok(ScanControl::Stop {
            source_complete: true,
        })
    });
    assert_eq!(visits, 1);
    assert_eq!(result.reason.as_deref(), Some("result_limit"));
    assert_eq!(result.progress.sources, 1);
    assert_eq!(result.progress.source_opens, 1);
}

#[test]
fn exhausted_discovery_does_not_read_a_body() {
    let fixture = Fixture::new();
    fixture.source("a.jsonl", "a");
    fixture.source("b.jsonl", "b");
    let store = NativeStore::new(&json!({})).unwrap();
    let mut bound = limits();
    bound.max_discovery_entries = 1;
    let mut scan = ScanSession::new(&store, bound, Cancellation::default());
    let result = scan.run(&fixture.plan(vec![]), |_, _, _, _| {
        panic!("partial discovery visited a source")
    });
    assert!(!result.complete);
    assert_eq!(result.progress.source_opens, 0);
}

#[test]
fn explicit_paths_keep_their_order() {
    let fixture = Fixture::new();
    let a = fixture.source("a.jsonl", "a");
    let b = fixture.source("b.jsonl", "b");
    let store = NativeStore::new(&json!({})).unwrap();
    let mut scan = ScanSession::new(&store, limits(), Cancellation::default());
    let mut paths = Vec::new();
    let result = scan.run(
        &fixture.plan(vec![b.clone(), a.clone()]),
        |path, _, _, _| {
            paths.push(path.to_owned());
            Ok(ScanControl::Continue)
        },
    );
    assert!(result.complete, "{:?}", result.reason);
    assert_eq!(paths, vec![b, a]);
}

#[test]
fn cancellation_prevents_discovery_and_reads() {
    let fixture = Fixture::new();
    let store = NativeStore::new(&json!({})).unwrap();
    let cancel = Cancellation::default();
    cancel.cancel();
    let mut scan = ScanSession::new(&store, limits(), cancel);
    let result = scan.run(&fixture.plan(vec![]), |_, _, _, _| {
        panic!("cancelled visitor")
    });
    assert!(!result.complete);
    assert!(result.reason.unwrap().contains("cancelled"));
    assert_eq!(result.progress.source_opens, 0);
}

#[test]
fn projection_budget_is_cumulative() {
    let mut bound = limits();
    bound.max_read_bytes = 10;
    let mut budget = ScanBudget::new(bound);
    let cancel = Cancellation::default();
    budget.charge_projection(6, 1, &cancel).unwrap();
    assert!(budget.charge_projection(5, 1, &cancel).is_err());
    assert_eq!(budget.progress.projection_bytes, 6);
    assert_eq!(budget.progress.examined_events, 1);
}

#[test]
fn long_corpus_line_is_incomplete_without_visiting_it() {
    let fixture = Fixture::new();
    let path = fixture.0.join("corpus.txt");
    std::fs::write(&path, "abcdefghijk\n").unwrap();
    let mut bound = limits();
    bound.max_read_bytes = 4;
    let mut budget = ScanBudget::new(bound);
    let result = scan_corpus(
        &path,
        &mut budget,
        &Cancellation::default(),
        |_, _, _, _| panic!("partial line visited"),
    );
    assert!(!result.complete);
    assert_eq!(result.progress.source_bytes, 4);
}

#[test]
fn corpus_complete_zero_and_quota_are_distinct() {
    let fixture = Fixture::new();
    let path = fixture.0.join("corpus.txt");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let mut visited = Vec::new();
    let result = scan_corpus(
        &path,
        &mut ScanBudget::new(limits()),
        &Cancellation::default(),
        |i, text, _, _| {
            visited.push((i, text.to_owned()));
            Ok(false)
        },
    );
    assert!(result.complete);
    assert_eq!(visited, vec![(1, "one".into()), (2, "two".into())]);
    let result = scan_corpus(
        &path,
        &mut ScanBudget::new(limits()),
        &Cancellation::default(),
        |_, _, _, _| Ok(true),
    );
    assert!(!result.complete);
    assert_eq!(result.reason.as_deref(), Some("result_limit"));
    assert_eq!(result.progress.source_bytes, 8);
}
