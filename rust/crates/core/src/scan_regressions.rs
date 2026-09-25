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

fn gauges(store: &NativeStore, context: &Value) -> Value {
    store.request(
        &json!({"schema":SCHEMA,"id":"gauges","operation":"stats"}),
        context,
        &Cancellation::default(),
    )["data"]["gauges"]
        .clone()
}

#[test]
fn writer_failure_releases_lease_without_opening_next_source() {
    let fixture = Fixture::new();
    let first = fixture.source("first.jsonl", "a");
    let second = fixture.source("second.jsonl", "b");
    let store = NativeStore::new(&json!({})).unwrap();
    let mut scan = ScanSession::new(&store, limits(), Cancellation::default());
    let result = scan.run(&fixture.plan(vec![first, second]), |_, _, _, _| {
        Err(SnapshotError::new(Status::OutputLimit, "writer failed"))
    });
    assert!(!result.complete);
    assert_eq!(result.progress.source_opens, 1);
    assert_eq!(result.progress.sources, 1);
    assert_eq!(
        gauges(&store, &scan.context)["active_leases"].as_u64(),
        Some(0)
    );
    assert_eq!(
        gauges(&store, &scan.context)["pending_loads"].as_u64(),
        Some(0)
    );
}

fn load_request(scan: &ScanSession<'_>, path: &Path) -> Value {
    let mut request = scan.bounded_request("acquire");
    request.insert("path", json!(path.to_string_lossy().as_ref()));
    request.insert("classifier", json!({"id":"native","version":"1"}));
    request
}

#[test]
fn dropping_cancelled_driver_keeps_another_waiters_load_alive() {
    let fixture = Fixture::new();
    let path = fixture.source("shared.jsonl", "shared");
    let store =
        NativeStore::new(&json!({"max_read_bytes_per_step":16,"max_events_per_step":1})).unwrap();
    let mut first = ScanSession::new(&store, limits(), Cancellation::default());
    let mut second = ScanSession::new(&store, limits(), Cancellation::default());
    let a = first.request(load_request(&first, &path)).unwrap();
    let mut b = second.request(load_request(&second, &path)).unwrap();
    assert!(a["cursor"].as_str().is_some());
    assert_eq!(
        a["data"]["reservation"]["load_id"].as_str(),
        b["data"]["reservation"]["load_id"].as_str()
    );
    first.cancel.cancel();
    assert!(first
        .request(json!({"operation":"resume","cursor":a["cursor"]}))
        .is_err());
    drop(first);
    assert_eq!(
        gauges(&store, &second.context)["pending_loads"].as_u64(),
        Some(1)
    );
    for _ in 0..100 {
        let Some(cursor) = b["cursor"].as_str() else {
            break;
        };
        b = second
            .request(json!({"operation":"resume","cursor":cursor}))
            .unwrap();
    }
    assert_eq!(b["status"].as_str(), Some("ok"));
    let snapshot = store
        .pin(&b["data"]["description"]["handle"], &second.context)
        .unwrap();
    assert_eq!(snapshot.event_count, 1);
    store.discard_response(&b, &second.context).unwrap();
    assert_eq!(
        gauges(&store, &second.context)["active_leases"].as_u64(),
        Some(0)
    );
    assert_eq!(
        gauges(&store, &second.context)["pending_loads"].as_u64(),
        Some(0)
    );
}

#[test]
fn cumulative_read_budget_survives_small_load_pages_and_sources() {
    let fixture = Fixture::new();
    let first = fixture.source("first.jsonl", "a");
    let second = fixture.source("second.jsonl", "b");
    let config = json!({"max_read_bytes_per_step":16,"max_events_per_step":1});
    let baseline = NativeStore::new(&config).unwrap();
    let mut once = ScanSession::new(&baseline, limits(), Cancellation::default());
    let reference = once.run(&fixture.plan(vec![first.clone()]), |_, _, _, _| {
        Ok(ScanControl::Continue)
    });
    assert!(reference.complete, "{:?}", reference.reason);
    let mut bound = limits();
    bound.max_read_bytes = reference.progress.source_bytes + 1;
    let store = NativeStore::new(&config).unwrap();
    let mut scan = ScanSession::new(&store, bound, Cancellation::default());
    let mut visits = 0;
    let result = scan.run(&fixture.plan(vec![first, second]), |_, _, _, _| {
        visits += 1;
        Ok(ScanControl::Continue)
    });
    assert!(!result.complete);
    assert_eq!(visits, 1);
    assert!(result.progress.source_bytes <= bound.max_read_bytes);
    let context = scan.context.clone();
    drop(scan);
    assert_eq!(gauges(&store, &context)["active_leases"].as_u64(), Some(0));
    assert_eq!(gauges(&store, &context)["pending_loads"].as_u64(), Some(0));
}

#[test]
fn corpus_quota_on_final_line_is_complete() {
    let fixture = Fixture::new();
    let path = fixture.0.join("corpus.txt");
    for contents in ["last\n", "last"] {
        std::fs::write(&path, contents).unwrap();
        let result = scan_corpus(
            &path,
            &mut ScanBudget::new(limits()),
            &Cancellation::default(),
            |_, _, _, _| Ok(true),
        );
        assert!(result.complete, "{:?}", result.reason);
    }
}

#[test]
fn discovery_timestamps_keep_legacy_float_ordering() {
    let fixture = Fixture::new();
    let path = fixture.source("mtime.jsonl", "a");
    let file = std::fs::File::open(&path).unwrap();
    for (before_epoch, nanos) in [
        (false, 1),
        (false, 123_456_789),
        (true, 1),
        (true, 100_000_000),
    ] {
        let duration = std::time::Duration::from_nanos(nanos);
        let modified = if before_epoch {
            std::time::UNIX_EPOCH - duration
        } else {
            std::time::UNIX_EPOCH + duration
        };
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let metadata = file.metadata().unwrap();
        let stamp = crate::snapshot::SourceStamp::of(&metadata);
        assert_eq!(
            discovered_mtime(stamp.mtime_ns).to_bits(),
            crate::discovery::mtime_secs(&metadata).to_bits()
        );
    }
}
