use std::path::{Path, PathBuf};

use cc_transcript_core::render::{compact_line, display_path, event_json, transcript_header, Json};
use cc_transcript_core::scan::{
    scan_corpus, ScanBudget, ScanControl, ScanOutcome, ScanPlan, ScanSession,
};
use cc_transcript_core::scan_grep::{GrepOptions, GrepPatternSpec, GrepReducer, GrepSourceResult};
use cc_transcript_core::snapshot::{Cancellation, NativeStore, SnapshotError, Status, WorkLimits};
use cc_transcript_core::types::{ContentBlock, Entry};
use regex::{Regex, RegexBuilder};
use sonic_rs::json;

use crate::output::{click_error, eline, usage_error, CliExit, Out};
use crate::target::py_path;
use crate::DiscoveryOpts;

const USAGE: &str = "cc-transcript grep [OPTIONS] PATTERN [PATHS]...";
const HELP_PATH: &str = "cc-transcript grep";
const DEFAULT_MAX_MATCHES: usize = 20;

#[derive(clap::Args, Default)]
pub struct ScanOptions {
    #[arg(long)]
    pub max_read_bytes: Option<usize>,
    #[arg(long)]
    pub max_events: Option<usize>,
    #[arg(long)]
    pub max_output_bytes: Option<usize>,
    #[arg(long)]
    pub max_discovery_entries: Option<usize>,
    #[arg(long)]
    pub max_sources: Option<usize>,
    #[arg(long)]
    pub timeout_ms: Option<u64>,
}

impl ScanOptions {
    fn apply(&self, mut limits: WorkLimits) -> Result<WorkLimits, CliExit> {
        if let Some(n) = self.max_read_bytes {
            limits.max_read_bytes = n;
        }
        if let Some(n) = self.max_events {
            limits.max_events = n;
            limits.max_items = n;
        }
        if let Some(n) = self.max_output_bytes {
            limits.max_output_bytes = n;
        }
        if let Some(n) = self.max_discovery_entries {
            limits.max_discovery_entries = n;
        }
        if let Some(n) = self.max_sources {
            limits.max_sources = n;
        }
        if let Some(n) = self.timeout_ms {
            limits.deadline_unix_ms = cc_transcript_core::snapshot::now_ms().saturating_add(n);
        }
        if [
            limits.max_read_bytes,
            limits.max_events,
            limits.max_output_bytes,
            limits.max_discovery_entries,
            limits.max_sources,
        ]
        .contains(&0)
            || self.timeout_ms == Some(0)
        {
            return Err(usage_error(
                USAGE,
                HELP_PATH,
                "scan work limits must be positive",
            ));
        }
        Ok(limits)
    }
}

pub struct GrepArgs {
    pub pattern: String,
    pub patterns: Vec<String>,
    pub scan_json: bool,
    pub scan_limits: ScanOptions,
    pub paths: Vec<PathBuf>,
    pub discovery: DiscoveryOpts,
    pub corpus: Option<PathBuf>,
    pub kinds: Vec<String>,
    pub tool: Option<String>,
    pub errors: bool,
    pub ignore_case: bool,
    pub wheres: Vec<String>,
    pub context: usize,
    pub max_matches: Option<usize>,
    pub width: usize,
    pub uuids: bool,
    pub with_result: bool,
    pub json: bool,
}

pub fn compile_pattern(
    pattern: &str,
    ignore_case: bool,
    usage: &str,
    help_path: &str,
) -> Result<Regex, CliExit> {
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| usage_error(usage, help_path, &format!("invalid pattern: {e}")))
}

/// An empty `--where` searches everywhere (cli.py's default).
pub fn where_flags(wheres: &[String]) -> (bool, bool, bool) {
    if wheres.is_empty() {
        return (true, true, true);
    }
    (
        wheres.iter().any(|w| w == "text"),
        wheres.iter().any(|w| w == "thinking"),
        wheres.iter().any(|w| w == "tools"),
    )
}

fn cap(args: &GrepArgs) -> Option<usize> {
    args.max_matches.map_or_else(
        || args.corpus.is_none().then_some(DEFAULT_MAX_MATCHES),
        |n| (n > 0).then_some(n),
    )
}

