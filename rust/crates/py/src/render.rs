use std::collections::{HashMap, HashSet};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use sonic_rs::Value;

use cc_transcript_core::gateway::parse_transcript_bytes;
use cc_transcript_core::render::{self, Budget};
use cc_transcript_core::toolcall::{parse_tool_call, ToolCall};
use cc_transcript_core::types::{tool_use_index, ContentBlock, Entry};
use cc_transcript_core::value::normalize_last_wins;

use crate::mining::view_entry;
use crate::views::convert::parse_err;
use crate::views::events::AssistantEventView;
use crate::views::store::with_view_registry;
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

#[pyo3_stub_gen::derive::gen_stub_pyfunction]
#[pyfunction]
pub(crate) fn render_turn_from_events(
    prompt: String,
    #[gen_stub(override_type(type_repr = "list[cc_transcript.models.TranscriptEvent]", imports = ("cc_transcript.models",)))]
    events: Vec<Bound<'_, PyAny>>,
    turn_chars: usize,
    tool_chars: usize,
    tool_results: bool,
) -> PyResult<String> {
    let entries = events
        .iter()
        .map(|event| view_entry(event, "render_turn_from_events"))
        .collect::<PyResult<Vec<_>>>()?;
    let calls = ordered_tool_calls(&events, &entries)?;
    Ok(render::render_turn_parts(
        &prompt,
        &entries,
        &calls.iter().collect::<Vec<_>>(),
        &Budget {
            turn_chars,
            tool_chars,
        },
        tool_results,
    ))
}

fn ordered_tool_calls(events: &[Bound<'_, PyAny>], entries: &[&Entry]) -> PyResult<Vec<ToolCall>> {
    let mut calls = Vec::new();
    for (event, entry) in events.iter().zip(entries) {
        if let Entry::Assistant(assistant) = entry {
            let view = event.cast::<AssistantEventView>()?.get();
            with_view_registry(&view.r.registry, || {
                for block in &assistant.blocks {
                    if let ContentBlock::ToolUse(tool_use) = block {
                        calls.push(parse_tool_call(&tool_use.name, &tool_use.input));
                    }
                }
            });
        }
    }
    Ok(calls)
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
        let entries = parse_transcript_bytes(raw).map_err(parse_err)?.entries;
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
        let entries = parse_transcript_bytes(raw).map_err(parse_err)?.entries;
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
            .map(|raw| parse_transcript_bytes(raw).map(|p| p.entries))
            .collect::<Result<_, _>>()
            .map_err(parse_err)?;
        Ok(render::render_stats(&render::collect_stats(&transcripts)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cc_transcript_core::toolcall::{
        with_registry, McpToolSpec, SpanEditMap, ToolRegistrySnapshot,
    };
    use std::sync::Arc;

    fn event<'py>(py: Python<'py>, content: Value) -> Bound<'py, PyAny> {
        let source = sonic_rs::json!({
            "type":"assistant", "uuid":"a", "sessionId":"s", "timestamp":"2026-01-02T03:04:05Z",
            "message":{"role":"assistant","model":"claude","content":content}
        })
        .to_string();
        let entries = Arc::new(parse_transcript_bytes(source.as_bytes()).unwrap().entries);
        crate::views::events::event_view(py, &entries, 0).unwrap()
    }

    #[test]
    fn event_only_turn_preserves_prose_and_typed_call_order() {
        Python::initialize();
        Python::attach(|py| {
            let event = event(
                py,
                sonic_rs::json!([
                    {"type":"text","text":"editing"},
                    {"type":"tool_use","id":"t1","name":"Edit","input":{
                        "file_path":"/a.py","old_string":"x = 1","new_string":"x = 2"
                    }},
                    {"type":"text","text":"checking"},
                    {"type":"tool_use","id":"t2","name":"Bash","input":{"command":"echo done"}},
                    {"type":"text","text":"done"}
                ]),
            );
            assert_eq!(
                render_turn_from_events("fix the bug".to_string(), vec![event], 700, 1500, false).unwrap(),
                "user: fix the bug\nassistant: editing\nEdit /a.py\n- x = 1\n+ x = 2\nassistant: checking\necho done\nassistant: done"
            );
        });
    }

    #[test]
    fn event_only_turn_renders_malformed_known_tool_as_raw_input() {
        Python::initialize();
        Python::attach(|py| {
            let input = sonic_rs::json!({"file_path":"/a.py"});
            let event = event(
                py,
                sonic_rs::json!([
                    {"type":"tool_use","id":"t","name":"Edit","input":input.clone()}
                ]),
            );
            let rendered =
                render_turn_from_events(String::new(), vec![event], 700, 1500, false).unwrap();
            let raw = rendered
                .strip_prefix("Edit(")
                .unwrap()
                .strip_suffix(')')
                .unwrap();
            assert_eq!(sonic_rs::from_str::<Value>(raw).unwrap(), input);
        });
    }

    #[test]
    fn event_only_turn_keeps_captured_registry_for_lazy_calls() {
        Python::initialize();
        Python::attach(|py| {
            let pinned = ToolRegistrySnapshot::from_specs(HashMap::from([(
                "syn_render_pinned".to_string(),
                McpToolSpec {
                    behaves_like: "Edit".to_string(),
                    span_edit: Some(SpanEditMap {
                        path: "path".to_string(),
                        content: "content".to_string(),
                        delete: None,
                    }),
                },
            )]));
            let input = sonic_rs::json!({"path":"a.py","content":"pinned"});
            let event = with_registry(pinned, || {
                event(
                    py,
                    sonic_rs::json!([
                        {"type":"tool_use","id":"t","name":"mcp__fixture__syn_render_pinned",
                         "input":input.clone()}
                    ]),
                )
            });
            with_registry(ToolRegistrySnapshot::from_specs(HashMap::new()), || {
                let events = vec![event];
                let entries = events
                    .iter()
                    .map(|event| view_entry(event, "test").unwrap())
                    .collect::<Vec<_>>();
                let calls = ordered_tool_calls(&events, &entries).unwrap();
                assert!(matches!(&calls[0], ToolCall::SpanEdit(_)));
                let rendered =
                    render_turn_from_events(String::new(), events, 700, 1500, false).unwrap();
                let raw = rendered
                    .strip_prefix("mcp__fixture__syn_render_pinned(")
                    .unwrap()
                    .strip_suffix(')')
                    .unwrap();
                assert_eq!(sonic_rs::from_str::<Value>(raw).unwrap(), input);
            });
        });
    }
}
