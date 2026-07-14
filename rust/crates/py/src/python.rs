use chrono::{DateTime, Datelike, FixedOffset};
use crossbeam_channel::{bounded, Receiver};
use once_cell::sync::Lazy;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFloat, PyList, PyString, PyTuple};
use rayon::prelude::*;
use sonic_rs::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use crate::event::{build_event, build_print_result, parse_err};
use crate::{command, lexicon, mining, score};
use cc_transcript_core::activity::{
    hunk_overlap, lift_session, overlap_between, result_index, session_activity, ActivityOpts,
    Hunk, SessionActivity,
};
use cc_transcript_core::command::CommandLine;
use cc_transcript_core::facts;
use cc_transcript_core::filter::{compile_spec, spec_keep, CompiledSpec};
use cc_transcript_core::ids;
use cc_transcript_core::parse::{parse_bytes, parse_print_envelope};
use cc_transcript_core::query::{FileRef, Session};
use cc_transcript_core::types::Entry;

static PARSE_POOL: Lazy<rayon::ThreadPool> = Lazy::new(|| {
    let n = std::env::var("CC_TRANSCRIPT_PARSE_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| (num_cpus::get() * 2).min(32));
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("cc-transcript-parse-{i}"))
        .build()
        .expect("parse pool builds")
});

struct ParsedFile {
    path: String,
    mtime: f64,
    lines: Vec<Entry>,
}

// The drop-spec evaluates on the typed entry, so dropped lines are parsed but
// never materialized into Python objects.
fn parse_file_internal(
    path: &str,
    mtime: f64,
    filter: Option<&CompiledSpec>,
) -> Option<ParsedFile> {
    let bytes = std::fs::read(path).ok()?;
    let lines = parse_bytes(&bytes, |entry| {
        filter.is_none_or(|spec| spec_keep(spec, entry))
    })
    .ok()?;
    Some(ParsedFile {
        path: path.to_string(),
        mtime,
        lines,
    })
}

// A typed entry that cannot materialize to a Python event (e.g. a year-zero
// timestamp below Python's MINYEAR) is dropped; the rest of the file survives, so
// one corrupt event never discards the whole transcript.
fn parsed_file_to_py<'py>(py: Python<'py>, pf: ParsedFile) -> PyResult<Bound<'py, PyAny>> {
    let events = pf
        .lines
        .iter()
        .filter_map(|line| build_event(py, line).ok())
        .collect::<Vec<_>>();
    PyTuple::new(
        py,
        [
            PyString::new(py, &pf.path).into_any(),
            PyFloat::new(py, pf.mtime).into_any(),
            PyList::new(py, events)?.into_any(),
        ],
    )
    .map(Bound::into_any)
}

#[pyclass]
pub struct ParseStream {
    rx: Receiver<ParsedFile>,
}

#[pymethods]
impl ParseStream {
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        match py.detach(|| self.rx.recv().ok()) {
            None => Ok(None),
            Some(pf) => parsed_file_to_py(py, pf).map(Some),
        }
    }

    fn recv_many<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let mut out: Vec<Bound<'py, PyAny>> = Vec::new();
        // Block for the first file; return [] only when the channel is closed.
        match py.detach(|| self.rx.recv().ok()) {
            None => return Ok(out),
            Some(pf) => out.push(parsed_file_to_py(py, pf)?),
        }
        // Drain what is already buffered without blocking.
        while out.len() < max {
            match py.detach(|| self.rx.try_recv().ok()) {
                None => break,
                Some(pf) => out.push(parsed_file_to_py(py, pf)?),
            }
        }
        Ok(out)
    }
}