fn snapshot_error(error: SnapshotError) -> CliExit {
    click_error(&format!("{}: {}", error.status.as_str(), error.reason))
}

fn output_error() -> SnapshotError {
    SnapshotError::new(Status::OutputLimit, "stdout write failed")
}

struct Emitter {
    out: Out,
    scan_json: bool,
    first: bool,
    exit: Option<i32>,
}

impl Emitter {
    fn new(
        scan_json: bool,
        budget: &mut ScanBudget,
        _cancel: &Cancellation,
    ) -> Result<Self, CliExit> {
        let mut out = Out::new();
        if scan_json {
            if budget.remaining().max_output_bytes < 4096 {
                return Err(click_error(
                    "structured scan output requires 4096 status bytes",
                ));
            }
            budget.progress.output_bytes += 4096;
            out.line("{\"schema\":\"cc-transcript.scan/1\",\"matches\":[")?;
        }
        Ok(Self {
            out,
            scan_json,
            first: true,
            exit: None,
        })
    }

    fn emit(
        &mut self,
        line: String,
        budget: &mut ScanBudget,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        budget.charge_output(line.len().saturating_add(2), cancel)?;
        let result = if self.scan_json && !self.first {
            self.out.line(",").and_then(|()| self.out.line(&line))
        } else {
            self.out.line(&line)
        };
        self.first = false;
        result.map_err(|error| {
            self.exit = Some(error.0);
            output_error()
        })
    }

    fn finish(mut self, outcome: &ScanOutcome, counts: &[usize]) -> Result<(), CliExit> {
        if let Some(code) = self.exit {
            return Err(CliExit(code));
        }
        if self.scan_json {
            let status = cc_transcript_core::snapshot_codec::encode(
                &json!({"outcome":outcome,"counts":counts}),
                4032,
            )
            .map_err(snapshot_error)?;
            if status.len().saturating_add(64) > 4096 {
                return Err(click_error("scan status exceeds reserved output"));
            }
            self.out.line(&format!("],{}", &status[1..]))?;
        }
        self.out.finish()?;
        if let Some(reason) = &outcome.reason {
            if reason == "result_limit" {
                eline(&format!("warning: stopped at --max-matches {}; more matches may exist — raise it, or pass --max-matches 0 for no cap",counts.first().copied().unwrap_or(0)));
            } else {
                eline(&format!("warning: scan incomplete: {reason}"));
                return Err(CliExit(3));
            }
        }
        if counts.iter().all(|count| *count == 0) {
            return Err(CliExit(1));
        }
        Ok(())
    }
}

fn result_fields(event: &Entry, result: &GrepSourceResult<'_>) -> Vec<(String, Json)> {
    if !matches!(event, Entry::Assistant(_)) {
        return Vec::new();
    }
    event
        .blocks()
        .iter()
        .filter_map(|block| {
            let ContentBlock::ToolUse(tool) = block else {
                return None;
            };
            let value = result.results.get(tool.id.as_str())?;
            Some((
                tool.id.clone(),
                Json::Obj(vec![
                    ("is_error".into(), Json::Bool(value.is_error)),
                    ("denied".into(), Json::Bool(value.denied)),
                    (
                        "duration_ms".into(),
                        value.duration_ms.map_or(Json::Null, Json::Int),
                    ),
                ]),
            ))
        })
        .collect()
}

fn result_suffix(event: &Entry, result: &GrepSourceResult<'_>) -> String {
    if !matches!(event, Entry::Assistant(_)) {
        return String::new();
    }
    let markers: Vec<_> = event
        .blocks()
        .iter()
        .filter_map(|block| {
            let ContentBlock::ToolUse(tool) = block else {
                return None;
            };
            let value = result.results.get(tool.id.as_str())?;
            let status = if value.denied {
                "[denied]"
            } else if value.is_error {
                "[err]"
            } else {
                ""
            };
            let duration = value
                .duration_ms
                .map(|ms| format!("({ms}ms)"))
                .unwrap_or_default();
            let marker = [status.to_owned(), duration]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            (!marker.is_empty()).then_some(marker)
        })
        .collect();
    if markers.is_empty() {
        String::new()
    } else {
        format!(" {}", markers.join(" "))
    }
}

