use chrono::Datelike;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use cc_transcript_core::activity::lift_session;
use cc_transcript_core::context::{capture_window, ContextWindow};
use cc_transcript_core::gateway::parse_transcript_bytes;
use cc_transcript_core::ids::EventRef;
use cc_transcript_core::types::Entry;

use crate::views::convert::parse_err;

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
#[pyo3(signature = (raw, session_id, anchor_uuid, anchor_tool_use_id, before, after, preview_chars))]
pub(crate) fn context_capture_window(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "bytes"))] raw: &[u8],
    session_id: &str,
    anchor_uuid: &str,
    anchor_tool_use_id: Option<&str>,
    before: usize,
    after: usize,
    preview_chars: i64,
) -> PyResult<String> {
    py.detach(|| {
        let entries = parse_transcript_bytes(raw).map_err(parse_err)?.entries;
        // Mirror the materialization drop: a year-0000 timestamp has no Python
        // datetime, so from_events never sees it (activity_lift does the same).
        let capped: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| entry.meta().is_none_or(|m| m.timestamp.year() >= 1))
            .collect();
        let lift = lift_session(session_id, &capped);
        let anchor = EventRef {
            session_id: session_id.to_string(),
            event_uuid: anchor_uuid.to_string(),
            tool_use_id: anchor_tool_use_id.map(str::to_string),
        };
        capture_window(&lift, &anchor, before, after, preview_chars)
            .map(|window| window.to_json())
            .map_err(PyValueError::new_err)
    })
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn context_roundtrip(data: &str) -> PyResult<String> {
    ContextWindow::from_json(data)
        .map(|window| window.to_json())
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn context_render_preview(data: &str, turn_chars: usize) -> PyResult<String> {
    ContextWindow::from_json(data)
        .map(|window| window.render_preview(turn_chars))
        .map_err(|e| PyValueError::new_err(e.to_string()))
}
