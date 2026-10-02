use super::*;
use crate::scan::{ScanControl, ScanPlan, ScanProgress, ScanSession};
use crate::scan_checkpoint::{GrepCheckpoints, SourceRecord, MAX_RECORD_BYTES, PREFIX_SEGMENT};
use crate::scan_stream::SourceStream;
use crate::snapshot::{NativeStore, WorkLimits};
use sonic_rs::{json, Value};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    let run = control.map(|control| {
        let (stop, names_through) = stop(&control.expect("claude source streams"));
        Run {
            emitted: std::mem::take(&mut emitted),
            counts: grep.counts().to_vec(),
            stop,
            names_through,
            complete: grep.complete(),
        }
    });
    (run, emitted, budget.progress.clone())
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
    assert!(stream.verify().unwrap());
    std::fs::File::options()
        .write(true)
        .open(&source.0)
        .unwrap()
        .set_len(1)
        .unwrap();
    assert_eq!(stream.verify().unwrap_err().status, Status::Changed);
}

struct Cache(PathBuf, GrepCheckpoints);

impl Cache {
    fn new() -> Self {
        Self::segmented(PREFIX_SEGMENT)
    }

    fn segmented(segment: u64) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cc-grep-checkpoints-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self(
            dir.clone(),
            GrepCheckpoints::new(dir, "test".into()).segmented(segment),
        )
    }

    fn records(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    fn record(&self) -> SourceRecord {
        sonic_rs::from_slice(&std::fs::read(&self.records()[0]).unwrap()).unwrap()
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
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
        append(&source.0, &sparse(110 + round * 10, &[])[100 + round * 10..]);
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

#[test]
fn a_racing_append_past_the_validation_cap_is_incomplete() {
    let source = Source::new(&sparse(100, &[50]), true);
    let (result, _, progress) = racing(
        &source,
        &json!({"max_scan_validate_bytes": 1000}),
        None,
        None,
    );
    assert_eq!(result.err().unwrap().status, Status::Incomplete);
    assert_eq!(progress.validated_bytes, 0);
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