pub fn run(args: GrepArgs) -> Result<(), CliExit> {
    let store = NativeStore::new(&json!({})).map_err(snapshot_error)?;
    let limits = args.scan_limits.apply(store.scan_limits())?;
    let cancel = Cancellation::default();
    *crate::SCAN_CANCELLATION.lock().expect("scan cancellation") = Some(cancel.clone());
    let _interrupt = ScanInterrupt;
    if let Some(path) = &args.corpus {
        return run_over_corpus(&args, path, limits, cancel);
    }
    let mut session = ScanSession::new(&store, limits, cancel.clone());
    let mut reducer = match prepare_reducer(&args, &mut session.budget, &cancel) {
        Ok(reducer) => reducer,
        Err(error) if error.status == Status::InvalidRequest => {
            return Err(usage_error(USAGE, HELP_PATH, &error.reason))
        }
        Err(error) => {
            let emitter = Emitter::new(
                args.scan_json,
                &mut session.budget,
                &Cancellation::default(),
            )?;
            return emitter.finish(
                &failed_outcome(error, &session.budget),
                &vec![0; args.patterns.len() + 1],
            );
        }
    };
    let plan = ScanPlan {
        paths: args
            .paths
            .iter()
            .map(|path| py_path(&path.to_string_lossy()))
            .collect(),
        root: args.discovery.validated_root(USAGE, HELP_PATH)?,
        project: args.discovery.project.clone(),
        contains: args.discovery.contains.clone(),
        source_limit: args.discovery.effective_limit(),
    };
    let mut emitter = Emitter::new(args.scan_json, &mut session.budget, &cancel)?;
    let mut files = 0usize;
    let mut outcome = session.run(&plan, |path, snapshot, budget, cancel| {
        let result = reducer.scan_source(snapshot, budget, cancel)?;
        if result.hits.is_empty() {
            return Ok(if result.quota_reached {
                ScanControl::Stop {
                    source_complete: result.source_complete,
                }
            } else {
                ScanControl::Continue
            });
        }
        files += 1;
        if !args.json && !args.scan_json {
            emitter.emit(transcript_header(&path.to_string_lossy()), budget, cancel)?;
        }
        for (window_index, window) in result.windows.iter().enumerate() {
            if !args.json && !args.scan_json && args.context > 0 && window_index > 0 {
                emitter.emit("--".to_owned(), budget, cancel)?;
            }
            for index in window.clone() {
                result.preflight_render(snapshot, index, budget, cancel)?;
                let event = snapshot.entry(index);
                let hit = result.hits.iter().find(|hit| hit.event_index == index);
                let line = if args.json || args.scan_json {
                    let Json::Obj(mut fields) = event_json(index, event) else {
                        unreachable!("event_json object")
                    };
                    fields.insert(
                        0,
                        (
                            "path".into(),
                            Json::Str(path.to_string_lossy().into_owned()),
                        ),
                    );
                    if hit.is_none() {
                        fields.push(("context".into(), Json::Bool(true)));
                    }
                    if args.with_result {
                        let results = result_fields(event, &result);
                        if !results.is_empty() {
                            fields.push(("results".into(), Json::Obj(results)));
                        }
                    }
                    if args.scan_json {
                        fields.push(("generation".into(), Json::Str(snapshot.id.clone())));
                        fields.push((
                            "pattern_ids".into(),
                            Json::Arr(
                                hit.map(|hit| {
                                    hit.pattern_ids
                                        .iter()
                                        .map(|id| Json::Int(*id as i64))
                                        .collect()
                                })
                                .unwrap_or_default(),
                            ),
                        ));
                    }
                    Json::Obj(fields).dumps()
                } else {
                    let mut line =
                        compact_line(index, event, &result.names, args.width, false, args.uuids);
                    if args.with_result {
                        line.push_str(&result_suffix(event, &result));
                    }
                    line
                };
                emitter.emit(line, budget, cancel)?;
            }
        }
        Ok(if result.quota_reached {
            ScanControl::Stop {
                source_complete: result.source_complete,
            }
        } else {
            ScanControl::Continue
        })
    });
    if outcome.complete && !reducer.complete() {
        outcome.complete = false;
        outcome.reason = Some("result_limit".to_owned());
    }
    let counts = reducer.counts();
    if !args.json && !args.scan_json {
        let note = if outcome.selected_sources < outcome.available_sources {
            format!(
                " · searched {} of {} transcripts — use --all",
                outcome.selected_sources, outcome.available_sources
            )
        } else {
            String::new()
        };
        if let Err(error) = emitter.emit(
            format!(
                "{files} files, {} matches{note}",
                counts.iter().sum::<usize>()
            ),
            &mut session.budget,
            &cancel,
        ) {
            outcome.complete = false;
            outcome.reason = Some(error.reason);
        }
        outcome.progress = session.budget.progress.clone();
    }
    emitter.finish(&outcome, counts)
}

