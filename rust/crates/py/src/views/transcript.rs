use std::sync::Arc;

use pyo3::exceptions::{PyIndexError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyList, PySlice, PyTuple};
use pyo3::IntoPyObjectExt;

use cc_transcript_core::types::Entry;

use crate::views::dunder::frozen_copy;
use crate::views::events::event_view;

/// The parsed events of a single transcript file, backed by the native parse
/// output; ``events`` materializes lazy views on access.
///
/// One parse owns one ``Arc<Vec<Entry>>`` that every event and nested view shares,
/// so retaining any single view keeps that whole parse's entries alive; keeping one
/// event from each of several parses retains all of their entries. The live
/// :class:`~cc_transcript.watch.WatchEvent` stream is exempt — its tailer builds a
/// one-entry Arc per yielded event, so a held watch event pins only itself.
///
/// Attributes:
///     path: The transcript's path on disk.
///     mtime: The transcript's modification time when parsed.
///     events: The parsed events, in file order.
#[pyclass(name = "Transcript", module = "cc_transcript", frozen)]
pub(crate) struct TranscriptView {
    pub path: String,
    pub mtime: f64,
    pub entries: Arc<Vec<Entry>>,
}

#[pymethods]
impl TranscriptView {
    #[getter]
    fn path(&self) -> &str {
        &self.path
    }

    #[getter]
    fn mtime(&self) -> f64 {
        self.mtime
    }

    #[getter]
    fn events(&self) -> EventListView {
        EventListView {
            entries: Arc::clone(&self.entries),
        }
    }
}

/// A lazily-materializing sequence of transcript events over one parse output.
///
/// Implements the immutable ``collections.abc.Sequence`` interface — indexing,
/// slicing, iteration, ``len``, ``in``, :func:`reversed`, ``index``, and
/// ``count`` — and is registered as a virtual ``Sequence``. ``copy()`` returns a
/// plain ``list``, and ``==`` is elementwise against any list, tuple, or
/// ``EventList`` (``list(events) == events``). Views materialize fresh on each
/// access, so identity across accesses is not guaranteed by design
/// (``events[0] is events[0]`` is False). ``copy.copy`` and ``copy.deepcopy``
/// return the immutable list itself; pickle is unsupported. Like the
/// :class:`Transcript` it comes from, retaining any event pins the whole parse.
#[pyclass(name = "EventList", module = "cc_transcript", frozen)]
pub(crate) struct EventListView {
    pub entries: Arc<Vec<Entry>>,
}

#[pymethods]
impl EventListView {
    fn __len__(&self) -> usize {
        self.entries.len()
    }

    fn __getitem__<'py>(
        &self,
        py: Python<'py>,
        index: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Ok(i) = index.extract::<isize>() {
            let n = self.entries.len() as isize;
            let idx = if i < 0 { i + n } else { i };
            if idx < 0 || idx >= n {
                return Err(PyIndexError::new_err("EventList index out of range"));
            }
            return event_view(py, &self.entries, idx as usize);
        }
        if let Ok(slice) = index.cast::<PySlice>() {
            let indices = slice.indices(self.entries.len() as isize)?;
            let mut views = Vec::with_capacity(indices.slicelength);
            let mut i = indices.start;
            for _ in 0..indices.slicelength {
                views.push(event_view(py, &self.entries, i as usize)?);
                i += indices.step;
            }
            return Ok(PyList::new(py, views)?.into_any());
        }
        Err(PyTypeError::new_err(
            "EventList indices must be integers or slices",
        ))
    }

    fn __reversed__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let views = (0..self.entries.len())
            .rev()
            .map(|i| event_view(py, &self.entries, i))
            .collect::<PyResult<Vec<_>>>()?;
        Ok(PyList::new(py, views)?.into_any().try_iter()?.into_any())
    }

    #[pyo3(signature = (value, start=0, stop=None))]
    fn index(
        &self,
        py: Python<'_>,
        value: &Bound<'_, PyAny>,
        start: isize,
        stop: Option<isize>,
    ) -> PyResult<usize> {
        let n = self.entries.len() as isize;
        let clamp = |i: isize| (if i < 0 { (i + n).max(0) } else { i.min(n) }) as usize;
        let (lo, hi) = (clamp(start), clamp(stop.unwrap_or(n)));
        for i in lo..hi {
            if event_view(py, &self.entries, i)?.eq(value)? {
                return Ok(i);
            }
        }
        Err(PyValueError::new_err("value is not in EventList"))
    }

    fn count(&self, py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<usize> {
        let mut hits = 0;
        for i in 0..self.entries.len() {
            if event_view(py, &self.entries, i)?.eq(value)? {
                hits += 1;
            }
        }
        Ok(hits)
    }

    fn copy<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let views = (0..self.entries.len())
            .map(|i| event_view(py, &self.entries, i))
            .collect::<PyResult<Vec<_>>>()?;
        PyList::new(py, views)
    }

    fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let is_sequence = other.is_instance_of::<PyList>()
            || other.is_instance_of::<PyTuple>()
            || other.is_instance_of::<EventListView>();
        if !is_sequence {
            return Ok(py.NotImplemented());
        }
        if other.len()? != self.entries.len() {
            return false.into_py_any(py);
        }
        for i in 0..self.entries.len() {
            if !event_view(py, &self.entries, i)?.eq(&other.get_item(i)?)? {
                return false.into_py_any(py);
            }
        }
        true.into_py_any(py)
    }
}

frozen_copy!(EventListView);
frozen_copy!(TranscriptView);
