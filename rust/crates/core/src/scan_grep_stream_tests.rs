use super::*;
use crate::scan::{ScanControl, ScanPlan, ScanProgress, ScanSession};
use crate::scan_checkpoint::{
    GrepCheckpoints, QueryLayer, ReducerState, SourceRecord, LOCK_STRIPES, MAX_RECORDS,
    MAX_RECORD_BYTES, PREFIX_SEGMENT, PROTOCOL_DIR,
};
use crate::scan_stream::{LineSpan, SourceStream, VALIDATE_HOOKS};
use crate::snapshot::{NativeStore, WorkLimits};
use sha2::{Digest, Sha256};
use sonic_rs::{json, Value};
use std::collections::BTreeSet;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const LOCKED_SCAN_WAIT: Duration = Duration::from_secs(30);

type Interrupt = fn(&mut ScanBudget<'_>, &Cancellation);

struct Source(PathBuf);

impl Source {
    fn new(lines: &[String], terminated: bool) -> Self {
        let mut text = lines.join("\n");
        if terminated {
            text.push('\n');
        }
        Self::raw(text.as_bytes())
    }

    fn raw(contents: &[u8]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cc-grep-stream-{}-{}.jsonl",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        Self(path)
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

#[derive(Debug, PartialEq)]
struct Emitted {
    index: usize,
    pattern_ids: Option<Vec<usize>>,
    opens_source: bool,
    opens_window: bool,
    names: Vec<(String, Option<String>)>,
}

impl Emitted {
    fn of(event: &GrepEvent<'_>) -> Self {
        Self {
            index: event.index,
            pattern_ids: event.pattern_ids.map(<[usize]>::to_vec),
            opens_source: event.opens_source,
            opens_window: event.opens_window,
            names: event
                .entry
                .tool_results()
                .map(|result| {
                    (
                        result.tool_use_id.clone(),
                        event
                            .names
                            .get(result.tool_use_id.as_str())
                            .map(|name| (*name).to_owned()),
                    )
                })
                .collect(),
        }
    }
}

#[derive(Debug, PartialEq)]
struct Run {
    emitted: Vec<Emitted>,
    counts: Vec<usize>,
    stop: Option<bool>,
    names_through: Option<u64>,
    complete: bool,
}

fn line(value: Value) -> String {
    sonic_rs::to_string(&value).unwrap()
}

fn user(uuid: &str, parent: Option<&str>, content: Value) -> Value {
    json!({"type":"user","uuid":uuid,"parentUuid":parent,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":content}})
}

fn assistant(uuid: &str, parent: &str, blocks: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"parentUuid":parent,"sessionId":"s","timestamp":"2026-01-01T00:00:01Z","message":{"model":"test","content":blocks}})
}

fn tool(id: &str, name: &str, input: Value) -> Value {
    json!({"type":"tool_use","id":id,"name":name,"input":input})
}

fn result(uuid: &str, parent: &str, id: &str, text: &str) -> Value {
    user(
        uuid,
        Some(parent),
        json!([{"type":"tool_result","tool_use_id":id,"content":text,"is_error":false}]),
    )
}

fn limits() -> WorkLimits {
    WorkLimits {
        max_read_bytes: 64 * 1024 * 1024,
        max_source_read_bytes: 64 * 1024 * 1024,
        max_events: 1_000_000,
        max_items: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        max_sources: 100,
        max_discovery_entries: 100,
        deadline_unix_ms: crate::snapshot::now_ms() + 60_000,
    }
}

fn options() -> GrepOptions {
    GrepOptions {
        kinds: vec![],
        tool: None,
        errors: false,
        where_text: true,
        where_thinking: true,
        where_tools: true,
        context: 0,
        with_result: false,
        ignore_case: false,
    }
}

fn reducer<'store>(
    patterns: &[(&str, Option<usize>)],
    options: GrepOptions,
    budget: &mut ScanBudget<'store>,
) -> GrepReducer<'store> {
    GrepReducer::new(
        patterns
            .iter()
            .enumerate()
            .map(|(id, (pattern, cap))| GrepPatternSpec {
                id,
                pattern: (*pattern).into(),
                max_matches: *cap,
            })
            .collect(),
        options,
        budget,
        &Cancellation::default(),
    )
    .unwrap()
}

fn stop(control: &ScanControl) -> (Option<bool>, Option<u64>) {
    match control {
        ScanControl::Continue => (None, None),
        ScanControl::Stop {
            source_complete,
            names_through,
        } => (Some(*source_complete), *names_through),
    }
}

fn streamed(
    path: &Path,
    patterns: &[(&str, Option<usize>)],
    options: GrepOptions,
    render_names: bool,
    limits: WorkLimits,
) -> (Result<Run, SnapshotError>, Vec<Emitted>, ScanProgress) {
    checkpointed(path, patterns, options, render_names, limits, None)
}

fn checkpointed(
    path: &Path,
    patterns: &[(&str, Option<usize>)],
    options: GrepOptions,
    render_names: bool,
    limits: WorkLimits,
    checkpoints: Option<&GrepCheckpoints>,
) -> (Result<Run, SnapshotError>, Vec<Emitted>, ScanProgress) {
    configured(
        &json!({}),
        path,
        patterns,
        options,
        render_names,
        limits,
        checkpoints,
    )
}

fn configured(
    config: &Value,
    path: &Path,
    patterns: &[(&str, Option<usize>)],
    options: GrepOptions,
    render_names: bool,
    limits: WorkLimits,
    checkpoints: Option<&GrepCheckpoints>,
) -> (Result<Run, SnapshotError>, Vec<Emitted>, ScanProgress) {
    let store = NativeStore::new(config).unwrap();
    let mut budget = ScanBudget::new(&store, limits);
    let mut grep = reducer(patterns, options, &mut budget);
    let mut emitted = Vec::new();
    let control = grep.scan_stream(
        path,
        render_names,
        checkpoints,
        &mut budget,
        &Cancellation::default(),
        |event, budget, cancel| {
            let _staging = event.preflight_render(budget, cancel)?;
            emitted.push(Emitted::of(&event));
            Ok(())
        },
    );
    let run = run_of(control, &grep, &mut emitted);
    (run, emitted, budget.progress.clone())
}

fn run_of(
    control: Result<Option<ScanControl>, SnapshotError>,
    grep: &GrepReducer<'_>,
    emitted: &mut Vec<Emitted>,
) -> Result<Run, SnapshotError> {
    control.map(|control| {
        let (stop, names_through) = stop(&control.expect("claude source streams"));
        Run {
            emitted: std::mem::take(emitted),
            counts: grep.counts().to_vec(),
            stop,
            names_through,
            complete: grep.complete(),
        }
    })
}

fn prepared(path: &Path, patterns: &[(&str, Option<usize>)], options: GrepOptions) -> Run {
    let store = NativeStore::new(&json!({})).unwrap();
    let mut session = ScanSession::new(&store, limits(), Cancellation::default());
    let mut grep = reducer(patterns, options, &mut session.budget);
    let mut emitted = Vec::new();
    let control = session
        .visit_snapshot(path, &mut |_, snapshot, budget, cancel| {
            let result = grep.scan_source(snapshot, budget, cancel)?;
            result.emit(snapshot, budget, cancel, |event, budget, cancel| {
                let _staging = event.preflight_render(budget, cancel)?;
                emitted.push(Emitted::of(&event));
                Ok(())
            })?;
            Ok(if result.quota_reached {
                ScanControl::Stop {
                    source_complete: result.source_complete,
                    names_through: None,
                }
            } else {
                ScanControl::Continue
            })
        })
        .unwrap();
    Run {
        emitted,
        counts: grep.counts().to_vec(),
        stop: stop(&control).0,
        names_through: None,
        complete: grep.complete(),
    }
}

fn transcript() -> Vec<String> {
    let long = format!("needle {}", "x".repeat(150_000));
    vec![
        line(json!({"type":"last-prompt","leafUuid":"u9","sessionId":"s"})),
        line(user("u0", None, json!("first needle"))),
        line(assistant(
            "a1",
            "u0",
            json!([{"type":"text","text":"about the needle"},tool("t1","Bash",json!({"command":"echo needle"}))]),
        )),
        line(result("u1", "a1", "t1", "needle output")),
        String::new(),
        "not json {".to_owned(),
        "[1,2]".to_owned(),
        line(json!({"type":"mode","mode":"normal","sessionId":"s"})),
        line({
            let mut value = user("u2", Some("u1"), json!("sidechain needle"));
            value.insert("isSidechain", json!(true));
            value
        }),
        line(assistant(
            "a2",
            "u2",
            json!([tool(
                "t2",
                "Edit",
                json!({"file_path":"/a","old_string":"needle","new_string":"thread"})
            )]),
        )),
        line(result("u3", "a2", "t2", "edited")),
        line(result("u4", "u3", "t3", "early needle")),
        line(assistant(
            "a3",
            "u4",
            json!([tool("t3", "Read", json!({"file_path":"/needle"}))]),
        )),
        line(assistant(
            "a1",
            "u0",
            json!([{"type":"text","text":"about the needle"},tool("t1","Bash",json!({"command":"echo needle"}))]),
        )),
        line(user("u5", Some("a3"), json!(long))),
        line(user("u6", Some("u5"), json!("needle again"))),
        line(user("u7", Some("u6"), json!("tail"))),
    ]
}

fn case(name: &str) -> (Vec<(&'static str, Option<usize>)>, GrepOptions) {
    let mut options = options();
    let patterns = match name {
        "plain" => vec![("needle", None)],
        "quota" => vec![("needle", Some(3))],
        "multi" => vec![("needle", Some(2)), ("tail|edited", Some(1))],
        "context" => {
            options.context = 2;
            vec![("needle", None)]
        }
        "context-quota" => {
            options.context = 1;
            vec![("needle", Some(2))]
        }
        "kinds" => {
            options.kinds = vec!["user".into()];
            vec![("needle", None)]
        }
        "tool-forward" => {
            options.tool = Some("Read".into());
            vec![("needle", None)]
        }
        "tool-bash" => {
            options.tool = Some("Bash".into());
            vec![("needle|output", None)]
        }
        "ignore-case" => {
            options.ignore_case = true;
            vec![("NEEDLE", Some(1))]
        }
        "final" => vec![("tail", Some(1))],
        "absent" => vec![("absent", None)],
        _ => unreachable!("unknown case {name}"),
    };
    (patterns, options)
}

#[test]
fn streamed_grep_matches_the_prepared_snapshot_path() {
    let names = [
        "plain",
        "quota",
        "multi",
        "context",
        "context-quota",
        "kinds",
        "tool-forward",
        "tool-bash",
        "ignore-case",
        "final",
        "absent",
    ];
    for terminated in [true, false] {
        let source = Source::new(&transcript(), terminated);
        for name in names {
            for render_names in [true, false] {
                let (patterns, options) = case(name);
                let mut expected = prepared(&source.0, &patterns, options);
                let (patterns, options) = case(name);
                let (actual, _, _) =
                    streamed(&source.0, &patterns, options, render_names, limits());
                let mut actual = actual.unwrap();
                assert!(actual.names_through.is_none() || actual.stop == Some(false));
                actual.names_through = None;
                if !render_names {
                    for emitted in expected.emitted.iter_mut().chain(&mut actual.emitted) {
                        emitted.names.clear();
                    }
                }
                assert_eq!(
                    actual, expected,
                    "{name}, terminated {terminated}, render names {render_names}"
                );
            }
        }
    }
}

#[test]
fn quota_stops_reading_before_the_rest_of_the_file() {
    let mut lines = vec![
        line(user("u0", None, json!("a"))),
        line(user("u1", Some("u0"), json!("b"))),
        line(user("u2", Some("u1"), json!("needle"))),
    ];
    lines.extend((3..4003).map(|index| {
        line(user(
            &format!("u{index}"),
            Some("u2"),
            json!("x".repeat(1000)),
        ))
    }));
    let source = Source::new(&lines, true);
    let (run, _, progress) = streamed(&source.0, &[("needle", Some(1))], options(), true, limits());
    let run = run.unwrap();
    assert_eq!(run.stop, Some(false));
    assert_eq!(run.emitted.len(), 1);
    assert!(std::fs::metadata(&source.0).unwrap().len() > 4_000_000);
    assert_eq!(progress.source_bytes, 64 * 1024);
    assert_eq!(progress.parsed_events, 4);
    assert_eq!(progress.examined_events, 4);
    assert_eq!(progress.preparation_reserved_events, 0);
}

#[test]
fn complete_scan_charges_exact_bytes_lines_and_events() {
    let lines = vec![
        line(user("u0", None, json!("needle"))),
        String::new(),
        "garbage".to_owned(),
        line(user("u1", Some("u0"), json!("other"))),
        line(user("u2", Some("u1"), json!("needle"))),
    ];
    let source = Source::new(&lines, true);
    let (run, _, progress) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    let run = run.unwrap();
    assert_eq!(run.stop, None);
    assert_eq!(run.counts, vec![2]);
    assert_eq!(
        progress.source_bytes as u64,
        std::fs::metadata(&source.0).unwrap().len()
    );
    assert_eq!(progress.parsed_events, 5);
    assert_eq!(progress.examined_events, 3 + 2);
    assert_eq!(progress.source_opens, 1);
}

#[test]
fn exhausted_source_budget_keeps_earlier_hits_and_reports_incomplete() {
    let mut lines = vec![line(user("u0", None, json!("needle")))];
    lines.extend((1..2000).map(|index| {
        line(user(
            &format!("u{index}"),
            Some("u0"),
            json!("x".repeat(1000)),
        ))
    }));
    let source = Source::new(&lines, true);
    let mut bound = limits();
    bound.max_source_read_bytes = 200_000;
    let (run, emitted, progress) = streamed(&source.0, &[("needle", None)], options(), true, bound);
    let error = run.err().unwrap();
    assert_eq!(
        (error.status, error.reason.as_str()),
        (Status::Incomplete, "source_read_limit")
    );
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].index, 0);
    assert_eq!(progress.source_bytes, 200_000);
}

