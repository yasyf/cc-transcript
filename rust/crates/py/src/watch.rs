//! pyo3 exposure of the core live-tail cursor: `WatchTailer` drives one core
//! `tick` per poll, materializing each appended entry into a Python event, and
//! snapshots its cursor state so the parity suite can compare against the
//! Python `TailState`.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use pyo3::IntoPyObjectExt;
use std::path::PathBuf;

use crate::views::events::event_view;
use cc_transcript_core::watch::{tick, TailState};
use std::sync::Arc;

/// A stateful tail over the transcript tree: one `tick` per poll, holding the
/// per-file cursors between calls. The Python facade wraps it in the async
/// poll-forever loop.
#[pyclass]
pub struct WatchTailer {
    state: TailState,
}

#[pymethods]
impl WatchTailer {
    #[new]
    fn new() -> Self {
        WatchTailer {
            state: TailState::default(),
        }
    }

    /// Run one poll step over `roots`, returning `(path, session_id,
    /// is_sidechain, event)` tuples for each freshly appended entry.
    #[pyo3(signature = (roots, from_start=false))]
    fn tick<'py>(
        &mut self,
        py: Python<'py>,
        roots: Vec<String>,
        from_start: bool,
    ) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let roots: Vec<PathBuf> = roots.into_iter().map(PathBuf::from).collect();
        let events = py.detach(|| tick(&mut self.state, &roots, from_start));
        events
            .into_iter()
            .map(|event| {
                let path = event.path.to_string_lossy().into_owned();
                Ok(PyList::new(
                    py,
                    [
                        path.into_bound_py_any(py)?,
                        event.session_id.as_str().into_bound_py_any(py)?,
                        event.is_sidechain.into_bound_py_any(py)?,
                        event_view(py, &Arc::new(vec![event.event]), 0)?,
                    ],
                )?
                .into_any())
            })
            .collect()
    }

    /// The whole cursor state: `{"primed": bool, "cursors": {path: {offset,
    /// size, mtime, session_id, seen}}}`, the Python `TailState` projection.
    fn snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let cursors = PyDict::new(py);
        for (path, cursor) in &self.state.cursors {
            let projected = PyDict::new(py);
            projected.set_item("offset", cursor.offset)?;
            projected.set_item("size", cursor.size)?;
            projected.set_item("mtime", cursor.mtime)?;
            projected.set_item("session_id", cursor.session_id.as_deref())?;
            projected.set_item("seen", cursor.seen().collect::<Vec<_>>())?;
            cursors.set_item(path.to_string_lossy().as_ref(), projected)?;
        }
        let out = PyDict::new(py);
        out.set_item("primed", self.state.primed)?;
        out.set_item("cursors", cursors)?;
        Ok(out)
    }
}