#[pyfunction]
#[pyo3(signature = (paths, prefetch, spec_json=None))]
fn stream_parse(
    paths: Vec<(String, f64)>,
    prefetch: usize,
    spec_json: Option<String>,
) -> PyResult<ParseStream> {
    let filter: Option<Arc<CompiledSpec>> = match spec_json {
        Some(json) => Some(Arc::new(
            compile_spec(&json).map_err(PyValueError::new_err)?,
        )),
        None => None,
    };
    let depth = prefetch.max(1);
    let (tx, rx) = bounded::<ParsedFile>(depth);
    let (permits_tx, permits_rx) = bounded::<()>(depth);
    for _ in 0..depth {
        permits_tx.send(()).expect("seed permits");
    }
    thread::spawn(move || {
        PARSE_POOL.install(|| {
            paths.into_par_iter().for_each_with(
                (tx, permits_tx, permits_rx),
                |(tx, permits_tx, permits_rx), (path, mtime)| {
                    if permits_rx.recv().is_err() {
                        return;
                    }
                    if let Some(pf) = parse_file_internal(&path, mtime, filter.as_deref()) {
                        let _ = tx.send(pf);
                    }
                    let _ = permits_tx.send(());
                },
            );
        });
    });
    Ok(ParseStream { rx })
}

#[pyfunction]
fn parse_print_result<'py>(py: Python<'py>, raw: &[u8]) -> PyResult<Bound<'py, PyAny>> {
    let value: Value = sonic_rs::from_slice(raw)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    let result = parse_print_envelope(&value).map_err(parse_err)?;
    build_print_result(py, &result)
}

#[pyfunction]
fn lexicon_tokenize(text: &str) -> PyResult<Vec<String>> {
    lexicon::tokenize(text).map_err(PyValueError::new_err)
}

#[pyfunction]
fn lexicon_polarity(token: &str) -> i32 {
    lexicon::polarity(token)
}

#[pyfunction]
fn lexicon_has_hit(text: &str, want_negative: bool) -> PyResult<bool> {
    lexicon::has_hit(text, want_negative).map_err(PyValueError::new_err)
}

#[pyfunction]
fn lexicon_overrides() -> Vec<(String, i32)> {
    lexicon::overrides_entries()
}

#[pyfunction]
fn embedded_literals(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    use cc_transcript_core::generated::{command, mining, protocol};

    let dict = PyDict::new(py);
    dict.set_item("protocol.DENIAL_PREFIX", protocol::DENIAL_PREFIX)?;
    dict.set_item(
        "protocol.DENIAL_KIND_USER_REJECTED",
        protocol::DENIAL_KIND_USER_REJECTED,
    )?;
    dict.set_item(
        "protocol.DENIAL_KIND_PERMISSION_RULE",
        protocol::DENIAL_KIND_PERMISSION_RULE,
    )?;
    dict.set_item("protocol.USER_SAID_MARKER", protocol::USER_SAID_MARKER)?;
    dict.set_item("protocol.USER_SAID_TRAILER", protocol::USER_SAID_TRAILER)?;
    dict.set_item("protocol.ANSWERED_PREFIX", protocol::ANSWERED_PREFIX)?;
    dict.set_item("protocol.ANSWERED_TRAILER", protocol::ANSWERED_TRAILER)?;
    dict.set_item(
        "protocol.INTERRUPT_MARKER_PATTERN",
        protocol::INTERRUPT_MARKER_PATTERN,
    )?;
    dict.set_item(
        "protocol.AGENT_INJECTION_PATTERN",
        protocol::AGENT_INJECTION_PATTERN,
    )?;
    dict.set_item("mining.TRANSCRIPT_MESSAGE", mining::TRANSCRIPT_MESSAGE)?;
    dict.set_item("mining.PLAN_REVIEW", mining::PLAN_REVIEW)?;
    dict.set_item("mining.INTERRUPT_REJECTION", mining::INTERRUPT_REJECTION)?;
    dict.set_item("mining.REVIEW_COMMENT", mining::REVIEW_COMMENT)?;
    dict.set_item("mining.QUESTION_ANSWER", mining::QUESTION_ANSWER)?;
    dict.set_item(
        "mining.DETECTOR_TRANSCRIPT_MESSAGE",
        mining::DETECTOR_TRANSCRIPT_MESSAGE,
    )?;
    dict.set_item(
        "mining.DETECTOR_EXIT_PLAN_REJECTION",
        mining::DETECTOR_EXIT_PLAN_REJECTION,
    )?;
    dict.set_item(
        "mining.DETECTOR_PLAN_REENTRY",
        mining::DETECTOR_PLAN_REENTRY,
    )?;
    dict.set_item("mining.DETECTOR_DENIAL", mining::DETECTOR_DENIAL)?;
    dict.set_item("mining.DETECTOR_INTERRUPT", mining::DETECTOR_INTERRUPT)?;
    dict.set_item(
        "mining.DETECTOR_REVIEW_COMMENT",
        mining::DETECTOR_REVIEW_COMMENT,
    )?;
    dict.set_item(
        "mining.DETECTOR_ASK_USER_QUESTION",
        mining::DETECTOR_ASK_USER_QUESTION,
    )?;
    dict.set_item("mining.ANSWER_PREVIEW_SEP", mining::ANSWER_PREVIEW_SEP)?;
    dict.set_item("mining.ANSWER_NOTES_SEP", mining::ANSWER_NOTES_SEP)?;
    dict.set_item("mining.NO_OPTION_SELECTED", mining::NO_OPTION_SELECTED)?;
    dict.set_item("mining.NONE", mining::NONE)?;
    dict.set_item("mining.LOW", mining::LOW)?;
    dict.set_item("command.WRAPPER_COMMANDS", command::WRAPPER_COMMANDS)?;
    dict.set_item("command.MULTI_LEVEL_TOOLS", command::MULTI_LEVEL_TOOLS)?;
    dict.set_item("command.COMPOUND_OPS", command::COMPOUND_OPS)?;
    dict.set_item("command.ASSIGNMENT_PATTERN", command::ASSIGNMENT_PATTERN)?;
    Ok(dict)
}

