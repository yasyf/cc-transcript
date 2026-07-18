use std::collections::{HashMap, HashSet};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use sonic_rs::Value;

use cc_transcript_core::activity::Turn;
use cc_transcript_core::parse::parse_bytes;
use cc_transcript_core::render::{self, Budget};
use cc_transcript_core::toolcall::parse_tool_call;
use cc_transcript_core::types::{tool_use_index, Entry};
use cc_transcript_core::value::normalize_last_wins;

use crate::mining::view_entry;
use crate::views::convert::parse_err;
use crate::views::toolcall::ToolCallBaseView;

// The tool_use_id -> tool name join the renderer keys on (filterspec.tool_names).
fn tool_names(entries: &[Entry]) -> HashMap<&str, &str> {
    tool_use_index(entries)
        .into_iter()
        .map(|(id, block)| (id, block.name.as_str()))
        .collect()
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_tool_call(
    name: &str,
    input_json: &str,
    turn_chars: usize,
    tool_chars: usize,
) -> PyResult<String> {
    let mut input: Value = sonic_rs::from_str(input_json)
        .map_err(|e| PyValueError::new_err(format!("invalid JSON: {e}")))?;
    // This standalone gateway bypasses parse_entry's dedup, so normalize before parse_tool_call.
    normalize_last_wins(&mut input);
    Ok(render::render_tool_call(
        &parse_tool_call(name, &input),
        &Budget {
            turn_chars,
            tool_chars,
        },
    ))
}

// render.render_tool_call over an already-parsed typed call view (no re-parse); mirrors
// render_tool_call's numeric budget.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_tool_call_view(
    #[gen_stub(override_type(type_repr = "cc_transcript.tools.ToolCall", imports = ("cc_transcript.tools",)))]
    call: &Bound<'_, PyAny>,
    turn_chars: usize,
    tool_chars: usize,
) -> PyResult<String> {
    Ok(render::render_tool_call(
        &call.cast::<ToolCallBaseView>()?.get().call,
        &Budget {
            turn_chars,
            tool_chars,
        },
    ))
}

// render.render_turn over view_entry-borrowed events; render_turn reads only prompt + events.
#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_turn_from_events(
    prompt: String,
    #[gen_stub(override_type(type_repr = "list[cc_transcript.models.TranscriptEvent]", imports = ("cc_transcript.models",)))]
    events: Vec<Bound<'_, PyAny>>,
    turn_chars: usize,
    tool_chars: usize,
) -> PyResult<String> {
    let entries = events
        .iter()
        .map(|event| view_entry(event, "render_turn_from_events"))
        .collect::<PyResult<Vec<_>>>()?;
    let turn = Turn {
        index: 0,
        prompt,
        started_at: None,
        ended_at: None,
        events: entries,
        tool_uses: Vec::new(),
    };
    Ok(render::render_turn(
        &turn,
        &Budget {
            turn_chars,
            tool_chars,
        },
    ))
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_compact_lines(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "bytes"))] raw: &[u8],
    width: usize,
    thinking: bool,
    uuids: bool,
) -> PyResult<Vec<String>> {
    py.detach(|| {
        let entries = parse_bytes(raw, |_| true).map_err(parse_err)?;
        let names = tool_names(&entries);
        Ok(entries
            .iter()
            .enumerate()
            .map(|(i, event)| render::compact_line(i, event, &names, width, thinking, uuids))
            .collect())
    })
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_haystacks(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "bytes"))] raw: &[u8],
    wheres: Vec<String>,
) -> PyResult<Vec<String>> {
    py.detach(|| {
        let entries = parse_bytes(raw, |_| true).map_err(parse_err)?;
        let where_set: HashSet<String> = wheres.into_iter().collect();
        let (text, thinking, tools) = (
            where_set.contains("text"),
            where_set.contains("thinking"),
            where_set.contains("tools"),
        );
        Ok(entries
            .iter()
            .map(|event| render::haystack(event, text, thinking, tools))
            .collect())
    })
}

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_stats(
    py: Python<'_>,
    #[gen_stub(override_type(type_repr = "list[bytes]"))] raws: Vec<Vec<u8>>,
) -> PyResult<String> {
    py.detach(|| {
        let transcripts: Vec<Vec<Entry>> = raws
            .iter()
            .map(|raw| parse_bytes(raw, |_| true))
            .collect::<Result<_, _>>()
            .map_err(parse_err)?;
        Ok(render::render_stats(&render::collect_stats(&transcripts)))
    })
}
