use std::collections::HashMap;

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
    Ok(context_capture_windows(
        py,
        raw,
        vec![(
            session_id.to_string(),
            anchor_uuid.to_string(),
            anchor_tool_use_id.map(str::to_string),
        )],
        before,
        after,
        preview_chars,
    )?
    .remove(0))
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn context_capture_windows(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "bytes"))] raw: &[u8],
    anchors: Vec<(String, String, Option<String>)>,
    before: usize,
    after: usize,
    preview_chars: i64,
) -> PyResult<Vec<String>> {
    if anchors.is_empty() {
        return Ok(Vec::new());
    }
    py.detach(|| {
        let entries = parse_transcript_bytes(raw).map_err(parse_err)?.entries;
        // Mirror activity_lift: Python datetime cannot represent year zero.
        let capped: Vec<Entry> = entries
            .into_iter()
            .filter(|entry| entry.meta().is_none_or(|m| m.timestamp.year() >= 1))
            .collect();
        let mut sessions = HashMap::new();
        anchors
            .iter()
            .map(|(session_id, event_uuid, tool_use_id)| {
                let lift = sessions
                    .entry(session_id.as_str())
                    .or_insert_with(|| lift_session(session_id, &capped));
                capture_window(
                    lift,
                    &EventRef {
                        session_id: session_id.clone(),
                        event_uuid: event_uuid.clone(),
                        tool_use_id: tool_use_id.clone(),
                    },
                    before,
                    after,
                    preview_chars,
                )
                .map(|window| window.to_json())
                .map_err(PyValueError::new_err)
            })
            .collect()
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