#[test]
fn exhausted_event_budget_stops_parsing() {
    let lines: Vec<_> = (0..100)
        .map(|index| line(user(&format!("u{index}"), None, json!("absent"))))
        .collect();
    let source = Source::new(&lines, true);
    let mut bound = limits();
    bound.max_events = 41;
    let (run, _, progress) = streamed(&source.0, &[("needle", None)], options(), true, bound);
    assert_eq!(run.err().unwrap().status, Status::Incomplete);
    assert_eq!(progress.parsed_events + progress.examined_events, 41);
}

#[test]
fn tool_name_redefined_after_use_is_incomplete() {
    let lines = vec![
        line(result("u0", "a0", "t", "needle")),
        line(assistant("a0", "u0", json!([tool("t", "Bash", json!({}))]))),
        line(assistant("a1", "a0", json!([tool("t", "Read", json!({}))]))),
    ];
    let source = Source::new(&lines, true);
    let (run, emitted, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    let error = run.err().unwrap();
    assert_eq!(error.status, Status::Incomplete);
    assert!(error.reason.contains("redefined"), "{}", error.reason);
    assert_eq!(emitted[0].names, vec![("t".into(), Some("Bash".into()))]);
}

#[test]
fn codex_sources_fall_back_to_preparation() {
    let source = Source::new(
        &[line(json!({"type":"session_meta","payload":{"id":"x"}}))],
        true,
    );
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let mut grep = reducer(&[("x", None)], options(), &mut budget);
    let control = grep
        .scan_stream(
            &source.0,
            true,
            None,
            &mut budget,
            &Cancellation::default(),
            |_, _, _| panic!("codex event streamed"),
        )
        .unwrap();
    assert!(control.is_none());
    assert_eq!(budget.progress.parsed_events, 0);
}

#[test]
fn stream_reads_the_pinned_prefix_and_rejects_truncation() {
    let source = Source::new(&["a".into(), "b".into()], true);
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let cancel = Cancellation::default();
    let mut stream = SourceStream::open(&source.0, 0, &mut budget, &cancel).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&source.0)
        .unwrap()
        .write_all(b"c\n")
        .unwrap();
    let mut lines = Vec::new();
    while let Some(line) = stream.next_line(&mut budget, &cancel).unwrap() {
        lines.push(stream.bytes(&line).to_vec());
    }
    assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec()]);
    assert_eq!(budget.progress.source_bytes, 4);
    assert!(stream.verify().unwrap().is_some());
    std::fs::File::options()
        .write(true)
        .open(&source.0)
        .unwrap()
        .set_len(1)
        .unwrap();
    assert_eq!(stream.verify().unwrap_err().status, Status::Changed);
}

#[test]
fn refused_span_revalidation_charges_no_validation_bytes() {
    let source = Source::raw(b"");
    let size = 17 * 1024 * 1024;
    std::fs::File::options()
        .write(true)
        .open(&source.0)
        .unwrap()
        .set_len(size as u64)
        .unwrap();
    let store = NativeStore::new(
        &json!({"max_retained_bytes":16*1024*1024,"reserved_hook_accounted_bytes":0}),
    )
    .unwrap();
    let span = LineSpan {
        offset: 0,
        len: size,
        terminated: false,
    };
    let cancelled = Cancellation::default();
    cancelled.cancel();
    for (cancel, status) in [
        (cancelled, Status::Cancelled),
        (Cancellation::default(), Status::RetainedLimit),
    ] {
        let mut budget = ScanBudget::new(&store, limits());
        let mut stream =
            SourceStream::open(&source.0, 0, &mut budget, &Cancellation::default()).unwrap();
        assert_eq!(
            stream
                .revalidate_span(&span, &mut budget, &cancel)
                .unwrap_err()
                .status,
            status
        );
        assert_eq!(budget.progress.validated_bytes, 0, "{status:?}");
    }
}

struct Cache(PathBuf, GrepCheckpoints);

impl Cache {
    fn new() -> Self {
        Self::indexing(crate::scan_index::MIN_SOURCE_BYTES)
    }

    fn indexing(from: u64) -> Self {
        Self::with(from, PREFIX_SEGMENT)
    }

    fn segmented(segment: u64) -> Self {
        Self::with(crate::scan_index::MIN_SOURCE_BYTES, segment)
    }

    fn with(from: u64, segment: u64) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cc-grep-checkpoints-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self(
            dir.join(PROTOCOL_DIR),
            GrepCheckpoints::new(dir, "test".into())
                .indexing_from(from)
                .segmented(segment),
        )
    }

    fn root(&self) -> &Path {
        self.0.parent().unwrap()
    }

    fn record(&self) -> crate::scan_checkpoint::SourceRecord {
        sonic_rs::from_slice(&std::fs::read(&self.records()[0]).unwrap()).unwrap()
    }

    fn records(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect()
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        std::fs::remove_dir_all(self.root()).unwrap();
    }
}

fn sparse(count: usize, hits: &[usize]) -> Vec<String> {
    (0..count)
        .map(|index| {
            let text = if hits.contains(&index) {
                format!("needle {index}")
            } else {
                format!("filler {index} {}", "x".repeat(500))
            };
            line(user(&format!("u{index}"), None, json!(text)))
        })
        .collect()
}

fn append(path: &Path, lines: &[String]) -> u64 {
    let text = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    text.len() as u64
}

fn line_bytes(path: &Path, indices: &[usize]) -> usize {
    let text = std::fs::read_to_string(path).unwrap();
    text.lines()
        .enumerate()
        .filter(|(index, _)| indices.contains(index))
        .map(|(_, line)| line.len())
        .sum()
}