#[pyfunction]
fn score_short_circuit(spec_json: String, buckets: Vec<Vec<String>>) -> PyResult<Vec<Option<i64>>> {
    score::score_short_circuit(&spec_json, &buckets).map_err(PyValueError::new_err)
}

#[pyfunction]
fn score_post_process(
    spec_json: String,
    buckets: Vec<Vec<String>>,
    raw: Vec<i64>,
) -> PyResult<Vec<i64>> {
    score::score_post_process(&spec_json, &buckets, &raw).map_err(PyValueError::new_err)
}

#[pyfunction]
fn command_prefixes(py: Python<'_>, commands: Vec<String>) -> Vec<Vec<String>> {
    py.detach(|| commands.par_iter().map(|c| command::prefixes(c)).collect())
}

#[pyfunction]
fn command_parse<'py>(py: Python<'py>, command: &str) -> PyResult<Bound<'py, PyDict>> {
    command::line_to_py(py, &CommandLine::parse(command))
}

#[pyfunction]
#[pyo3(signature = (path, waiting_tools=None, human_facing_tools=None))]
fn session_activity_probe<'py>(
    py: Python<'py>,
    path: String,
    waiting_tools: Option<Vec<String>>,
    human_facing_tools: Option<Vec<String>>,
) -> PyResult<Bound<'py, PyDict>> {
    let defaults = ActivityOpts::default();
    let opts = ActivityOpts {
        waiting_tools: waiting_tools.map_or(defaults.waiting_tools, HashSet::from_iter),
        human_facing_tools: human_facing_tools
            .map_or(defaults.human_facing_tools, HashSet::from_iter),
    };
    let activity = py.detach(|| -> PyResult<SessionActivity> {
        let bytes = std::fs::read(&path)?;
        Ok(session_activity(
            &parse_bytes(&bytes, |_| true).map_err(parse_err)?,
            &opts,
        ))
    })?;
    let dict = PyDict::new(py);
    dict.set_item("is_waiting", activity.is_waiting)?;
    dict.set_item("mid_tool", activity.mid_tool)?;
    dict.set_item("last_event_epoch", activity.last_event_epoch)?;
    let pending = activity
        .pending
        .iter()
        .map(|item| {
            let entry = PyDict::new(py);
            entry.set_item("tool_use_id", item.tool_use_id.as_deref())?;
            entry.set_item("name", &item.name)?;
            entry.set_item("kind", item.kind.as_str())?;
            Ok(entry)
        })
        .collect::<PyResult<Vec<_>>>()?;
    dict.set_item("pending", PyList::new(py, pending)?)?;
    Ok(dict)
}

