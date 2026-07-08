mod activity;
mod command;
mod event;
mod filter;
mod lexicon;
mod mining;
mod model;
mod parse;
mod protocol;
mod score;
mod types;
mod value;

use crossbeam_channel::{bounded, Receiver};
use memchr::memchr_iter;
use once_cell::sync::Lazy;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFloat, PyList, PyString, PyTuple};
use rayon::prelude::*;
use sonic_rs::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use crate::activity::{session_activity, ActivityOpts, SessionActivity};
use crate::event::{build_event, build_print_result};
use crate::filter::{compile_spec, spec_keep, CompiledSpec};
use crate::parse::{parse_entry, parse_print_envelope, ParseError};
use crate::types::Entry;

const AVG_LINE_BYTES: usize = 1400;

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

// Lines that are not valid JSON are skipped; a JSON line that fails the typed
// parse (e.g. a missing required field) fails the whole file — whole-file
// parity with PythonBackend, which parses every line before filtering. The
// drop-spec evaluates on the typed entry, so dropped lines are parsed but
// never materialized into Python objects.
fn parse_line(
    line: &[u8],
    lines: &mut Vec<Entry>,
    filter: Option<&CompiledSpec>,
) -> Result<(), ParseError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    if let Ok(value) = sonic_rs::from_slice::<Value>(line) {
        let entry = parse_entry(value)?;
        if filter.is_none_or(|spec| spec_keep(spec, &entry)) {
            lines.push(entry);
        }
    }
    Ok(())
}

fn parse_bytes(bytes: &[u8], filter: Option<&CompiledSpec>) -> Result<Vec<Entry>, ParseError> {
    let mut lines: Vec<Entry> = Vec::with_capacity(bytes.len() / AVG_LINE_BYTES + 1);
    let mut start = 0usize;
    for pos in memchr_iter(b'\n', bytes) {
        parse_line(&bytes[start..pos], &mut lines, filter)?;
        start = pos + 1;
    }
    if start < bytes.len() {
        parse_line(&bytes[start..], &mut lines, filter)?;
    }
    Ok(lines)
}

fn parse_file_internal(path: &str, mtime: f64, filter: Option<&CompiledSpec>) -> Option<ParsedFile> {
    let bytes = std::fs::read(path).ok()?;
    Some(ParsedFile {
        path: path.to_string(),
        mtime,
        lines: parse_bytes(&bytes, filter).ok()?,
    })
}

fn parsed_file_to_py<'py>(py: Python<'py>, pf: ParsedFile) -> PyResult<Bound<'py, PyAny>> {
    let events = pf
        .lines
        .iter()
        .map(|line| build_event(py, line))
        .collect::<PyResult<Vec<_>>>()?;
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
    // Malformed files are already skipped at parse time; a file whose typed
    // entries still fail Python materialization is silently skipped too.
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        loop {
            match py.detach(|| self.rx.recv().ok()) {
                None => return Ok(None),
                Some(pf) => {
                    if let Ok(obj) = parsed_file_to_py(py, pf) {
                        return Ok(Some(obj));
                    }
                }
            }
        }
    }

    fn recv_many<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let mut out: Vec<Bound<'py, PyAny>> = Vec::new();
        // Block for the first materialized file; return [] only when the channel
        // is genuinely closed, so an all-skipped batch never reads as "done".
        loop {
            match py.detach(|| self.rx.recv().ok()) {
                None => return Ok(out),
                Some(pf) => {
                    if let Ok(obj) = parsed_file_to_py(py, pf) {
                        out.push(obj);
                        break;
                    }
                }
            }
        }
        // Drain what is already buffered without blocking, skipping bad files.
        while out.len() < max {
            match py.detach(|| self.rx.try_recv().ok()) {
                None => break,
                Some(pf) => {
                    if let Ok(obj) = parsed_file_to_py(py, pf) {
                        out.push(obj);
                    }
                }
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
        Some(json) => Some(Arc::new(compile_spec(&json).map_err(PyValueError::new_err)?)),
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
    let value: Value =
        sonic_rs::from_slice(raw).map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    let result = parse_print_envelope(&value)?;
    build_print_result(py, &result)
}

#[pyfunction]
fn lexicon_available() -> bool {
    lexicon::available()
}

#[pyfunction]
fn lexicon_polarity(lemma: &str) -> i32 {
    lexicon::polarity(lemma)
}

#[pyfunction]
fn lexicon_has_hit(text: &str, floor: i32, want_negative: bool) -> bool {
    lexicon::has_hit(text, floor, want_negative)
}

#[pyfunction]
fn lexicon_overrides() -> Vec<(String, i32)> {
    lexicon::overrides_entries()
}

#[pyfunction]
fn score_short_circuit(spec_json: String, buckets: Vec<Vec<String>>) -> PyResult<Vec<Option<i64>>> {
    score::score_short_circuit(&spec_json, &buckets).map_err(PyValueError::new_err)
}

#[pyfunction]
fn score_post_process(spec_json: String, buckets: Vec<Vec<String>>, raw: Vec<i64>) -> PyResult<Vec<i64>> {
    score::score_post_process(&spec_json, &buckets, &raw).map_err(PyValueError::new_err)
}

#[pyfunction]
fn command_prefixes(py: Python<'_>, commands: Vec<String>) -> Vec<Vec<String>> {
    py.detach(|| commands.par_iter().map(|c| command::prefixes(c)).collect())
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
        human_facing_tools: human_facing_tools.map_or(defaults.human_facing_tools, HashSet::from_iter),
    };
    let activity = py.detach(|| -> PyResult<SessionActivity> {
        let bytes = std::fs::read(&path)?;
        Ok(session_activity(&parse_bytes(&bytes, None)?, &opts))
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
#[pyo3(signature = (raw, spec_json))]
fn mine_signals<'py>(
    py: Python<'py>,
    raw: &[u8],
    spec_json: String,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let spec = mining::compile_spec(&spec_json).map_err(PyValueError::new_err)?;
    mining::mine(py, raw, &spec)
}

#[pymodule]
fn _parser_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(stream_parse, m)?)?;
    m.add_function(wrap_pyfunction!(parse_print_result, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_available, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_polarity, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_has_hit, m)?)?;
    m.add_function(wrap_pyfunction!(lexicon_overrides, m)?)?;
    m.add_function(wrap_pyfunction!(score_short_circuit, m)?)?;
    m.add_function(wrap_pyfunction!(score_post_process, m)?)?;
    m.add_function(wrap_pyfunction!(command_prefixes, m)?)?;
    m.add_function(wrap_pyfunction!(mine_signals, m)?)?;
    m.add_function(wrap_pyfunction!(session_activity_probe, m)?)?;
    m.add_class::<ParseStream>()?;
    Ok(())
}