fn run_over_corpus(
    args: &GrepArgs,
    path: &Path,
    limits: WorkLimits,
    cancel: Cancellation,
) -> Result<(), CliExit> {
    let mut budget = ScanBudget::new(limits);
    let mut reducer = match prepare_reducer(args, &mut budget, &cancel) {
        Ok(reducer) => reducer,
        Err(error) if error.status == Status::InvalidRequest => {
            return Err(usage_error(USAGE, HELP_PATH, &error.reason))
        }
        Err(error) => {
            let emitter = Emitter::new(args.scan_json, &mut budget, &Cancellation::default())?;
            return emitter.finish(
                &failed_outcome(error, &budget),
                &vec![0; args.patterns.len() + 1],
            );
        }
    };
    let mut emitter = Emitter::new(args.scan_json, &mut budget, &cancel)?;
    let mut outcome = scan_corpus(
        path,
        &mut budget,
        &cancel,
        |line_number, line, budget, cancel| {
            let ids = reducer.scan_text(line, budget, cancel)?;
            if !ids.is_empty() {
                budget.charge_projection(
                    line.len().saturating_mul(6).saturating_add(1024),
                    0,
                    cancel,
                )?;
                let rendered = if args.scan_json {
                    sonic_rs::to_string(&json!({"path":path.to_string_lossy().as_ref(),"line":line_number,"text":line,"pattern_ids":ids})).map_err(|e|SnapshotError::new(Status::OutputLimit,e.to_string()))?
                } else {
                    line.to_owned()
                };
                emitter.emit(rendered, budget, cancel)?;
            }
            Ok(reducer.satisfied())
        },
    );
    if outcome.complete && !reducer.complete() {
        outcome.complete = false;
        outcome.reason = Some("result_limit".to_owned());
    }
    let counts = reducer.counts();
    if !args.scan_json {
        if let Err(error) = emitter.emit(
            format!(
                "{} matches in {}",
                counts.iter().sum::<usize>(),
                display_path(&path.to_string_lossy())
            ),
            &mut budget,
            &cancel,
        ) {
            outcome.complete = false;
            outcome.reason = Some(error.reason);
        }
        outcome.progress = budget.progress.clone();
    }
    emitter.finish(&outcome, counts)
}

fn failed_outcome(error: SnapshotError, budget: &ScanBudget) -> ScanOutcome {
    ScanOutcome {
        complete: false,
        reason: Some(format!("{}: {}", error.status.as_str(), error.reason)),
        selected_sources: 0,
        available_sources: 0,
        progress: budget.progress.clone(),
    }
}

struct ScanInterrupt;
impl Drop for ScanInterrupt {
    fn drop(&mut self) {
        *crate::SCAN_CANCELLATION.lock().expect("scan cancellation") = None;
    }
}

fn prepare_reducer(
    args: &GrepArgs,
    budget: &mut ScanBudget,
    cancel: &Cancellation,
) -> Result<GrepReducer, SnapshotError> {
    let (where_text, where_thinking, where_tools) = where_flags(&args.wheres);
    let patterns = std::iter::once(&args.pattern)
        .chain(args.patterns.iter())
        .enumerate()
        .map(|(id, pattern)| GrepPatternSpec {
            id,
            pattern: pattern.clone(),
            max_matches: cap(&args),
        })
        .collect();
    let options = GrepOptions {
        kinds: args.kinds.clone(),
        tool: args.tool.clone(),
        errors: args.errors,
        ignore_case: args.ignore_case,
        where_text,
        where_thinking,
        where_tools,
        context: args.context,
        with_result: args.with_result,
    };
    GrepReducer::new(patterns, options, budget, cancel)
}