#[pyfunction]
#[pyo3(signature = (events, spec_json, callable_formats))]
fn mine_events<'py>(
    py: Python<'py>,
    events: Vec<Bound<'py, PyAny>>,
    spec_json: String,
    callable_formats: Vec<(String, Py<PyAny>, Py<PyAny>)>,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let mut spec = mining::compile_spec(&spec_json).map_err(PyValueError::new_err)?;
    mining::attach_callable_formats(&mut spec, callable_formats);
    mining::mine_events(py, &events, &spec)
}

#[pyfunction]
fn ids_canonical_json(value_json: &str) -> PyResult<String> {
    let value: Value = sonic_rs::from_str(value_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    ids::canonical_json(&value).map_err(PyValueError::new_err)
}

#[pyfunction]
fn ids_tool_digest(name: &str, input_json: &str) -> PyResult<String> {
    let input: Value = sonic_rs::from_str(input_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    ids::tool_digest(name, &input).map_err(PyValueError::new_err)
}

// activity.py `ms`: round(dt.timestamp() * 1000), the round-half-even float path Python
// takes — not chrono's truncating timestamp_millis — so sub-ms stamps project identically.
fn epoch_ms(dt: DateTime<FixedOffset>) -> i64 {
    ((dt.timestamp_micros() as f64 / 1_000_000.0) * 1000.0).round_ties_even() as i64
}

#[pyfunction]
#[pyo3(signature = (path, max_events))]
fn activity_lift<'py>(
    py: Python<'py>,
    path: String,
    max_events: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let entries: Vec<Entry> = py.detach(|| -> PyResult<Vec<Entry>> {
        let bytes = std::fs::read(&path)?;
        parse_bytes(&bytes, |_| true).map_err(parse_err)
    })?;
    // Mirror stream_parse's materialization drop: a year-0000 timestamp has no Python
    // datetime (MINYEAR), so build_event drops it — P3 moves this guard to parse time.
    let capped: Vec<Entry> = entries
        .into_iter()
        .filter(|entry| entry.meta().map_or(true, |m| m.timestamp.year() >= 1))
        .take(max_events)
        .collect();
    let lift = lift_session("", &capped);
    let out = PyDict::new(py);
    out.set_item("turn_count", lift.turns.len())?;
    let turns = lift
        .turns
        .iter()
        .map(|turn| {
            let td = PyDict::new(py);
            td.set_item("index", turn.index)?;
            td.set_item("prompt", &turn.prompt)?;
            td.set_item("started_at_ms", turn.started_at.map(epoch_ms))?;
            td.set_item("ended_at_ms", turn.ended_at.map(epoch_ms))?;
            td.set_item("event_count", turn.events.len())?;
            let tool_uses = turn
                .tool_uses
                .iter()
                .map(|use_| {
                    let ud = PyDict::new(py);
                    ud.set_item("event_uuid", use_.event_uuid)?;
                    ud.set_item("tool_use_id", use_.tool_use_id)?;
                    ud.set_item("name", use_.name)?;
                    ud.set_item("ts_ms", epoch_ms(use_.ts))?;
                    ud.set_item("has_result", use_.result.is_some())?;
                    ud.set_item("result_is_error", use_.result.map(|r| r.is_error))?;
                    ud.set_item("result_ts_ms", use_.result_ts.map(epoch_ms))?;
                    ud.set_item("duration_ms", use_.duration_ms())?;
                    Ok(ud)
                })
                .collect::<PyResult<Vec<_>>>()?;
            td.set_item("tool_uses", PyList::new(py, tool_uses)?)?;
            let edits = turn
                .edits()
                .iter()
                .map(|edit| {
                    let ed = PyDict::new(py);
                    ed.set_item("file_path", &edit.file_path)?;
                    ed.set_item("tool", edit.tool)?;
                    let hunks = edit
                        .hunks
                        .iter()
                        .map(|h| {
                            let hd = PyDict::new(py);
                            hd.set_item("old", &h.old)?;
                            hd.set_item("new", &h.new)?;
                            Ok(hd)
                        })
                        .collect::<PyResult<Vec<_>>>()?;
                    ed.set_item("hunks", PyList::new(py, hunks)?)?;
                    ed.set_item("event_uuid", edit.event_uuid)?;
                    ed.set_item("tool_use_id", edit.tool_use_id)?;
                    ed.set_item("turn_index", edit.turn_index)?;
                    ed.set_item("ts_ms", epoch_ms(edit.ts))?;
                    Ok(ed)
                })
                .collect::<PyResult<Vec<_>>>()?;
            td.set_item("edits", PyList::new(py, edits)?)?;
            Ok(td)
        })
        .collect::<PyResult<Vec<_>>>()?;
    out.set_item("turns", PyList::new(py, turns)?)?;
    let index = result_index(&capped)
        .iter()
        .map(|r| {
            let rd = PyDict::new(py);
            rd.set_item("tool_use_id", r.tool_use_id)?;
            rd.set_item("result_ts_ms", r.result_ts.map(epoch_ms))?;
            rd.set_item("is_error", r.block.is_error)?;
            Ok(rd)
        })
        .collect::<PyResult<Vec<_>>>()?;
    out.set_item("result_index", PyList::new(py, index)?)?;
    let edits = lift.edits();
    let mut overlaps: Vec<Bound<PyDict>> = Vec::new();
    for i in 0..edits.len() {
        for j in (i + 1)..edits.len() {
            if edits[i].file_path == edits[j].file_path {
                let od = PyDict::new(py);
                od.set_item("a_tool_use_id", edits[i].tool_use_id)?;
                od.set_item("b_tool_use_id", edits[j].tool_use_id)?;
                od.set_item("overlap", overlap_between(&edits[i].hunks, &edits[j].hunks))?;
                overlaps.push(od);
            }
        }
    }
    out.set_item("hunk_overlaps", PyList::new(py, overlaps)?)?;
    Ok(out)
}

// A deterministic query.py battery, mirrored by gen_query_golden.py's project_session.
#[pyfunction]
#[pyo3(signature = (path, max_events))]
fn query_session<'py>(
    py: Python<'py>,
    path: String,
    max_events: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let entries: Vec<Entry> = py.detach(|| -> PyResult<Vec<Entry>> {
        let bytes = std::fs::read(&path)?;
        parse_bytes(&bytes, |_| true).map_err(parse_err)
    })?;
    let capped: Vec<Entry> = entries
        .into_iter()
        .filter(|entry| entry.meta().map_or(true, |m| m.timestamp.year() >= 1))
        .take(max_events)
        .collect();
    let lift = lift_session("", &capped);
    let session = Session::from_lift(&lift);

    let paths = |files: Vec<FileRef>| files.into_iter().map(|f| f.path).collect::<Vec<_>>();

    let out = PyDict::new(py);
    out.set_item("tool_calls", session.tool_calls().count())?;
    out.set_item(
        "tool_calls_with_errors",
        session.tool_calls().with_errors().count(),
    )?;
    out.set_item("files_touched", paths(session.files_touched()))?;
    out.set_item("edited_files", paths(session.edited_files()))?;
    out.set_item("count_failures", session.count_failures())?;
    out.set_item("commands", session.commands())?;
    out.set_item("first_prompt", session.first_prompt())?;
    out.set_item("user_text", session.user_text())?;
    out.set_item("len", session.len())?;
    out.set_item("bool", session.non_empty())?;

    let has_tool = PyDict::new(py);
    for name in ["Bash", "Edit|Write", "Read", "Task", "Skill"] {
        has_tool.set_item(name, session.has_tool(name))?;
    }
    out.set_item("has_tool", has_tool)?;
    out.set_item(
        "has_command",
        vec![
            session.has_command(&["git", "push"]),
            session.has_command(&["ls"]),
        ],
    )?;
    out.set_item("has_edit_to", session.has_edit_to(&["*.py"]))?;
    out.set_item("has_read", session.has_read("test"))?;
    out.set_item("has_skill", session.has_skill(&["commit", "codex"]))?;
    out.set_item("user_said", session.user_said(&["fix", "error"]))?;
    out.set_item("assistant_text", session.assistant_text(3, 80))?;
    out.set_item(
        "has_override",
        session.has_override("OVERRIDE", &["Edit", "Write"]),
    )?;

    let windows = PyDict::new(py);
    windows.set_item("after_write", session.after("Write", None).len())?;
    windows.set_item("before_bash", session.before("Bash").len())?;
    windows.set_item("prior", session.prior().len())?;
    windows.set_item("recent5", session.recent(5).len())?;
    windows.set_item("current_turn", session.current_turn().len())?;
    out.set_item("windows", windows)?;

    let detail = PyDict::new(py);
    detail.set_item("named_bash", session.tool_calls().named("Bash").count())?;
    detail.set_item(
        "touching_py",
        session.tool_calls().touching(&["*.py"]).count(),
    )?;
    detail.set_item(
        "under_src",
        session
            .tool_calls()
            .under(&["src", "cc_transcript"])
            .count(),
    )?;
    detail.set_item("in_turn0", session.tool_calls().in_turns(&[0]).count())?;
    detail.set_item(
        "first_name",
        session.tool_calls().first().map(|use_| use_.call.name()),
    )?;
    detail.set_item(
        "last_name",
        session.tool_calls().last().map(|use_| use_.call.name()),
    )?;
    detail.set_item("files", paths(session.tool_calls().files()))?;
    out.set_item("tool_calls_detail", detail)?;

    let file_refs = session
        .files_touched()
        .into_iter()
        .map(|f| {
            let fd = PyDict::new(py);
            fd.set_item("path", &f.path)?;
            fd.set_item("is_test", f.is_test())?;
            fd.set_item("suffix", f.suffix())?;
            Ok(fd)
        })
        .collect::<PyResult<Vec<_>>>()?;
    out.set_item("file_refs", PyList::new(py, file_refs)?)?;

    Ok(out)
}

