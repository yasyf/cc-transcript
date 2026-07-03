mod event;
mod filter;
mod lexicon;
mod mining;
mod model;
mod protocol;
mod score;
mod value;

use crossbeam_channel::{bounded, Receiver};
use memchr::memchr_iter;
use once_cell::sync::Lazy;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyFloat, PyList, PyString, PyTuple};
use rayon::prelude::*;
use sonic_rs::Value;
use std::sync::Arc;
use std::thread;

use crate::event::{build_event, build_print_result};
use crate::filter::{compile_spec, spec_keep, CompiledSpec};

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
    lines: Vec<Value>,
}

fn parse_line(line: &[u8], lines: &mut Vec<Value>, filter: Option<&CompiledSpec>) {
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    if let Ok(value) = sonic_rs::from_slice::<Value>(line) {
        if filter.map_or(true, |spec| spec_keep(spec, &value)) {
            lines.push(value);
        }
    }
}

fn parse_file_internal(
    path: &str,
    mtime: f64,
    filter: Option<&CompiledSpec>,
) -> std::io::Result<ParsedFile> {
    let bytes = std::fs::read(path)?;
    let mut lines: Vec<Value> = Vec::with_capacity(bytes.len() / AVG_LINE_BYTES + 1);
    let mut start = 0usize;
    for pos in memchr_iter(b'\n', &bytes) {
        parse_line(&bytes[start..pos], &mut lines, filter);
        start = pos + 1;
    }
    if start < bytes.len() {
        parse_line(&bytes[start..], &mut lines, filter);
    }
    Ok(ParsedFile {
        path: path.to_string(),
        mtime,
        lines,
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
    // A file whose events cannot be materialized (e.g. a malformed line missing a
    // required field) is silently skipped — whole-file parity with PythonBackend.
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
                    if let Ok(pf) = parse_file_internal(&path, mtime, filter.as_deref()) {
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
    build_print_result(py, &value)
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
    m.add_function(wrap_pyfunction!(mine_signals, m)?)?;
    m.add_class::<ParseStream>()?;
    Ok(())
}