#[test]
fn warm_run_replays_hits_without_rereading_the_prefix() {
    let cache = Cache::new();
    let source = Source::new(&sparse(400, &[5, 200, 390]), true);
    let size = std::fs::metadata(&source.0).unwrap().len() as usize;
    let (cold, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let cold = cold.unwrap();
    assert_eq!(progress.source_bytes, size);
    assert_eq!(cache.records().len(), 1);
    let (warm, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(warm.unwrap(), cold);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(
        progress.source_bytes,
        64 + line_bytes(&source.0, &[5, 200, 390])
    );
    assert_eq!(progress.parsed_events, 3);
    assert_eq!(progress.validated_bytes, 0);
}

#[test]
fn appended_run_reads_only_the_fence_hits_and_new_bytes() {
    let cache = Cache::new();
    let source = Source::new(&sparse(400, &[5, 200]), true);
    checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    let before = std::fs::metadata(&source.0).unwrap().len() as usize;
    let appended = append(&source.0, &sparse(450, &[420])[400..]);
    let (warm, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(warm.unwrap(), fresh.unwrap());
    assert_eq!(
        progress.source_bytes as u64,
        64 + line_bytes(&source.0, &[5, 200]) as u64 + appended
    );
    assert_eq!(progress.parsed_events, 2 + 50);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(progress.validated_bytes, before);
}

#[test]
fn growth_after_an_equal_length_prefix_rewrite_matches_a_fresh_scan() {
    let cache = Cache::new();
    let source = Source::new(&sparse(100, &[]), true);
    let (cold, _, _) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(cold.unwrap().counts, vec![0]);
    let text = std::fs::read_to_string(&source.0)
        .unwrap()
        .replace("filler 5 ", "needle 5 ");
    std::fs::write(&source.0, &text).unwrap();
    append(&source.0, &sparse(101, &[])[100..]);
    let (warm, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    let warm = warm.unwrap();
    assert_eq!(warm, fresh.unwrap());
    assert_eq!(warm.counts, vec![1]);
    assert!(warm.complete);
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(progress.cache_invalidations, 1);
    assert_eq!(progress.validated_bytes, text.len());
    assert_eq!(
        progress.source_bytes as u64,
        64 + std::fs::metadata(&source.0).unwrap().len()
    );
}

const SEGMENT: usize = 16 * 1024;

#[test]
fn appends_keep_fixed_size_prefix_segments() {
    let cache = Cache::segmented(SEGMENT as u64);
    let source = Source::new(&sparse(100, &[5]), true);
    let warm = || {
        checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        )
    };
    warm().0.unwrap();
    for round in 0..5 {
        let before = std::fs::metadata(&source.0).unwrap().len() as usize;
        append(
            &source.0,
            &sparse(110 + round * 10, &[])[100 + round * 10..],
        );
        let (run, _, progress) = warm();
        assert_eq!(run.unwrap().counts, vec![1]);
        assert_eq!(progress.cache_hits, 1);
        assert_eq!(progress.validated_bytes, before);
        let size = std::fs::metadata(&source.0).unwrap().len() as usize;
        let prefix = cache.record().file.prefix;
        assert_eq!(prefix.len(), size.div_ceil(SEGMENT));
        assert_eq!(prefix.last().unwrap().0 as usize, size);
        assert!(prefix[..prefix.len() - 1]
            .iter()
            .enumerate()
            .all(|(index, (end, _))| *end as usize == (index + 1) * SEGMENT));
    }
}

#[test]
fn growth_validation_stops_at_the_rewritten_segment() {
    for target in ["filler 40 ", "filler 148 "] {
        let cache = Cache::segmented(SEGMENT as u64);
        let source = Source::new(&sparse(150, &[]), true);
        checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        )
        .0
        .unwrap();
        let text = std::fs::read_to_string(&source.0).unwrap();
        let at = text.find(target).unwrap();
        std::fs::write(
            &source.0,
            text.replace(target, &target.replace("filler", "needle")),
        )
        .unwrap();
        append(&source.0, &sparse(151, &[])[150..]);
        let (run, _, progress) = checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        );
        let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
        let run = run.unwrap();
        assert_eq!(run, fresh.unwrap(), "{target}");
        assert_eq!(run.counts, vec![1], "{target}");
        assert_eq!(progress.cache_hits, 0, "{target}");
        assert_eq!(progress.cache_invalidations, 1, "{target}");
        assert_eq!(
            progress.validated_bytes,
            ((at / SEGMENT + 1) * SEGMENT).min(text.len()),
            "{target}"
        );
    }
}

#[test]
fn a_prefix_proof_for_a_large_transcript_fits_in_one_record() {
    let segments: Vec<(u64, u64)> = (1..=16_384)
        .map(|index| (index * PREFIX_SEGMENT, u64::MAX))
        .collect();
    assert!(sonic_rs::to_vec(&segments).unwrap().len() * 4 < MAX_RECORD_BYTES);
}

#[test]
fn growth_past_the_validation_cap_rescans_without_the_record() {
    let cache = Cache::new();
    let source = Source::new(&sparse(100, &[5]), true);
    let config = json!({"max_scan_validate_bytes": 1000});
    let capped = || {
        configured(
            &config,
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        )
    };
    capped().0.unwrap();
    append(&source.0, &sparse(101, &[100])[100..]);
    let size = std::fs::metadata(&source.0).unwrap().len() as usize;
    let (run, _, progress) = capped();
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    let run = run.unwrap();
    assert_eq!(run, fresh.unwrap());
    assert_eq!(run.counts, vec![2]);
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(progress.cache_invalidations, 0);
    assert_eq!(progress.validated_bytes, 0);
    assert_eq!(progress.source_bytes, 64 + size);
    let (again, _, progress) = capped();
    assert_eq!(again.unwrap(), run);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(progress.validated_bytes, 0);
}

fn racing(
    source: &Source,
    config: &Value,
    checkpoints: Option<&GrepCheckpoints>,
    rewrite: Option<u64>,
) -> (
    Result<Option<ScanControl>, SnapshotError>,
    Vec<usize>,
    ScanProgress,
) {
    let store = NativeStore::new(config).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let mut grep = reducer(&[("needle", None)], options(), &mut budget);
    let mut raced = false;
    let result = grep.scan_stream(
        &source.0,
        true,
        checkpoints,
        &mut budget,
        &Cancellation::default(),
        |_, _, _| {
            if !raced {
                if let Some(offset) = rewrite {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&source.0)
                        .unwrap()
                        .write_all_at(b"needle 5 ", offset)
                        .unwrap();
                }
                append(&source.0, &sparse(101, &[])[100..]);
                raced = true;
            }
            Ok(())
        },
    );
    assert!(raced);
    (result, grep.counts().to_vec(), budget.progress.clone())
}

#[test]
fn a_rewrite_racing_a_scan_fails_that_scan() {
    let cache = Cache::new();
    let source = Source::new(&sparse(100, &[50]), true);
    let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
    let rewrite = std::fs::read_to_string(&source.0)
        .unwrap()
        .find("filler 5 ")
        .unwrap() as u64;
    let (result, _, progress) = racing(&source, &json!({}), Some(&cache.1), Some(rewrite));
    assert_eq!(result.err().unwrap().status, Status::Changed);
    assert_eq!(progress.validated_bytes, pinned);
    let (warm, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    let warm = warm.unwrap();
    assert_eq!(warm, fresh.unwrap());
    assert_eq!(warm.counts, vec![2]);
    assert_eq!(progress.cache_invalidations, 1);
}

#[test]
fn an_append_racing_a_scan_revalidates_its_prefix_in_that_scan() {
    let source = Source::new(&sparse(100, &[50]), true);
    let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
    let (result, counts, progress) = racing(&source, &json!({}), None, None);
    result.unwrap();
    assert_eq!(counts, vec![1]);
    assert_eq!(progress.source_bytes, pinned);
    assert_eq!(progress.validated_bytes, pinned);
}

fn rewrite_between_validation_blocks(source: &Source, replaced: bool) {
    let path = source.0.clone();
    let at = std::fs::read_to_string(&path)
        .unwrap()
        .find("filler 5 ")
        .unwrap() as u64;
    let mut fired = false;
    VALIDATE_HOOKS.lock().unwrap().insert(
        source.0.clone(),
        Box::new(move |validated| {
            if fired || validated < 2 * SEGMENT as u64 {
                return;
            }
            fired = true;
            if replaced {
                let staged = path.with_extension("replaced");
                std::fs::write(
                    &staged,
                    std::fs::read_to_string(&path)
                        .unwrap()
                        .replace("filler 5 ", "needle 5 "),
                )
                .unwrap();
                std::fs::rename(&staged, &path).unwrap();
            } else {
                let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                file.write_all_at(b"needle 5 ", at).unwrap();
                file.set_modified(std::time::UNIX_EPOCH).unwrap();
            }
        }),
    );
}

#[test]
fn a_rewrite_during_growth_validation_fails_that_scan() {
    for replaced in [false, true] {
        let cache = Cache::segmented(SEGMENT as u64);
        let source = Source::new(&sparse(100, &[50]), true);
        let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
        rewrite_between_validation_blocks(&source, replaced);
        let (result, _, progress) = racing(&source, &json!({}), Some(&cache.1), None);
        VALIDATE_HOOKS.lock().unwrap().remove(&source.0);
        assert_eq!(result.err().unwrap().status, Status::Changed, "{replaced}");
        assert_eq!(progress.validated_bytes, pinned, "{replaced}");
        let (next, _, _) = checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        );
        let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
        let next = next.unwrap();
        assert_eq!(next, fresh.unwrap(), "{replaced}");
        assert_eq!(next.counts, vec![2], "{replaced}");
    }
}

#[test]
fn a_resumed_unterminated_tail_rewritten_as_it_grows_fails_that_scan() {
    let cache = Cache::new();
    let source = Source::new(&sparse(100, &[99]), false);
    let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
    let tail = std::fs::read_to_string(&source.0)
        .unwrap()
        .find("needle 99")
        .unwrap() as u64;
    let (cold, _, _) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(cold.unwrap().counts, vec![1]);
    assert!(cache.record().file.committed < pinned as u64);
    let (result, _, progress) = racing(&source, &json!({}), Some(&cache.1), Some(tail));
    assert_eq!(result.err().unwrap().status, Status::Changed);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(progress.validated_bytes, pinned);
}

#[test]
fn a_racing_append_past_the_validation_cap_is_incomplete() {
    let capped = json!({"max_scan_validate_bytes": 1000});
    let open = json!({});
    for (config, hits) in [(&capped, 0), (&open, 1)] {
        let cache = Cache::new();
        let source = Source::new(&sparse(100, &[50]), true);
        let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
        let (result, _, progress) = racing(&source, &capped, Some(&cache.1), None);
        assert_eq!(result.err().unwrap().status, Status::Incomplete, "{hits}");
        assert_eq!(progress.validated_bytes, 0, "{hits}");
        let (next, _, progress) = configured(
            config,
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        );
        let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
        assert_eq!(next.unwrap(), fresh.unwrap(), "{hits}");
        assert_eq!(progress.cache_hits, hits, "{hits}");
        assert_eq!(progress.validated_bytes, hits * pinned, "{hits}");
    }
}

#[test]
fn a_rewrite_racing_an_indexed_query_fails_that_query() {
    let cache = Cache::indexing(0);
    let source = Source::new(&sparse(100, &[50]), true);
    build(&source, &cache);
    let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
    let rewrite = std::fs::read_to_string(&source.0)
        .unwrap()
        .find("filler 5 ")
        .unwrap() as u64;
    let (result, _, progress) = racing(&source, &json!({}), Some(&cache.1), Some(rewrite));
    assert_eq!(result.err().unwrap().status, Status::Changed);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(progress.validated_bytes, pinned);
}