#[pyfunction]
fn activity_hunk_overlap(a_old: &str, a_new: &str, b_old: &str, b_new: &str) -> f64 {
    hunk_overlap(
        &Hunk {
            old: a_old.to_string(),
            new: a_new.to_string(),
        },
        &Hunk {
            old: b_old.to_string(),
            new: b_new.to_string(),
        },
    )
}

fn pairs_list<'py>(
    py: Python<'py>,
    pairs: &[(String, usize)],
    key: &str,
) -> PyResult<Bound<'py, PyList>> {
    let items = pairs
        .iter()
        .map(|(name, count)| {
            let d = PyDict::new(py);
            d.set_item(key, name)?;
            d.set_item("count", *count)?;
            Ok(d)
        })
        .collect::<PyResult<Vec<_>>>()?;
    PyList::new(py, items)
}

fn fact_to_dict<'py>(py: Python<'py>, fact: &facts::ToolFact) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("session_id", &fact.session_id)?;
    d.set_item("tool_use_id", &fact.tool_use_id)?;
    d.set_item("tool", &fact.tool)?;
    d.set_item("command_prefixes", &fact.command_prefixes)?;
    d.set_item("command", &fact.command)?;
    d.set_item("mcp_server", &fact.mcp_server)?;
    d.set_item("mcp_tool", &fact.mcp_tool)?;
    d.set_item("mcp_access", &fact.mcp_access)?;
    d.set_item("file_path", &fact.file_path)?;
    d.set_item("is_error", fact.is_error)?;
    d.set_item("denied", fact.denied)?;
    d.set_item("denial_kind", &fact.denial_kind)?;
    d.set_item("user_said", &fact.user_said)?;
    d.set_item("duration_ms", fact.duration_ms)?;
    d.set_item("ts_ms", epoch_ms(fact.ts))?;
    Ok(d)
}

