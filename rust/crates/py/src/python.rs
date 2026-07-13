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
use cc_transcript_core::activity::{session_activity, ActivityOpts, SessionActivity};
use cc_transcript_core::command::CommandLine;
use cc_transcript_core::filter::{compile_spec, spec_keep, CompiledSpec};
use cc_transcript_core::parse::{parse_bytes, parse_print_envelope};
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
    m.add_function(wrap_pyfunction!(session_activity_probe, m)?)?;
    m.add_function(wrap_pyfunction!(crate::nlp::nlp_analyze, m)?)?;
    m.add_class::<ParseStream>()?;
    Ok(())
}