#[test]
fn a_rewrite_during_growth_validation_fails_an_indexed_query() {
    for replaced in [false, true] {
        let cache = Cache::with(0, SEGMENT as u64);
        let source = Source::new(&sparse(100, &[50]), true);
        build(&source, &cache);
        let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
        rewrite_between_validation_blocks(&source, replaced);
        let (result, _, progress) = racing(&source, &json!({}), Some(&cache.1), None);
        VALIDATE_HOOKS.lock().unwrap().remove(&source.0);
        assert_eq!(result.err().unwrap().status, Status::Changed, "{replaced}");
        assert_eq!(progress.cache_hits, 1, "{replaced}");
        assert_eq!(progress.validated_bytes, pinned, "{replaced}");
        let (next, _, _) = checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        );
        let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
        let next = next.unwrap();
        assert_eq!(next, fresh.unwrap(), "{replaced}");
        assert_eq!(next.counts, vec![2], "{replaced}");
    }
}

#[test]
fn an_index_stays_closed_when_its_prefix_proof_sees_a_rewrite() {
    for replaced in [false, true] {
        let cache = Cache::with(0, SEGMENT as u64);
        let source = Source::new(&sparse(100, &[50]), true);
        build(&source, &cache);
        let pinned = std::fs::metadata(&source.0).unwrap().len() as usize;
        append(&source.0, &sparse(101, &[])[100..]);
        rewrite_between_validation_blocks(&source, replaced);
        let (result, _, progress) = checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        );
        VALIDATE_HOOKS.lock().unwrap().remove(&source.0);
        assert_eq!(result.err().unwrap().status, Status::Changed, "{replaced}");
        assert_eq!(progress.cache_hits, 0, "{replaced}");
        assert_eq!(progress.validated_bytes, pinned, "{replaced}");
        let (next, _, _) = checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            limits(),
            Some(&cache.1),
        );
        let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
        let next = next.unwrap();
        assert_eq!(next, fresh.unwrap(), "{replaced}");
        assert_eq!(next.counts, vec![2], "{replaced}");
    }
}