fn project_facts<'py>(
    py: Python<'py>,
    tool_facts: &[facts::ToolFact],
) -> PyResult<Bound<'py, PyDict>> {
    let fact_dicts = tool_facts
        .iter()
        .map(|f| fact_to_dict(py, f))
        .collect::<PyResult<Vec<_>>>()?;
    let out = PyDict::new(py);
    out.set_item("facts", PyList::new(py, fact_dicts)?)?;
    out.set_item(
        "command_prefix_counts",
        pairs_list(py, &facts::command_prefix_counts(tool_facts), "prefix")?,
    )?;
    let mcp = facts::mcp_summary(tool_facts)
        .into_iter()
        .map(|(server, summary)| {
            let d = PyDict::new(py);
            d.set_item("server", server)?;
            d.set_item("read", summary.read)?;
            d.set_item("write", summary.write)?;
            d.set_item("total", summary.total)?;
            d.set_item("tools", pairs_list(py, &summary.tools, "tool")?)?;
            Ok(d)
        })
        .collect::<PyResult<Vec<_>>>()?;
    out.set_item("mcp_summary", PyList::new(py, mcp)?)?;
    Ok(out)
}

// The facts.py analytics per path, mirrored by gen_facts_golden.py's project_file.
#[pyfunction]
#[pyo3(signature = (paths, max_events))]
fn tool_facts<'py>(
    py: Python<'py>,
    paths: Vec<String>,
    max_events: usize,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    paths
        .iter()
        .map(|path| {
            let facts = py.detach(|| -> PyResult<Vec<facts::ToolFact>> {
                let bytes = std::fs::read(path)?;
                let entries: Vec<Entry> = parse_bytes(&bytes, |_| true).map_err(parse_err)?;
                let capped: Vec<Entry> = entries
                    .into_iter()
                    .filter(|entry| entry.meta().map_or(true, |m| m.timestamp.year() >= 1))
                    .take(max_events)
                    .collect();
                Ok(
                    match capped
                        .iter()
                        .find_map(|e| e.meta().map(|m| m.session_id.clone()))
                    {
                        Some(session_id) => facts::tool_facts(&session_id, path, &capped),
                        None => Vec::new(),
                    },
                )
            })?;
            project_facts(py, &facts)
        })
        .collect()
}

