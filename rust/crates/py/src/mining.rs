use std::sync::Arc;

use chrono::{DateTime, Datelike, FixedOffset};
use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyIterator, PyString};

use cc_transcript_core::activity::{lift_session, sample_refs};
use cc_transcript_core::mining::{
    self, CallableReviewFormat, MiningError, MiningResult, MiningSignal, ReviewComment,
};
use cc_transcript_core::parse::parse_bytes;
use cc_transcript_core::snapshot::{
    Cancellation, SnapshotError, Status, TranscriptSnapshot, WorkLimits,
};
use cc_transcript_core::types::Entry;

use crate::views::convert::{json_to_py, parse_err};

pub use cc_transcript_core::mining::{compile_spec, CompiledMiningSpec, MiningUsage};

pub(crate) fn error(error: MiningError) -> PyErr {
    match error {
        MiningError::Snapshot(error) => crate::snapshots::error(error),
        MiningError::Invalid(reason) => PyValueError::new_err(reason),
        MiningError::Callback(error) => match error.downcast::<PyErr>() {
            Ok(error) => *error,
            Err(error) => PyRuntimeError::new_err(error.to_string()),
        },
    }
}

struct CompiledCallableFormat {
    name: String,
    pattern: Py<PyAny>,
    extract: Py<PyAny>,
    bounded: bool,
}

impl CallableReviewFormat for CompiledCallableFormat {
    fn name(&self) -> &str {
        &self.name
    }

    fn fingerprint(&self) -> [u8; 32] {
        let mut fingerprint = [0; 32];
        let width = std::mem::size_of::<usize>();
        fingerprint[..width].copy_from_slice(&(self.pattern.as_ptr() as usize).to_le_bytes());
        fingerprint[width..2 * width]
            .copy_from_slice(&(self.extract.as_ptr() as usize).to_le_bytes());
        fingerprint
    }

    fn bounded(&self) -> bool {
        self.bounded
    }

    fn visit(
        &self,
        text: &str,
        bounded: bool,
        preflight: &dyn Fn(usize) -> MiningResult<()>,
        emit: &mut dyn FnMut(ReviewComment) -> MiningResult<()>,
    ) -> MiningResult<()> {
        Python::attach(|py| -> PyResult<()> {
            preflight(0).map_err(error)?;
            if self
                .pattern
                .bind(py)
                .call_method1("search", (text,))?
                .is_none()
            {
                return Ok(());
            }
            let extracted = self.extract.bind(py).call1((text,))?;
            if bounded && extracted.cast::<PyIterator>().is_err() {
                return Err(crate::snapshots::error(SnapshotError::new(
                    Status::Incomplete,
                    format!(
                        "bounded mining callback {} must return an iterator",
                        self.name
                    ),
                )));
            }
            let mut iterator = extracted.try_iter()?;
            loop {
                preflight(0).map_err(error)?;
                let Some(item) = iterator.next() else { break };
                let item = item?;
                let comment = item.getattr("comment")?;
                let file = item.getattr("file")?;
                let bytes = comment
                    .cast::<PyString>()?
                    .to_str()?
                    .len()
                    .saturating_add(if file.is_none() {
                        0
                    } else {
                        file.cast::<PyString>()?.to_str()?.len()
                    })
                    .saturating_add(self.name.len());
                preflight(4096usize.saturating_add(bytes.saturating_mul(6))).map_err(error)?;
                emit(ReviewComment {
                    file: file.extract()?,
                    line_start: item.getattr("line_start")?.extract()?,
                    line_end: item.getattr("line_end")?.extract()?,
                    comment: comment.extract()?,
                })
                .map_err(error)?;
            }
            Ok(())
        })
        .map_err(|error| MiningError::Callback(Box::new(error)))
    }
}

pub fn attach_callable_formats(
    spec: &mut CompiledMiningSpec,
    formats: Vec<(String, Py<PyAny>, Py<PyAny>)>,
) {
    mining::set_callable_formats(
        spec,
        formats
            .into_iter()
            .map(|(name, pattern, extract)| {
                Arc::new(CompiledCallableFormat {
                    name,
                    pattern,
                    extract,
                    bounded: false,
                }) as Arc<dyn CallableReviewFormat>
            })
            .collect(),
    );
}

pub fn attach_bounded_callable_formats(
    spec: &mut CompiledMiningSpec,
    formats: Vec<(String, Py<PyAny>, Py<PyAny>, bool)>,
) -> PyResult<()> {
    let formats = formats
        .into_iter()
        .map(|(name, pattern, extract, bounded)| {
            if !bounded {
                return Err(crate::snapshots::error(SnapshotError::new(
                    Status::Incomplete,
                    format!("unbounded mining callback: {name}"),
                )));
            }
            Ok(Arc::new(CompiledCallableFormat {
                name,
                pattern,
                extract,
                bounded,
            }) as Arc<dyn CallableReviewFormat>)
        })
        .collect::<PyResult<Vec<_>>>()?;
    mining::set_callable_formats(spec, formats);
    Ok(())
}