#[test]
fn quota_checkpoint_answers_from_the_recorded_hits() {
    let cache = Cache::new();
    let source = Source::new(&sparse(2000, &[3, 4, 1500]), true);
    let (cold, _, _) = checkpointed(
        &source.0,
        &[("needle", Some(2))],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let cold = cold.unwrap();
    assert_eq!(cold.stop, Some(false));
    let (warm, _, progress) = checkpointed(
        &source.0,
        &[("needle", Some(2))],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(warm.unwrap(), cold);
    assert_eq!(
        progress.source_bytes,
        64 + line_bytes(&source.0, &[3, 4, 5])
    );
}

#[test]
fn budget_capped_runs_advance_through_partial_checkpoints() {
    let cache = Cache::new();
    let lines = sparse(1000, &[10, 400, 800]);
    let source = Source::new(&lines, true);
    let size = std::fs::metadata(&source.0).unwrap().len() as usize;
    let mut bound = limits();
    bound.max_source_read_bytes = 200_000;
    let mut runs = 0;
    let finished = loop {
        runs += 1;
        let (run, _, progress) = checkpointed(
            &source.0,
            &[("needle", None)],
            options(),
            true,
            bound,
            Some(&cache.1),
        );
        assert!(progress.source_bytes <= 200_000);
        match run {
            Ok(run) => break run,
            Err(error) => assert_eq!(error.reason, "source_read_limit"),
        }
    };
    assert!(runs > size / 200_000);
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(finished, fresh.unwrap());
}

#[test]
fn rewritten_or_corrupt_records_rescan_from_the_start() {
    let cache = Cache::new();
    let source = Source::new(&sparse(100, &[5]), true);
    checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    let text = std::fs::read_to_string(&source.0)
        .unwrap()
        .replace("filler 99 ", "filler 98 ");
    std::fs::write(&source.0, &text).unwrap();
    let (rewritten, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(progress.cache_invalidations, 1);
    assert_eq!(progress.source_bytes, text.len());
    assert_eq!(rewritten.unwrap().counts, vec![1]);
    let grown = text.replace("needle 5", "thread 5");
    std::fs::write(&source.0, &grown).unwrap();
    append(&source.0, &sparse(101, &[])[100..]);
    let (edited, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(edited.unwrap().counts, vec![0]);
    for record in cache.records() {
        std::fs::write(record, b"{not json").unwrap();
    }
    let (corrupt, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(
        progress.source_bytes as u64,
        std::fs::metadata(&source.0).unwrap().len()
    );
    assert_eq!(corrupt.unwrap().counts, vec![0]);
    let (other, _, progress) = checkpointed(
        &source.0,
        &[("filler 7 ", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(other.unwrap().counts, vec![1]);
}

#[test]
fn checkpoint_before_provider_detection_still_sniffs_appended_lines() {
    let cache = Cache::new();
    let source = Source::raw(b"\n\n");
    checkpointed(
        &source.0,
        &[("x", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    append(
        &source.0,
        &[line(json!({"type":"session_meta","payload":{"id":"x"}}))],
    );
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let mut grep = reducer(&[("x", None)], options(), &mut budget);
    let control = grep
        .scan_stream(
            &source.0,
            true,
            Some(&cache.1),
            &mut budget,
            &Cancellation::default(),
            |_, _, _| panic!("codex event streamed"),
        )
        .unwrap();
    assert!(control.is_none());
}

#[test]
fn restored_pending_hits_emit_before_the_next_read() {
    let cache = Cache::new();
    let source = Source::new(&sparse(10, &[2]), true);
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let mut grep = reducer(&[("needle", None)], options(), &mut budget);
    let error = grep
        .scan_stream(
            &source.0,
            true,
            Some(&cache.1),
            &mut budget,
            &Cancellation::default(),
            |_, _, _| Err(SnapshotError::new(Status::OutputLimit, "writer failed")),
        )
        .err()
        .unwrap();
    assert_eq!(error.status, Status::OutputLimit);
    let mut bound = limits();
    bound.max_events = 2;
    let (run, emitted, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        bound,
        Some(&cache.1),
    );
    assert_eq!(run.unwrap_err().status, Status::Incomplete);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(
        emitted.iter().map(|event| event.index).collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn same_size_rewrite_fails_verification() {
    let source = Source::new(&["aaaa".into(), "bbbb".into()], true);
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let cancel = Cancellation::default();
    let stream = SourceStream::open(&source.0, 0, &mut budget, &cancel).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&source.0)
        .unwrap()
        .write_all(b"cccc")
        .unwrap();
    assert_eq!(stream.verify().unwrap_err().status, Status::Changed);
}

#[test]
fn checkpoint_reads_are_charged_and_bounded() {
    let cache = Cache::new();
    let source = Source::new(&sparse(10, &[2]), true);
    checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    for record in cache.records() {
        std::fs::write(record, vec![b' '; 10_000]).unwrap();
    }
    let (_, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 0);
    assert!(progress.projection_bytes >= 10_000);
    for record in cache.records() {
        std::fs::write(record, vec![b' '; 30_000]).unwrap();
    }
    let mut bound = limits();
    bound.max_read_bytes = 20_000;
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        bound,
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 0);
    assert!(progress.projection_bytes < 20_000);
    assert_eq!(run.unwrap().counts, vec![1]);
}

#[test]
fn large_integer_options_bind_a_checkpoint_key() {
    let cache = Cache::new();
    let source = Source::new(&sparse(3, &[1]), true);
    let mut wide = options();
    wide.context = 1 << 53;
    let (run, _, _) = checkpointed(
        &source.0,
        &[("needle", Some(1 << 60))],
        wide,
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().emitted.len(), 3);
}

#[test]
fn queries_share_one_file_record_and_resume_independently() {
    let cache = Cache::new();
    let mut lines = sparse(600, &[50, 500]);
    lines[300] = line(assistant(
        "a",
        "u",
        json!([tool("t9", "Bash", json!({"command":"ls"}))]),
    ));
    lines[310] = line(result("r", "a", "t9", "needle result"));
    let source = Source::new(&lines, true);
    let mut capped = limits();
    capped.max_source_read_bytes = 120_000;
    let (partial, _, _) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        capped,
        Some(&cache.1),
    );
    assert_eq!(partial.err().unwrap().reason, "source_read_limit");
    let (other, _, progress) = checkpointed(
        &source.0,
        &[("filler 7 ", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(other.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(cache.records().len(), 1);
    let (resumed, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(progress.cache_hits, 1);
    assert!(progress.source_bytes < std::fs::metadata(&source.0).unwrap().len() as usize);
    assert_eq!(resumed.unwrap(), fresh.unwrap());
    let (again, _, progress) = checkpointed(
        &source.0,
        &[("filler 7 ", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(again.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 1);
}

fn redefined() -> Vec<String> {
    vec![
        line(assistant("a0", "u", json!([tool("t", "Bash", json!({}))]))),
        line(result("u1", "a0", "t", "needle")),
        line(user("u2", Some("u1"), json!("filler"))),
        line(assistant("a1", "u2", json!([tool("t", "Read", json!({}))]))),
    ]
}

fn through(lines: &[String], count: usize) -> u64 {
    lines[..count]
        .iter()
        .map(|line| line.len() as u64 + 1)
        .sum()
}

fn tool_filter(name: &str) -> GrepOptions {
    let mut options = options();
    options.tool = Some(name.into());
    options
}

#[test]
fn early_stop_after_a_name_lookup_reports_the_resolved_prefix() {
    let lines = redefined();
    let source = Source::new(&lines, true);
    let (run, _, _) = streamed(
        &source.0,
        &[("needle", Some(1))],
        tool_filter("Bash"),
        true,
        limits(),
    );
    let run = run.unwrap();
    assert_eq!(run.counts, vec![1]);
    assert_eq!(run.stop, Some(false));
    assert_eq!(run.names_through, Some(through(&lines, 3)));
    assert_eq!(
        prepared(&source.0, &[("needle", Some(1))], tool_filter("Bash")).counts,
        vec![0]
    );
    let store = NativeStore::new(&json!({})).unwrap();
    let mut session = ScanSession::new(&store, limits(), Cancellation::default());
    let mut grep = reducer(
        &[("needle", Some(1))],
        tool_filter("Bash"),
        &mut session.budget,
    );
    let outcome = session.each_source(
        &ScanPlan {
            paths: vec![source.0.clone()],
            root: std::env::temp_dir(),
            project: None,
            contains: None,
            source_limit: None,
        },
        |session, path| {
            Ok(grep
                .scan_stream(
                    path,
                    true,
                    None,
                    &mut session.budget,
                    &Cancellation::default(),
                    |_, _, _| Ok(()),
                )?
                .unwrap())
        },
    );
    assert_eq!(
        outcome.reason,
        Some(format!(
            "result_limit; tool names resolved through byte {}",
            through(&lines, 3)
        ))
    );
}

#[test]
fn unique_ids_label_only_name_dependent_early_stops() {
    let lines = vec![
        line(user("u0", None, json!("start"))),
        line(assistant("a0", "u0", json!([tool("t", "Bash", json!({}))]))),
        line(result("u1", "a0", "t", "needle result")),
        line(user("u2", Some("u1"), json!("needle text"))),
        line(user("u3", Some("u2"), json!("needle text"))),
        line(user("u4", Some("u3"), json!("tail"))),
    ];
    let source = Source::new(&lines, true);
    let (compact, _, _) = streamed(&source.0, &[("needle", Some(1))], options(), true, limits());
    assert_eq!(compact.unwrap().names_through, Some(through(&lines, 4)));
    let (json_mode, _, _) = streamed(
        &source.0,
        &[("needle", Some(1))],
        options(),
        false,
        limits(),
    );
    assert_eq!(json_mode.unwrap().names_through, None);
    let (text_only, _, _) = streamed(
        &source.0,
        &[("needle text", Some(1))],
        options(),
        true,
        limits(),
    );
    let text_only = text_only.unwrap();
    assert_eq!(text_only.stop, Some(false));
    assert_eq!(text_only.names_through, None);
    let (to_eof, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(to_eof.unwrap().names_through, None);
}

#[test]
fn warm_names_layer_through_eof_makes_early_stops_exact() {
    for (lines, options) in [
        (redefined(), tool_filter("Bash")),
        (
            vec![
                line(assistant("a0", "u", json!([tool("t", "Bash", json!({}))]))),
                line(result("u1", "a0", "t", "needle")),
                line(user("u2", Some("u1"), json!("needle"))),
                line(user("u3", Some("u2"), json!("tail"))),
            ],
            self::options(),
        ),
    ] {
        let cache = Cache::new();
        let source = Source::new(&lines, true);
        checkpointed(
            &source.0,
            &[("absent", None)],
            self::options(),
            true,
            limits(),
            Some(&cache.1),
        )
        .0
        .unwrap();
        let (warm, _, _) = checkpointed(
            &source.0,
            &[("needle", Some(1))],
            GrepOptions {
                tool: options.tool.clone(),
                ..self::options()
            },
            true,
            limits(),
            Some(&cache.1),
        );
        let warm = warm.unwrap();
        assert_eq!(warm.names_through, None);
        assert_eq!(
            warm,
            prepared(
                &source.0,
                &[("needle", Some(1))],
                GrepOptions {
                    tool: options.tool,
                    ..self::options()
                }
            )
        );
    }
}

#[test]
fn a_used_name_contradicted_by_a_later_full_layer_is_incomplete() {
    let cache = Cache::new();
    let lines = redefined();
    let source = Source::new(&lines, true);
    let (first, _, _) = checkpointed(
        &source.0,
        &[("needle", Some(1))],
        tool_filter("Bash"),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(first.unwrap().names_through, Some(through(&lines, 3)));
    checkpointed(
        &source.0,
        &[("absent", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    let (again, _, _) = checkpointed(
        &source.0,
        &[("needle", Some(1))],
        tool_filter("Bash"),
        true,
        limits(),
        Some(&cache.1),
    );
    let error = again.err().unwrap();
    assert_eq!(error.status, Status::Incomplete);
    assert!(error.reason.contains("redefined"), "{}", error.reason);
    assert!(cache.records().is_empty());
}

fn extras() -> Vec<String> {
    vec![
        line(
            json!({"type":"system","subtype":"informational","content":"system needle","uuid":"s1","sessionId":"s","timestamp":"2026-01-01T00:00:02Z"}),
        ),
        line(
            json!({"type":"attachment","attachment":{"type":"hook_success","content":"hook needle"},"uuid":"h1","sessionId":"s","timestamp":"2026-01-01T00:00:02Z"}),
        ),
        line(
            json!({"type":"attachment","attachment":{"type":"task_reminder","content":[{"needle":true}]},"uuid":"h2","sessionId":"s","timestamp":"2026-01-01T00:00:02Z"}),
        ),
        line(assistant(
            "a9",
            "u7",
            json!([{"type":"thinking","thinking":"deep needle","signature":"x"},{"type":"text","text":"answer"},tool("t9","Grep",json!({"pattern":"needle"}))]),
        )),
        line(result("u9", "a9", "t9", "grep needle\nsecond line")),
        line(json!({"type":"permission-mode","permissionMode":"default","sessionId":"s"})),
    ]
}

fn bulk(count: usize, hits: &[usize]) -> Vec<String> {
    let pad = "x".repeat(1000);
    let mut lines = transcript();
    lines.extend((0..count).map(|index| {
        let text = match index {
            _ if hits.contains(&index) => format!("needle {index} {pad}"),
            100 => format!("\u{212A}elvin probe {pad}"),
            _ => format!("filler {index} {pad}"),
        };
        line(user(&format!("b{index}"), None, json!(text)))
    }));
    lines.extend(extras());
    lines.extend(transcript());
    lines
}

fn build(source: &Source, cache: &Cache) -> crate::scan_index::IndexLayer {
    checkpointed(
        &source.0,
        &[("zq-never", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    cache.record().file.index.unwrap()
}

fn index_files(cache: &Cache) -> PathBuf {
    std::fs::read_dir(&cache.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "idx"))
        .unwrap()
}

#[test]
fn projections_rebuild_every_haystack_mask() {
    let mut entries = Vec::new();
    for text in transcript().iter().chain(&extras()) {
        crate::parse::parse_line(text.as_bytes(), &mut entries, &|_| true).unwrap();
    }
    assert_eq!(entries.len(), 20);
    for entry in &entries {
        let projected = Projected::of(entry);
        let mut bytes = Vec::new();
        projected.encode(&mut bytes);
        assert_eq!(
            Projected::decode_all(&bytes).unwrap(),
            vec![projected.clone()]
        );
        for mask in 0..8 {
            let (text, thinking, tools) = (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
            let expected = crate::render::haystack(entry, text, thinking, tools);
            assert_eq!(projected.haystack(text, thinking, tools), expected);
            assert_eq!(
                projected
                    .haystack_bound(text, thinking, tools, usize::MAX)
                    .unwrap(),
                expected.len()
            );
        }
    }
}

#[test]
fn literal_needs_hold_for_every_matching_haystack() {
    let haystacks = [
        "ship lane ready",
        "READY-FOR-SHIP now",
        "the \u{212A}elvin scale",
        "multi\nline needle",
        "needle 900 x",
        "fooXYZbar",
        "",
    ];
    let patterns = [
        ("ship lane|READY-FOR-SHIP", false),
        ("kelvin", true),
        ("line\\nneedle", false),
        ("n.edl.\\s9", false),
        ("foo.*bar", false),
        ("(?i)ready-for", false),
        ("x*", false),
        ("[^\\s\\S]", false),
    ];
    for (pattern, ignore_case) in patterns {
        let regex = regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .unwrap();
        let needs = crate::scan_index::needs(pattern, ignore_case);
        for haystack in haystacks.iter().filter(|haystack| regex.is_match(haystack)) {
            let present: std::collections::HashSet<u32> = haystack
                .split('\n')
                .flat_map(|part| crate::scan_projection::grams(part.as_bytes()))
                .collect();
            for need in &needs {
                assert!(
                    need.as_ref().is_none_or(|literals| literals
                        .iter()
                        .any(|keys| keys.iter().all(|key| present.contains(key)))),
                    "{pattern} on {haystack:?}"
                );
            }
        }
    }
    assert!(crate::scan_index::needs("ship lane|READY-FOR-SHIP", false)
        .iter()
        .all(Option::is_some));
    assert!(crate::scan_index::needs("n.edl.\\s9", false)
        .iter()
        .all(Option::is_none));
    assert_eq!(
        crate::scan_index::needs("[^\\s\\S]", false),
        [Some(vec![]), Some(vec![])]
    );
}

#[test]
fn indexed_new_queries_match_the_prepared_snapshot_path() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(2400, &[3, 900, 1700, 2350]), true);
    let index = build(&source, &cache);
    assert_eq!(index.segments[0].blocks, 64);
    assert!(index.segments.len() >= 2);
    assert_eq!(index.committed, std::fs::metadata(&source.0).unwrap().len());
    let mut names: Vec<_> = [
        "plain",
        "quota",
        "multi",
        "context",
        "context-quota",
        "kinds",
        "tool-forward",
        "tool-bash",
        "ignore-case",
        "final",
        "absent",
    ]
    .map(case)
    .into_iter()
    .collect();
    names.push((vec![("kelvin", None)], {
        let mut options = options();
        options.ignore_case = true;
        options
    }));
    names.push((vec![("needle 1700", Some(1))], options()));
    for (patterns, options) in names {
        for render_names in [true, false] {
            let rebuilt = GrepOptions {
                kinds: options.kinds.clone(),
                tool: options.tool.clone(),
                ..options
            };
            let mut expected = prepared(&source.0, &patterns, rebuilt);
            let rebuilt = GrepOptions {
                kinds: options.kinds.clone(),
                tool: options.tool.clone(),
                ..options
            };
            let (actual, _, progress) = checkpointed(
                &source.0,
                &patterns,
                rebuilt,
                render_names,
                limits(),
                Some(&cache.1),
            );
            let mut actual = actual.unwrap();
            assert_eq!(progress.cache_hits, 1, "{patterns:?}");
            assert_eq!(actual.names_through, None);
            if !render_names {
                for emitted in expected.emitted.iter_mut().chain(&mut actual.emitted) {
                    emitted.names.clear();
                }
            }
            assert_eq!(
                actual, expected,
                "{patterns:?}, render names {render_names}"
            );
        }
    }
}

#[test]
fn warm_new_query_reads_candidate_blocks_and_hit_lines_only() {
    let cache = Cache::indexing(0);
    let lines = bulk(2400, &[900]);
    let source = Source::new(&lines, true);
    let index = build(&source, &cache);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 900", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(
        progress.source_bytes,
        64 + lines[transcript().len() + 900].len()
    );
    assert_eq!(progress.parsed_events, 1);
    assert!(
        (progress.projection_bytes as u64) < index.proj / 8,
        "{} of {}",
        progress.projection_bytes,
        index.proj
    );
    assert!(
        progress.examined_events < 200,
        "{}",
        progress.examined_events
    );
}

#[test]
fn appended_lines_extend_the_index_from_the_suffix() {
    let cache = Cache::indexing(0);
    let lines = bulk(2400, &[900]);
    let source = Source::new(&lines, true);
    build(&source, &cache);
    let before = std::fs::metadata(&source.0).unwrap().len();
    let appended = append(&source.0, &sparse(2450, &[2420])[2400..]);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 2420", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(progress.source_bytes as u64, 64 + appended);
    assert_eq!(progress.validated_bytes as u64, before);
    assert_eq!(progress.parsed_events, 50);
    let size = std::fs::metadata(&source.0).unwrap().len();
    assert_eq!(cache.record().file.index.unwrap().committed, size);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 2420|needle 900", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(
        &source.0,
        &[("needle 2420|needle 900", None)],
        options(),
        true,
        limits(),
    );
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!(progress.parsed_events, 2);
    assert_eq!(progress.validated_bytes, 0);
}

#[test]
fn an_index_serves_a_grown_file_only_after_the_prefix_proof() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(2400, &[900]), true);
    build(&source, &cache);
    let before = std::fs::metadata(&source.0).unwrap().len();
    let text = std::fs::read_to_string(&source.0)
        .unwrap()
        .replace("filler 50 ", "needle 50 ");
    std::fs::write(&source.0, &text).unwrap();
    append(&source.0, &sparse(2401, &[])[2400..]);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 50 ", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(
        &source.0,
        &[("needle 50 ", None)],
        options(),
        true,
        limits(),
    );
    let run = run.unwrap();
    assert_eq!(run, fresh.unwrap());
    assert_eq!(run.counts, vec![1]);
    assert!(run.complete);
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(progress.cache_invalidations, 1);
    assert_eq!(progress.validated_bytes as u64, before);
}

#[test]
fn growth_past_the_validation_cap_leaves_the_index_unopened() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(2400, &[900]), true);
    build(&source, &cache);
    append(&source.0, &sparse(2401, &[2400])[2400..]);
    let size = std::fs::metadata(&source.0).unwrap().len() as usize;
    let (run, _, progress) = configured(
        &json!({"max_scan_validate_bytes": 1000}),
        &source.0,
        &[("needle 2400|needle 900", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(
        &source.0,
        &[("needle 2400|needle 900", None)],
        options(),
        true,
        limits(),
    );
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(progress.validated_bytes, 0);
    assert_eq!(progress.source_bytes, 64 + size);
}

fn escaped() -> Vec<String> {
    vec![
        r#"{"type":"user","uuid":"e1","parentUuid":null,"sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"caf\u00e9 said \"hi\" at C:\\temp \u212Aelvin \u03a3\u038a\u03a3\u03a5\u03a6\u039f\u03a3 rolls"}}"#.to_owned(),
        line(assistant(
            "e2",
            "e1",
            json!([{"type":"thinking","thinking":"thought marker","signature":"x"},{"type":"text","text":"text marker"},tool("e3","Bash",json!({"command":"echo tool marker"}))]),
        )),
        line(result("e4", "e2", "e3", "result marker \\u00e9")),
    ]
}

#[test]
fn indexed_queries_match_fresh_scans_across_escapes_folds_and_masks() {
    let cache = Cache::indexing(0);
    let mut lines = bulk(300, &[30]);
    let tail = lines.split_off(transcript().len() + 150);
    lines.extend(escaped());
    lines.extend(tail);
    let source = Source::new(&lines, true);
    build(&source, &cache);
    let patterns = [
        ("café", false),
        ("caf\\x{e9} said \"hi\"", false),
        ("C:\\\\temp", false),
        ("\\\\u00e9", false),
        ("kelvin", true),
        ("σίσυφος", true),
        ("thought marker", false),
        ("text marker|tool marker", false),
        ("result marker", false),
    ];
    for mask in 1..8 {
        for (pattern, ignore_case) in patterns {
            let masked = || GrepOptions {
                where_text: mask & 1 != 0,
                where_thinking: mask & 2 != 0,
                where_tools: mask & 4 != 0,
                ignore_case,
                ..options()
            };
            let (run, _, progress) = checkpointed(
                &source.0,
                &[(pattern, None)],
                masked(),
                true,
                limits(),
                Some(&cache.1),
            );
            let (fresh, _, _) = streamed(&source.0, &[(pattern, None)], masked(), true, limits());
            assert_eq!(run.unwrap(), fresh.unwrap(), "{pattern} under mask {mask}");
            assert_eq!(progress.cache_hits, 1, "{pattern} under mask {mask}");
        }
    }
}

#[test]
fn literal_free_patterns_traverse_projections_not_the_source() {
    let cache = Cache::indexing(0);
    let lines = bulk(2400, &[900]);
    let source = Source::new(&lines, true);
    let index = build(&source, &cache);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("n.edl.\\s9", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(
        progress.source_bytes,
        64 + lines[transcript().len() + 900].len()
    );
    assert!(progress.projection_bytes as u64 >= index.proj);
}

#[test]
fn budget_capped_indexed_runs_resume_with_pending_context() {
    let cache = Cache::indexing(0);
    let lines = bulk(2400, &[3, 900, 901, 1700]);
    let source = Source::new(&lines, true);
    build(&source, &cache);
    let mut options = options();
    options.context = 2;
    let mut bound = limits();
    bound.max_read_bytes = 1_500_000;
    let mut runs = 0;
    let finished = loop {
        runs += 1;
        let rebuilt = GrepOptions {
            kinds: Vec::new(),
            tool: None,
            ..options
        };
        let (run, _, _) = checkpointed(
            &source.0,
            &[("n.edl.\\s", None)],
            rebuilt,
            true,
            bound,
            Some(&cache.1),
        );
        match run {
            Ok(run) => break run,
            Err(error) => assert!(
                matches!(error.status, Status::Incomplete | Status::OutputLimit),
                "{}",
                error.reason
            ),
        }
    };
    assert!(runs > 2);
    let (fresh, _, _) = streamed(&source.0, &[("n.edl.\\s", None)], options, true, limits());
    assert_eq!(finished, fresh.unwrap());
}

#[test]
fn damaged_index_files_are_discarded_and_rebuilt() {
    let cache = Cache::indexing(0);
    let lines = bulk(2400, &[900]);
    let source = Source::new(&lines, true);
    let index = build(&source, &cache);
    let dir = index_files(&cache);
    let proj = dir.join(format!("proj-{}", index.generation));
    let mut bytes = std::fs::read(&proj).unwrap();
    bytes[40] ^= 1;
    std::fs::write(&proj, &bytes).unwrap();
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("first needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let error = run.unwrap_err();
    assert_eq!(error.status, Status::Changed);
    assert_eq!(progress.cache_invalidations, 1);
    assert!(!dir.exists());
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("first needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(
        &source.0,
        &[("first needle", None)],
        options(),
        true,
        limits(),
    );
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!(progress.cache_hits, 0);
    let rebuilt = cache.record().file.index.unwrap();
    assert_ne!(rebuilt.generation, index.generation);
    let segment = dir.join(&rebuilt.segments[0].name);
    let mut bytes = std::fs::read(&segment).unwrap();
    bytes[20] ^= 1;
    std::fs::write(&segment, &bytes).unwrap();
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 900", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_invalidations, 1);
    assert_eq!(progress.cache_hits, 0);
}

#[test]
fn a_held_build_lock_leaves_the_index_to_its_holder() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(300, &[5]), true);
    let unindexed =
        GrepCheckpoints::new(cache.root().to_path_buf(), "test".into()).indexing_from(u64::MAX);
    checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&unindexed),
    )
    .0
    .unwrap();
    let key = cache.record().key;
    let held = cache.1.build_lock(&key).unwrap();
    let (run, _, _) = checkpointed(
        &source.0,
        &[("filler 7 ", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(cache.record().file.index, None);
    drop(held);
    build(&source, &cache);
}

#[test]
fn an_added_event_publishes_only_once_its_line_commits() {
    let cache = Cache::indexing(0);
    let dir = cache.0.join("builder.idx");
    std::fs::create_dir_all(&dir).unwrap();
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let cancel = Cancellation::default();
    let mut builder = crate::scan_index::Builder::start(&dir, None, u64::MAX, &mut budget, &cancel)
        .unwrap()
        .unwrap();
    let text = line(user("u0", None, json!("needle")));
    let mut entries = Vec::new();
    crate::parse::parse_line(text.as_bytes(), &mut entries, &|_| true).unwrap();
    let span = LineSpan {
        offset: 0,
        len: text.len(),
        terminated: true,
    };
    assert!(builder
        .add(span, 7, &entries[0], &mut budget, &cancel)
        .unwrap()
        .is_some());
    let uncommitted = builder.publish().unwrap();
    assert_eq!((uncommitted.events, uncommitted.committed), (0, 0));
    builder
        .add(span, 7, &entries[0], &mut budget, &cancel)
        .unwrap();
    builder
        .commit(span.end(), b"x", true, &mut budget, &cancel)
        .unwrap();
    let committed = builder.publish().unwrap();
    assert_eq!((committed.events, committed.committed), (1, span.end()));
}

#[test]
fn an_index_of_blank_lines_still_detects_the_provider() {
    let cache = Cache::indexing(0);
    let source = Source::raw(b"\n\n  \n");
    checkpointed(
        &source.0,
        &[("x", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    assert!(!cache.record().file.index.unwrap().sniffed);
    append(
        &source.0,
        &[
            line(json!({"type":"session_meta","payload":{"id":"x"}})),
            line(
                json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"needle"}]}}),
            ),
        ],
    );
    let store = NativeStore::new(&json!({})).unwrap();
    let mut budget = ScanBudget::new(&store, limits());
    let mut grep = reducer(&[("needle", None)], options(), &mut budget);
    let control = grep
        .scan_stream(
            &source.0,
            true,
            Some(&cache.1),
            &mut budget,
            &Cancellation::default(),
            |_, _, _| panic!("codex event streamed"),
        )
        .unwrap();
    assert!(control.is_none());
}

#[test]
fn pruned_events_still_mark_capped_patterns_incomplete() {
    let cache = Cache::indexing(0);
    let lines: Vec<String> = (0..512)
        .map(|index| {
            let text = if index == 255 { "needle" } else { "xxxx" };
            line(user(&format!("u{index}"), None, json!(text)))
        })
        .collect();
    let source = Source::new(&lines, true);
    let index = build(&source, &cache);
    assert_eq!(index.blocks.len(), 2);
    let patterns = [("needle", Some(1)), ("absent", None)];
    let expected = prepared(&source.0, &patterns, options());
    let (actual, _, progress) = checkpointed(
        &source.0,
        &patterns,
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(progress.cache_hits, 1);
    let actual = actual.unwrap();
    assert!(!actual.complete);
    assert_eq!(actual, expected);
}

#[test]
fn incoherent_index_geometry_is_damage_not_a_panic() {
    let cache = Cache::indexing(0);
    let lines = bulk(2400, &[900]);
    let source = Source::new(&lines, true);
    build(&source, &cache);
    let mut record = cache.record();
    record.file.index.as_mut().unwrap().segments[0].first = 1;
    std::fs::write(&cache.records()[0], sonic_rs::to_vec(&record).unwrap()).unwrap();
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 900", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(
        &source.0,
        &[("needle 900", None)],
        options(),
        true,
        limits(),
    );
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!(progress.cache_invalidations, 1);
    assert_eq!(progress.cache_hits, 0);
}

fn query_layer(key: &str, committed: u64) -> QueryLayer {
    QueryLayer {
        key: key.into(),
        indexed: false,
        committed,
        parsed: 1,
        decided: 1,
        emitted: 1,
        last_emitted: None,
        last_hit: None,
        stopped: None,
        reducer: ReducerState {
            counts: vec![0],
            matched_items: 0,
            coverage_complete: true,
        },
        referenced: Vec::new(),
        replay: Vec::new(),
        queue: Vec::new(),
    }
}

fn source_record(
    revision: &str,
    committed: u64,
    prefix: &[(u64, u64)],
    queries: &[(&str, u64)],
) -> SourceRecord {
    SourceRecord {
        key: "k".into(),
        size: committed + 10,
        revision: revision.into(),
        file: crate::scan_checkpoint::FileLayer {
            committed,
            fence: Vec::new(),
            sniffed: true,
            events: 1,
            names: Vec::new(),
            prefix: prefix.to_vec(),
            index: None,
        },
        queries: queries
            .iter()
            .map(|(key, committed)| query_layer(key, *committed))
            .collect(),
    }
}

fn query_keys(record: &SourceRecord) -> Vec<&str> {
    record
        .queries
        .iter()
        .map(|layer| layer.key.as_str())
        .collect()
}

#[test]
fn a_kept_newer_file_layer_keeps_its_own_size_and_revision() {
    let older = || source_record("r1", 90, &[(90, 1)], &[("q", 90)]);
    let newer = || source_record("r2", 150, &[(90, 1), (150, 2)], &[("q", 150)]);
    let merged = SourceRecord::merge(Some(newer()), true, older(), false).unwrap();
    assert_eq!((merged.size, merged.revision.as_str()), (160, "r2"));
    assert_eq!(merged.file.committed, 150);
    let merged = SourceRecord::merge(Some(older()), true, newer(), false).unwrap();
    assert_eq!((merged.size, merged.revision.as_str()), (160, "r2"));
    assert!(SourceRecord::merge(
        None,
        false,
        source_record("r1", 90, &[(150, 1)], &[("q", 90)]),
        false
    )
    .is_none());
}

#[test]
fn a_concurrent_record_shares_only_layers_inside_the_agreed_prefix() {
    let theirs = || {
        let mut record = source_record("r2", 150, &[(100, 7), (150, 8)], &[("a", 150), ("b", 100)]);
        record.queries.push(QueryLayer {
            indexed: true,
            ..query_layer("d", 0)
        });
        record
    };
    let merged = SourceRecord::merge(
        Some(theirs()),
        false,
        source_record("r3", 180, &[(100, 7), (180, 9)], &[("c", 180)]),
        false,
    )
    .unwrap();
    assert_eq!(merged.file.committed, 180);
    assert_eq!(query_keys(&merged), vec!["c", "b"]);
    let merged = SourceRecord::merge(
        Some(theirs()),
        false,
        source_record("r3", 180, &[(100, 7), (200, 9)], &[("c", 180)]),
        false,
    )
    .unwrap();
    assert_eq!(merged.file.committed, 150);
    assert_eq!(query_keys(&merged), vec!["a", "b", "d"]);
    let merged = SourceRecord::merge(
        Some(theirs()),
        false,
        source_record("r3", 180, &[(180, 5)], &[("c", 180)]),
        false,
    )
    .unwrap();
    assert_eq!(merged.file.prefix, vec![(180, 5)]);
    assert_eq!(query_keys(&merged), vec!["c"]);
    let merged = SourceRecord::merge(
        Some(theirs()),
        true,
        source_record("r3", 180, &[(100, 7), (150, 8), (180, 9)], &[("a", 180)]),
        false,
    )
    .unwrap();
    assert_eq!(query_keys(&merged), vec!["a", "b", "d"]);
    assert_eq!(merged.queries[0].committed, 180);
}

#[test]
fn orphaned_index_directories_are_reclaimed_after_a_publish() {
    let cache = Cache::indexing(0);
    let orphan = cache.0.join("0000.idx");
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(&orphan)
        .unwrap();
    std::fs::write(orphan.join("proj-x"), b"stale").unwrap();
    let source = Source::new(&sparse(20, &[3]), true);
    build(&source, &cache);
    assert!(!orphan.exists());
    assert!(index_files(&cache).exists());
}

fn seeded_record(source: &Source, cache: &Cache) -> (String, Vec<u8>) {
    checkpointed(
        &source.0,
        &[("zq-never", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    (
        cache.record().key,
        std::fs::read(&cache.records()[0]).unwrap(),
    )
}

fn stripe_mirror(key: &str) -> usize {
    let digest = Sha256::digest(key.as_bytes());
    (usize::from(digest[0]) << 8 | usize::from(digest[1])) % 1024
}

fn stripe_name(kind: &str, key: &str) -> String {
    format!("{kind}-{:03x}.lock", stripe_mirror(key))
}

fn stripe_file(cache: &Cache, kind: &str, key: &str) -> PathBuf {
    cache.0.join("locks").join(stripe_name(kind, key))
}

fn unlocked(path: &Path) -> std::fs::File {
    crate::scan_index::open_private(path, true).unwrap()
}

fn lock(file: &std::fs::File) {
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
}

fn hold(path: &Path) -> std::fs::File {
    let held = unlocked(path);
    lock(&held);
    held
}

fn hold_record_lock(cache: &Cache, key: &str) -> std::fs::File {
    hold(&stripe_file(cache, "record", key))
}

fn scanned_while_locked(
    held: std::fs::File,
    source: &Source,
    cache: &Cache,
    patterns: &[(&str, Option<usize>)],
    after_event: Interrupt,
) -> (Result<Run, SnapshotError>, Vec<Emitted>) {
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let store = NativeStore::new(&json!({})).unwrap();
            let mut budget = ScanBudget::new(&store, limits());
            let mut grep = reducer(patterns, options(), &mut budget);
            let mut emitted = Vec::new();
            let control = grep.scan_stream(
                &source.0,
                true,
                Some(&cache.1),
                &mut budget,
                &Cancellation::default(),
                |event, budget, cancel| {
                    let _staging = event.preflight_render(budget, cancel)?;
                    emitted.push(Emitted::of(&event));
                    after_event(budget, cancel);
                    Ok(())
                },
            );
            let run = run_of(control, &grep, &mut emitted);
            sender.send((run, emitted)).unwrap();
        });
        let outcome = receiver.recv_timeout(LOCKED_SCAN_WAIT);
        drop(held);
        outcome.expect("the scan returned while the record lock was held")
    })
}

#[test]
fn a_held_record_lock_skips_publication_and_keeps_the_result() {
    let cache = Cache::new();
    let source = Source::new(&sparse(20, &[3, 9]), true);
    let (key, before) = seeded_record(&source, &cache);
    let fresh = streamed(&source.0, &[("needle", None)], options(), true, limits())
        .0
        .unwrap();
    let held = hold_record_lock(&cache, &key);
    let (run, _) = scanned_while_locked(held, &source, &cache, &[("needle", None)], |_, _| {});
    assert_eq!(run.unwrap(), fresh);
    assert_eq!(std::fs::read(&cache.records()[0]).unwrap(), before);
    let (after, _, _) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(after.unwrap(), fresh);
    assert_eq!(cache.record().queries.len(), 2);
}

#[test]
fn an_interrupted_partial_scan_returns_without_the_record_lock() {
    let cache = Cache::new();
    let source = Source::new(&sparse(20, &[3, 9]), true);
    let (key, before) = seeded_record(&source, &cache);
    let interrupts: [(Interrupt, Status); 2] = [
        (|_, cancel| cancel.cancel(), Status::Cancelled),
        (
            |budget, _| budget.limits.deadline_unix_ms = 0,
            Status::Deadline,
        ),
    ];
    for (interrupt, status) in interrupts {
        let held = hold_record_lock(&cache, &key);
        let (run, emitted) =
            scanned_while_locked(held, &source, &cache, &[("needle", None)], interrupt);
        assert_eq!(run.unwrap_err().status, status);
        assert_eq!(emitted.len(), 1, "{status:?}");
        assert_eq!(
            std::fs::read(&cache.records()[0]).unwrap(),
            before,
            "{status:?}"
        );
    }
    let fresh = streamed(&source.0, &[("needle", None)], options(), true, limits())
        .0
        .unwrap();
    let (after, _, _) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(after.unwrap(), fresh);
    assert_eq!(cache.record().queries.len(), 2);
}

fn colliding(key: &str) -> String {
    (0..)
        .map(|attempt| format!("{key}-{attempt}"))
        .find(|candidate| stripe_mirror(candidate) == stripe_mirror(key))
        .unwrap()
}

fn apart(key: &str, name: &str) -> String {
    (0..)
        .map(|attempt| format!("{name}-{attempt}"))
        .find(|candidate| stripe_mirror(candidate) != stripe_mirror(key))
        .unwrap()
}

fn keyed_record(key: &str) -> SourceRecord {
    SourceRecord {
        key: key.into(),
        ..source_record("r1", 90, &[(90, 1)], &[("q", 90)])
    }
}

fn keyed(cache: &Cache, key: &str) -> SourceRecord {
    sonic_rs::from_slice(&std::fs::read(cache.0.join(format!("{key}.json"))).unwrap()).unwrap()
}

fn saved(cache: &Cache, key: &str) -> (bool, usize) {
    let saved = cache
        .1
        .save(keyed_record(key), false, None, MAX_RECORD_BYTES)
        .unwrap();
    (saved.published, saved.read)
}

fn age(cache: &Cache, key: &str) {
    std::fs::File::options()
        .write(true)
        .open(cache.0.join(format!("{key}.json")))
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
}

fn fill(cache: &Cache) {
    for index in 0..MAX_RECORDS {
        std::fs::write(cache.0.join(format!("filler-{index}.json")), b"{}").unwrap();
    }
}

fn lock_files(cache: &Cache) -> Vec<(String, u64)> {
    let mut files: Vec<(String, u64)> = std::fs::read_dir(cache.0.join("locks"))
        .unwrap()
        .map(|entry| entry.unwrap())
        .map(|entry| {
            (
                entry.file_name().into_string().unwrap(),
                entry.metadata().unwrap().ino(),
            )
        })
        .collect();
    files.sort();
    files
}

fn assert_referenced(cache: &Cache, key: &str, layer: &crate::scan_index::IndexLayer) {
    let dir = cache.0.join(format!("{key}.idx"));
    let len = |name: String| std::fs::metadata(dir.join(name)).unwrap().len();
    assert!(len(format!("proj-{}", layer.generation)) >= layer.proj);
    assert!(len(format!("events-{}", layer.generation)) >= layer.events as u64 * 20);
    for segment in &layer.segments {
        assert!(len(segment.name.clone()) >= segment.len, "{}", segment.name);
    }
}

#[test]
fn colliding_keys_share_one_stripe_file() {
    let cache = Cache::new();
    let first = "collide".to_owned();
    let second = colliding(&first);
    assert_ne!(first, second);
    assert_eq!(saved(&cache, &first), (true, 0));
    let held = hold_record_lock(&cache, &first);
    assert_eq!(saved(&cache, &second), (false, 0));
    assert!(!cache.0.join(format!("{second}.json")).exists());
    drop(held);
    let building = cache.1.build_lock(&first).unwrap();
    assert!(cache.1.build_lock(&second).is_none());
    assert!(!cache.0.join(format!("{second}.idx")).exists());
    drop(building);
    assert!(cache.1.build_lock(&second).is_some());
    assert_eq!(saved(&cache, &second), (true, 0));
    assert_eq!(keyed(&cache, &second).key, second);
    assert_eq!(
        lock_files(&cache)
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec![stripe_name("build", &first), stripe_name("record", &first)]
    );
}

#[test]
fn lock_files_are_stable_bounded_and_never_unlinked() {
    assert_eq!(LOCK_STRIPES, 1024);
    let cache = Cache::new();
    let keys: Vec<String> = (0..300).map(|index| format!("key-{index}")).collect();
    for key in &keys {
        assert_eq!(saved(&cache, key), (true, 0));
    }
    assert_eq!(cache.records().len(), MAX_RECORDS);
    let evicted: Vec<&String> = keys
        .iter()
        .filter(|key| !cache.0.join(format!("{key}.json")).exists())
        .collect();
    assert_eq!(evicted.len(), keys.len() - MAX_RECORDS);
    let expected: BTreeSet<String> = keys
        .iter()
        .map(|key| stripe_name("record", key))
        .chain(evicted.iter().map(|key| stripe_name("build", key)))
        .collect();
    let before = lock_files(&cache);
    assert_eq!(
        before
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert!(before.len() <= 2 * LOCK_STRIPES);
    assert!(std::fs::read_dir(&cache.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .all(|path| path.extension().is_none_or(|extension| extension != "lock")));
    for index in 300..350 {
        assert_eq!(saved(&cache, &format!("key-{index}")), (true, 0));
    }
    let after = lock_files(&cache);
    assert!(before.iter().all(|file| after.contains(file)));
    assert!(after.len() <= 2 * LOCK_STRIPES);
}

#[test]
fn eviction_skips_a_victim_whose_stripe_is_held() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(300, &[5]), true);
    let layer = build(&source, &cache);
    let key = cache.record().key;
    let record = cache.0.join(format!("{key}.json"));
    age(&cache, &key);
    let before = std::fs::read(&record).unwrap();
    fill(&cache);
    for kind in ["record", "build"] {
        let held = hold(&stripe_file(&cache, kind, &key));
        assert_eq!(saved(&cache, &apart(&key, kind)), (true, 0));
        assert_eq!(std::fs::read(&record).unwrap(), before, "{kind}");
        assert_referenced(&cache, &key, &layer);
        drop(held);
    }
    assert_eq!(saved(&cache, "trigger-free"), (true, 0));
    assert!(!record.exists());
    assert!(!cache.0.join(format!("{key}.idx")).exists());
    assert!(stripe_file(&cache, "build", &key).exists());
    assert!(stripe_file(&cache, "record", &key).exists());
}

#[test]
fn a_descriptor_opened_before_eviction_still_excludes_later_builders() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(300, &[5]), true);
    build(&source, &cache);
    let key = cache.record().key;
    let path = stripe_file(&cache, "build", &key);
    let paused = unlocked(&path);
    age(&cache, &key);
    fill(&cache);
    assert_eq!(saved(&cache, "trigger"), (true, 0));
    assert!(!cache.0.join(format!("{key}.json")).exists());
    assert!(!cache.0.join(format!("{key}.idx")).exists());
    assert_eq!(
        std::fs::metadata(&path).unwrap().ino(),
        paused.metadata().unwrap().ino()
    );
    lock(&paused);
    assert!(cache.1.build_lock(&key).is_none());
    let fresh = streamed(&source.0, &[("needle", None)], options(), true, limits())
        .0
        .unwrap();
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap(), fresh);
    assert_eq!(progress.cache_hits, 0);
    assert_eq!(keyed(&cache, &key).file.index, None);
    assert!(!cache.0.join(format!("{key}.idx")).exists());
    drop(paused);
    checkpointed(
        &source.0,
        &[("zq-never", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    )
    .0
    .unwrap();
    let layer = keyed(&cache, &key).file.index.unwrap();
    assert_referenced(&cache, &key, &layer);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 5", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle 5", None)], options(), true, limits());
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!(progress.cache_hits, 1);
}

#[test]
fn a_held_publication_stripe_blocks_publication_and_retirement() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(2400, &[900]), true);
    let layer = build(&source, &cache);
    let key = cache.record().key;
    let record = cache.0.join(format!("{key}.json"));
    let before = std::fs::read(&record).unwrap();
    let path = stripe_file(&cache, "record", &key);
    let paused = unlocked(&path);
    assert_eq!(saved(&cache, "trigger"), (true, 0));
    assert_eq!(
        std::fs::metadata(&path).unwrap().ino(),
        paused.metadata().unwrap().ino()
    );
    lock(&paused);
    append(&source.0, &sparse(2450, &[2420])[2400..]);
    let (run, _) =
        scanned_while_locked(paused, &source, &cache, &[("needle 2420", None)], |_, _| {});
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(std::fs::read(&record).unwrap(), before);
    assert_referenced(&cache, &key, &layer);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 2420", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 1);
    let extended = keyed(&cache, &key).file.index.unwrap();
    assert_eq!(
        extended.committed,
        std::fs::metadata(&source.0).unwrap().len()
    );
    assert_ne!(extended, layer);
    assert_referenced(&cache, &key, &extended);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!((progress.cache_hits, progress.cache_invalidations), (1, 0));
}

#[test]
fn a_builder_reclaims_an_orphan_on_its_own_build_stripe() {
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(2400, &[900]), true);
    build(&source, &cache);
    let key = cache.record().key;
    let orphan = cache.0.join(format!("{}.idx", colliding(&key)));
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
        .create(&orphan)
        .unwrap();
    std::fs::write(orphan.join("proj-x"), b"stale").unwrap();
    append(&source.0, &sparse(2450, &[2420])[2400..]);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 2420", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 1);
    assert!(!orphan.exists());
    let layer = cache.record().file.index.unwrap();
    assert_eq!(layer.committed, std::fs::metadata(&source.0).unwrap().len());
    assert_referenced(&cache, &key, &layer);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!(progress.cache_hits, 1);
}

fn legacy_victims(root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json" || extension == "idx")
        })
        .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_stripe_protocol_shares_no_entry_with_a_legacy_cache() {
    assert!(!PROTOCOL_DIR.contains('.'));
    let cache = Cache::indexing(0);
    let source = Source::new(&bulk(2400, &[900]), true);
    let layer = build(&source, &cache);
    let key = cache.record().key;
    let root = cache.root();
    let legacy: Vec<(PathBuf, &[u8])> = (0..MAX_RECORDS + 8)
        .map(|index| (root.join(format!("legacy-{index}.json")), &b"{}"[..]))
        .chain([
            (root.join(format!("{key}.json")), &b"legacy record"[..]),
            (
                root.join(format!("{key}.idx"))
                    .join(format!("proj-{}", layer.generation)),
                &b"legacy index"[..],
            ),
            (
                root.join("orphan.idx").join("proj-x"),
                &b"legacy orphan"[..],
            ),
        ])
        .collect();
    for (path, bytes) in &legacy {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
    }
    let planted = legacy_victims(root);
    let legacy_builder = hold(&root.join(format!("{key}.build.lock")));
    let legacy_saver = hold(&root.join(format!("{key}.lock")));
    fill(&cache);
    for index in 0..MAX_RECORDS {
        age(&cache, &format!("filler-{index}"));
    }
    append(&source.0, &sparse(2450, &[2420])[2400..]);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle 2420", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    assert_eq!(run.unwrap().counts, vec![1]);
    assert_eq!(progress.cache_hits, 1);
    assert_eq!(cache.records().len(), MAX_RECORDS);
    let extended = keyed(&cache, &key).file.index.unwrap();
    assert_eq!(
        extended.committed,
        std::fs::metadata(&source.0).unwrap().len()
    );
    for (path, bytes) in &legacy {
        assert_eq!(
            &std::fs::read(path).unwrap()[..],
            *bytes,
            "{}",
            path.display()
        );
    }
    assert_eq!(legacy_victims(root), planted);
    assert_eq!(
        std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<BTreeSet<_>>(),
        planted
            .iter()
            .cloned()
            .chain([
                PROTOCOL_DIR.to_owned(),
                format!("{key}.build.lock"),
                format!("{key}.lock"),
            ])
            .collect::<BTreeSet<_>>()
    );
    drop((legacy_builder, legacy_saver));
    for path in planted.iter().map(|name| root.join(name)) {
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
    assert_referenced(&cache, &key, &extended);
    let (run, _, progress) = checkpointed(
        &source.0,
        &[("needle", None)],
        options(),
        true,
        limits(),
        Some(&cache.1),
    );
    let (fresh, _, _) = streamed(&source.0, &[("needle", None)], options(), true, limits());
    assert_eq!(run.unwrap(), fresh.unwrap());
    assert_eq!((progress.cache_hits, progress.cache_invalidations), (1, 0));
}