#[pymodule]
fn _parser_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(stream_parse, m)?)?;
    m.add_function(wrap_pyfunction!(parse_print_result, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_tokenize, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_polarity, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_has_hit, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_overrides, m)?)?;
    m.add_function(wrap_pyfunction!(embedded_literals, m)?)?;
    m.add_function(wrap_pyfunction!(score_short_circuit, m)?)?;
    m.add_function(wrap_pyfunction!(score_post_process, m)?)?;
    m.add_function(wrap_pyfunction!(command_prefixes, m)?)?;
    m.add_function(wrap_pyfunction!(command_parse, m)?)?;
    m.add_function(wrap_pyfunction!(mine_events, m)?)?;
    m.add_function(wrap_pyfunction!(ids_canonical_json, m)?)?;
    m.add_function(wrap_pyfunction!(ids_tool_digest, m)?)?;
    m.add_function(wrap_pyfunction!(session_activity_probe, m)?)?;
    m.add_function(wrap_pyfunction!(crate::toolcall::toolcall_parse, m)?)?;
    m.add_function(wrap_pyfunction!(crate::toolcall::toolresult_parse, m)?)?;
    m.add_function(wrap_pyfunction!(activity_lift, m)?)?;
    m.add_function(wrap_pyfunction!(activity_hunk_overlap, m)?)?;
    m.add_function(wrap_pyfunction!(crate::context::context_capture_window, m)?)?;
    m.add_function(wrap_pyfunction!(crate::context::context_roundtrip, m)?)?;
    m.add_function(wrap_pyfunction!(crate::context::context_render_preview, m)?)?;
    m.add_function(wrap_pyfunction!(query_session, m)?)?;
    m.add_function(wrap_pyfunction!(tool_facts, m)?)?;
    m.add_function(wrap_pyfunction!(crate::render::render_tool_call, m)?)?;
    m.add_function(wrap_pyfunction!(crate::render::render_compact_lines, m)?)?;
    m.add_function(wrap_pyfunction!(crate::render::render_haystacks, m)?)?;
    m.add_function(wrap_pyfunction!(crate::render::render_stats, m)?)?;
    m.add_function(wrap_pyfunction!(crate::nlp::nlp_analyze, m)?)?;
    m.add_function(wrap_pyfunction!(
        crate::discovery::discovery_find_transcripts,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(
        crate::discovery::discovery_find_transcript,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(crate::discovery::discovery_find_in, m)?)?;
    m.add_function(wrap_pyfunction!(
        crate::discovery::discovery_subagent_paths,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(
        crate::discovery::discovery_subagent_transcripts,
        m
    )?)?;
    m.add_function(wrap_pyfunction!(
        crate::discovery::discovery_is_subagent_path,
        m
    )?)?;
    m.add_class::<ParseStream>()?;
    m.add_class::<crate::watch::WatchTailer>()?;
    Ok(())
}