pub(crate) fn build_signal_dict<'py>(
    py: Python<'py>,
    row: &MiningSignal,
) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    d.set_item("kind", &row.kind)?;
    d.set_item("detector", &row.detector)?;
    d.set_item("session_id", &row.session_id)?;
    d.set_item("event_index", row.event_index as i64)?;
    d.set_item("event_uuid", &row.event_uuid)?;
    d.set_item("occurred_at", occurred_at_iso(py, row.occurred_at)?)?;
    d.set_item("text", &row.text)?;
    d.set_item("cc_version", row.cc_version.as_deref())?;
    d.set_item("trigger_index", row.trigger_index)?;
    let signal = PyDict::new(py);
    signal.set_item("confidence", row.signal.confidence)?;
    signal.set_item("reasons", &row.signal.reasons)?;
    signal.set_item("durable", row.signal.durable)?;
    d.set_item("signal", signal)?;
    d.set_item("lower_bound", row.lower_bound)?;
    let evidence = PyDict::new(py);
    for (key, value) in &row.evidence.fields {
        evidence.set_item(key, json_to_py(py, value)?)?;
    }
    d.set_item("evidence", evidence)?;
    Ok(d)
}

fn occurred_at_iso<'py>(py: Python<'py>, dt: DateTime<FixedOffset>) -> PyResult<Bound<'py, PyAny>> {
    dt.into_pyobject(py)?.call_method0("isoformat")
}

struct PythonSignalSink<'py> {
    py: Python<'py>,
    rows: Vec<Bound<'py, PyDict>>,
}

impl mining::SignalSink for PythonSignalSink<'_> {
    fn push(&mut self, row: MiningSignal) -> MiningResult<()> {
        let row = build_signal_dict(self.py, &row)
            .map_err(|error| MiningError::Callback(Box::new(error)))?;
        self.rows.push(row);
        Ok(())
    }
}

pub fn mine_events<'py>(
    py: Python<'py>,
    events: &[Bound<'py, PyAny>],
    spec: &CompiledMiningSpec,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let entries = events
        .iter()
        .map(|event| view_entry(event, "mine"))
        .collect::<PyResult<Vec<_>>>()?;
    let mut sink = PythonSignalSink {
        py,
        rows: Vec::new(),
    };
    mining::mine_entries_into(entries, spec, &mut sink).map_err(error)?;
    Ok(sink.rows)
}

pub fn mine_snapshot<'py>(
    py: Python<'py>,
    snapshot: &TranscriptSnapshot,
    spec: &CompiledMiningSpec,
    limits: WorkLimits,
    cancel: &Cancellation,
    usage: &mut MiningUsage,
) -> PyResult<Vec<Bound<'py, PyDict>>> {
    let prepared = py
        .detach(|| mining::prepare_snapshot(snapshot, spec, limits, cancel, usage))
        .map_err(error)?;
    let mut sink = PythonSignalSink {
        py,
        rows: Vec::new(),
    };
    mining::mine_prepared_into(prepared, spec, usage, &mut sink).map_err(error)?;
    Ok(sink.rows)
}

/// Borrows the shared `Entry` behind a native event view — the no-round-trip
/// handle mining and the filter bindings run on.
pub(crate) fn view_entry<'a>(event: &'a Bound<'_, PyAny>, caller: &str) -> PyResult<&'a Entry> {
    use crate::views::events::{
        AssistantEventView, AttachmentEventView, ModeEventView, OtherEventView, SystemEventView,
        UserEventView,
    };
    if let Ok(view) = event.cast::<UserEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<AssistantEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<SystemEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<ModeEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<OtherEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    if let Ok(view) = event.cast::<AttachmentEventView>() {
        let r = &view.get().r;
        return Ok(&r.entries[r.idx]);
    }
    Err(PyTypeError::new_err(format!(
        "{caller}() takes parsed transcript events, got {}",
        event.get_type().name()?
    )))
}

/// The seeded negative-window draw over a transcript's completed turns
/// (mining/sampling.py sample_windows candidate build + draw): returns each drawn
/// anchor as `(turn_index, session_id, event_uuid)`, sorted by turn index.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (data, n, exclude, exclusion_radius, seed))]
pub(crate) fn mining_sample_refs(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "bytes"))] data: &[u8],
    n: usize,
    exclude: Vec<(String, String)>,
    exclusion_radius: usize,
    seed: String,
) -> PyResult<Vec<(usize, String, String)>> {
    py.detach(|| {
        let entries = parse_bytes(data, |_| true).map_err(parse_err)?;
        // Mirror the materialization drop: a year-0000 timestamp has no Python
        // datetime, so parse_events_from_bytes never yields it (context.rs does the same).
        let capped: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| entry.meta().is_none_or(|meta| meta.timestamp.year() >= 1))
            .collect();
        let session_id = capped
            .iter()
            .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()))
            .ok_or_else(|| PyValueError::new_err("transcript has no session id"))?;
        let lift = lift_session(&session_id, &capped);
        Ok(sample_refs(&lift, n, &exclude, exclusion_radius, &seed))
    })
}
