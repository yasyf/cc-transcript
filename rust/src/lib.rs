mod event;
mod model;

use crossbeam_channel::{bounded, Receiver};
use memchr::memchr_iter;
use once_cell::sync::Lazy;
use pyo3::prelude::*;
use pyo3::types::{PyFloat, PyList, PyString, PyTuple};
use rayon::prelude::*;
use sonic_rs::Value;
use std::thread;

use crate::event::build_event;

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

fn parse_line(line: &[u8], lines: &mut Vec<Value>) {
    if line.iter().all(u8::is_ascii_whitespace) {
        return;
    }
    if let Ok(value) = sonic_rs::from_slice::<Value>(line) {
        lines.push(value);
    }
}

fn parse_file_internal(path: &str, mtime: f64) -> std::io::Result<ParsedFile> {
    let bytes = std::fs::read(path)?;
    let mut lines: Vec<Value> = Vec::with_capacity(bytes.len() / AVG_LINE_BYTES + 1);
    let mut start = 0usize;
    for pos in memchr_iter(b'\n', &bytes) {
        parse_line(&bytes[start..pos], &mut lines);
        start = pos + 1;
    }
    if start < bytes.len() {
        parse_line(&bytes[start..], &mut lines);
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
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyAny>>> {
        match py.detach(|| self.rx.recv().ok()) {
            None => Ok(None),
            Some(pf) => Ok(Some(parsed_file_to_py(py, pf)?)),
        }
    }

    fn recv_many<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Vec<Bound<'py, PyAny>>> {
        py.detach(|| {
            let mut out: Vec<ParsedFile> = Vec::new();
            if let Ok(pf) = self.rx.recv() {
                out.push(pf);
                while out.len() < max {
                    match self.rx.try_recv() {
                        Ok(pf) => out.push(pf),
                        Err(_) => break,
                    }
                }
            }
            out
        })
        .into_iter()
        .map(|pf| parsed_file_to_py(py, pf))
        .collect()
    }
}

#[pyfunction]
fn stream_parse(paths: Vec<(String, f64)>, prefetch: usize) -> PyResult<ParseStream> {
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
                    if let Ok(pf) = parse_file_internal(&path, mtime) {
                        let _ = tx.send(pf);
                    }
                    let _ = permits_tx.send(());
                },
            );
        });
    });
    Ok(ParseStream { rx })
}

#[pymodule]
fn _parser_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(stream_parse, m)?)?;
    m.add_class::<ParseStream>()?;
    Ok(())
}
