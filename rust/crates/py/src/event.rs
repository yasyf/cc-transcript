use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use pyo3::IntoPyObjectExt;
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::model::{
    models_type, API_ERROR_CLS, ASSISTANT_EVENT_CLS, ASYNC_HOOK_RESPONSE_CLS, ATTACHMENT_EVENT_CLS,
    ATTRIBUTION_CLS, CACHE_CREATION_CLS, COMPACT_BOUNDARY_CLS, ENTRY_META_CLS, FALLBACK_BLOCK_CLS,
    HOOK_ADDITIONAL_CONTEXT_CLS, HOOK_BLOCKING_ERROR_CLS, HOOK_CANCELLED_CLS, HOOK_INFO_CLS,
    HOOK_NON_BLOCKING_ERROR_CLS, HOOK_SUCCESS_CLS, INIT_INFO_CLS, MCP_SERVER_CLS,
    MODEL_REFUSAL_FALLBACK_CLS, MODEL_USAGE_CLS, MODE_EVENT_CLS, OTHER_ATTACHMENT_CLS,
    OTHER_BLOCK_CLS, OTHER_EVENT_CLS, OTHER_SYSTEM_DETAIL_CLS, PLUGIN_CLS, PRESERVED_MESSAGES_CLS,
    PRESERVED_SEGMENT_CLS, PRINT_MESSAGE_CLS, PRINT_RESULT_CLS, QUEUED_COMMAND_CLS,
    SERVER_TOOL_USE_CLS, STOP_HOOK_SUMMARY_CLS, SYSTEM_EVENT_CLS, TEXT_BLOCK_CLS,
    THINKING_BLOCK_CLS, TOOL_RESULT_BLOCK_CLS, TOOL_USE_BLOCK_CLS, TURN_DURATION_CLS, USAGE_CLS,
    USER_EVENT_CLS,
};
use cc_transcript_core::parse::ParseError;
use cc_transcript_core::protocol::{interrupt_marker, is_agent_injection};
use cc_transcript_core::types::{
    joined_text, ApiError, AttachmentDetail, Attribution, ContentBlock, Entry, EntryMeta, InitInfo,
    ModelUsage, PrintBody, PrintMessage, PrintResult, SystemDetail, Usage, UserContent,
};

// Orphan rule: ParseError (core) and PyErr (pyo3) are both foreign here, so the
// conversion is a function rather than a From impl.
pub(crate) fn parse_err(err: ParseError) -> PyErr {
    match err {
        ParseError::Key(key) => PyKeyError::new_err(format!("'{key}'")),
        ParseError::Value(msg) => PyValueError::new_err(msg),
    }
}

fn json_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    match value.get_type() {
        JsonType::Null => Ok(py.None().into_bound(py)),
        JsonType::Boolean => value.as_bool().unwrap().into_bound_py_any(py),
        JsonType::Number => match (value.as_i64(), value.as_u64()) {
            (Some(i), _) => i.into_bound_py_any(py),
            (_, Some(u)) => u.into_bound_py_any(py),
            // sonic-rs stores the rest as f64, collapsing `-0` (orjson int 0)
            // and `-0.0` (orjson float -0.0) into +0.0; re-parse the raw text
            // with orjson's semantics.
            _ => number_from_raw(py, value),
        },
        JsonType::String => Ok(PyString::new(py, value.as_str().unwrap()).into_any()),
        JsonType::Array => {
            let list = PyList::empty(py);
            for item in value.as_array().unwrap() {
                list.append(json_to_py(py, item)?)?;
            }
            Ok(list.into_any())
        }
        JsonType::Object => {
            let dict = PyDict::new(py);
            for (k, v) in value.as_object().unwrap() {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            Ok(dict.into_any())
        }
    }
}

// orjson (the PythonBackend decoder) semantics over the raw text: a frac/exp
// marker means float (the f64 text parse keeps -0.0's sign); an integer
// literal is int when it fits i64/u64 (`-0` -> 0) and a lossy float beyond.
fn number_from_raw<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    let raw = value
        .as_raw_number()
        .expect("a parsed JSON number carries raw text under arbitrary_precision");
    let text = raw.as_str();
    if !text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
        match (text.parse::<i64>(), text.parse::<u64>()) {
            (Ok(i), _) => return i.into_bound_py_any(py),
            (_, Ok(u)) => return u.into_bound_py_any(py),
            _ => {}
        }
    }
    text.parse::<f64>()
        .expect("JSON number text parses as f64")
        .into_bound_py_any(py)
}

fn build_meta<'py>(py: Python<'py>, meta: &EntryMeta) -> PyResult<Bound<'py, PyAny>> {
    let args: [Bound<'py, PyAny>; 14] = [
        meta.uuid.as_str().into_bound_py_any(py)?,
        meta.parent_uuid.as_deref().into_bound_py_any(py)?,
        meta.session_id.as_str().into_bound_py_any(py)?,
        meta.timestamp.into_bound_py_any(py)?,
        meta.cwd.as_deref().into_bound_py_any(py)?,
        meta.git_branch.as_deref().into_bound_py_any(py)?,
        meta.version.as_deref().into_bound_py_any(py)?,
        meta.is_sidechain.into_bound_py_any(py)?,
        meta.is_meta.into_bound_py_any(py)?,
        meta.entrypoint.as_deref().into_bound_py_any(py)?,
        meta.is_compact_summary.into_bound_py_any(py)?,
        meta.is_visible_in_transcript_only.into_bound_py_any(py)?,
        meta.user_type.as_deref().into_bound_py_any(py)?,
        meta.slug.as_deref().into_bound_py_any(py)?,
    ];
    models_type(py, &ENTRY_META_CLS, "EntryMeta")?.call1(PyTuple::new(py, args)?)
}

fn build_attribution<'py>(
    py: Python<'py>,
    attribution: Option<&Attribution>,
) -> PyResult<Bound<'py, PyAny>> {
    match attribution {
        Some(a) => models_type(py, &ATTRIBUTION_CLS, "Attribution")?.call1((
            a.plugin.as_deref(),
            a.skill.as_deref(),
            a.mcp_server.as_deref(),
            a.mcp_tool.as_deref(),
        )),
        None => Ok(py.None().into_bound(py)),
    }
}

fn build_api_error<'py>(
    py: Python<'py>,
    api_error: Option<&ApiError>,
) -> PyResult<Bound<'py, PyAny>> {
    match api_error {
        Some(e) => models_type(py, &API_ERROR_CLS, "ApiError")?.call1((
            e.error.as_deref(),
            e.status,
            e.details.as_deref(),
        )),
        None => Ok(py.None().into_bound(py)),
    }
}

fn build_block<'py>(py: Python<'py>, block: &ContentBlock) -> PyResult<Bound<'py, PyAny>> {
    match block {
        ContentBlock::Text(text) => models_type(py, &TEXT_BLOCK_CLS, "TextBlock")?.call1((text,)),
        ContentBlock::Thinking(thinking) => {
            models_type(py, &THINKING_BLOCK_CLS, "ThinkingBlock")?.call1((thinking,))
        }
        ContentBlock::ToolUse(tu) => models_type(py, &TOOL_USE_BLOCK_CLS, "ToolUseBlock")?.call1((
            &tu.id,
            &tu.name,
            json_to_py(py, &tu.input)?,
        )),
        ContentBlock::ToolResult(tr) => {
            let tool_use_result = match &tr.tool_use_result {
                Some(value) => json_to_py(py, value)?,
                None => py.None().into_bound(py),
            };
            models_type(py, &TOOL_RESULT_BLOCK_CLS, "ToolResultBlock")?.call1((
                &tr.tool_use_id,
                &tr.content,
                tr.is_error,
                tr.is_async,
                tool_use_result,
                tr.denial_kind.as_deref(),
            ))
        }
        ContentBlock::Fallback(fb) => models_type(py, &FALLBACK_BLOCK_CLS, "FallbackBlock")?
            .call1((&fb.from_model, &fb.to_model)),
        ContentBlock::Other { ty, raw } => {
            models_type(py, &OTHER_BLOCK_CLS, "OtherBlock")?.call1((ty, json_to_py(py, raw)?))
        }
    }
}

// The Python model orders user blocks text-first, then tool results.
fn build_user_blocks<'py>(py: Python<'py>, content: &UserContent) -> PyResult<Bound<'py, PyTuple>> {
    match content {
        UserContent::Plain(_) => Ok(PyTuple::empty(py)),
        UserContent::Blocks(blocks) => {
            let objs = blocks
                .iter()
                .filter(|b| matches!(b, ContentBlock::Text(_)))
                .chain(
                    blocks
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolResult(_))),
                )
                .map(|b| build_block(py, b))
                .collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, objs)
        }
    }
}

fn build_assistant_blocks<'py>(
    py: Python<'py>,
    blocks: &[ContentBlock],
) -> PyResult<Bound<'py, PyTuple>> {
    let objs = blocks
        .iter()
        .map(|b| build_block(py, b))
        .collect::<PyResult<Vec<_>>>()?;
    PyTuple::new(py, objs)
}

fn build_usage<'py>(py: Python<'py>, usage: Option<&Usage>) -> PyResult<Bound<'py, PyAny>> {
    match usage {
        Some(usage) => build_usage_value(py, usage),
        None => Ok(py.None().into_bound(py)),
    }
}

fn build_usage_value<'py>(py: Python<'py>, usage: &Usage) -> PyResult<Bound<'py, PyAny>> {
    let cache_creation = match &usage.cache_creation {
        Some(cc) => models_type(py, &CACHE_CREATION_CLS, "CacheCreation")?
            .call1((cc.ephemeral_5m_input_tokens, cc.ephemeral_1h_input_tokens))?,
        None => py.None().into_bound(py),
    };
    let server_tool_use = match &usage.server_tool_use {
        Some(s) => models_type(py, &SERVER_TOOL_USE_CLS, "ServerToolUse")?
            .call1((s.web_search_requests, s.web_fetch_requests))?,
        None => py.None().into_bound(py),
    };
    models_type(py, &USAGE_CLS, "Usage")?.call1((
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
        cache_creation,
        usage.service_tier.as_deref(),
        usage.inference_geo.as_deref(),
        server_tool_use,
    ))
}

fn build_system_detail<'py>(py: Python<'py>, detail: &SystemDetail) -> PyResult<Bound<'py, PyAny>> {
    match detail {
        SystemDetail::StopHookSummary(s) => {
            let hook_infos = s
                .hook_infos
                .iter()
                .map(|hi| {
                    models_type(py, &HOOK_INFO_CLS, "HookInfo")?
                        .call1((&hi.command, hi.duration_ms))
                })
                .collect::<PyResult<Vec<_>>>()?;
            models_type(py, &STOP_HOOK_SUMMARY_CLS, "StopHookSummary")?.call1((
                s.hook_count,
                PyTuple::new(py, hook_infos)?,
                PyTuple::new(py, &s.hook_errors)?,
                PyTuple::new(py, &s.hook_additional_context)?,
                s.prevented_continuation,
                s.stop_reason.as_deref(),
                s.has_output,
                s.tool_use_id.as_deref(),
            ))
        }
        SystemDetail::CompactBoundary(c) => {
            let preserved_segment = match &c.preserved_segment {
                Some(ps) => {
                    models_type(py, &PRESERVED_SEGMENT_CLS, "PreservedSegment")?.call1((
                        ps.head_uuid.as_deref(),
                        ps.anchor_uuid.as_deref(),
                        ps.tail_uuid.as_deref(),
                    ))?
                }
                None => py.None().into_bound(py),
            };
            let preserved_messages = match &c.preserved_messages {
                Some(pm) => {
                    models_type(py, &PRESERVED_MESSAGES_CLS, "PreservedMessages")?.call1((
                        pm.anchor_uuid.as_deref(),
                        PyTuple::new(py, &pm.uuids)?,
                        PyTuple::new(py, &pm.all_uuids)?,
                    ))?
                }
                None => py.None().into_bound(py),
            };
            models_type(py, &COMPACT_BOUNDARY_CLS, "CompactBoundary")?.call1((
                c.trigger.as_deref(),
                c.pre_tokens,
                c.post_tokens,
                c.duration_ms,
                c.cumulative_dropped_tokens,
                PyTuple::new(py, &c.pre_compact_discovered_tools)?,
                preserved_segment,
                preserved_messages,
                c.logical_parent_uuid.as_deref(),
                c.precomputed,
            ))
        }
        SystemDetail::TurnDuration(t) => models_type(py, &TURN_DURATION_CLS, "TurnDuration")?
            .call1((
                t.duration_ms,
                t.message_count,
                t.pending_workflow_count,
                t.pending_background_agent_count,
            )),
        SystemDetail::ModelRefusalFallback(m) => {
            models_type(py, &MODEL_REFUSAL_FALLBACK_CLS, "ModelRefusalFallback")?.call1((
                m.api_refusal_category.as_deref(),
                m.api_refusal_explanation.as_deref(),
                m.trigger.as_deref(),
                m.direction.as_deref(),
                m.original_model.as_deref(),
                m.fallback_model.as_deref(),
                PyTuple::new(py, &m.retracted_message_uuids)?,
                m.refused_user_message_uuid.as_deref(),
            ))
        }
        SystemDetail::Other(raw) => models_type(py, &OTHER_SYSTEM_DETAIL_CLS, "OtherSystemDetail")?
            .call1((json_to_py(py, raw)?,)),
    }
}

fn build_attachment_detail<'py>(
    py: Python<'py>,
    detail: &AttachmentDetail,
) -> PyResult<Bound<'py, PyAny>> {
    match detail {
        AttachmentDetail::HookSuccess(h) => models_type(py, &HOOK_SUCCESS_CLS, "HookSuccess")?
            .call1((
                h.hook_name.as_deref(),
                h.hook_event.as_deref(),
                h.tool_use_id.as_deref(),
                h.command.as_deref(),
                h.content.as_deref(),
                h.stdout.as_deref(),
                h.stderr.as_deref(),
                h.exit_code,
                h.duration_ms,
            )),
        AttachmentDetail::HookBlockingError(h) => {
            let blocking_error = match &h.blocking_error {
                Some(v) => json_to_py(py, v)?,
                None => py.None().into_bound(py),
            };
            models_type(py, &HOOK_BLOCKING_ERROR_CLS, "HookBlockingError")?.call1((
                h.hook_name.as_deref(),
                h.hook_event.as_deref(),
                h.tool_use_id.as_deref(),
                blocking_error,
            ))
        }
        AttachmentDetail::HookNonBlockingError(h) => {
            models_type(py, &HOOK_NON_BLOCKING_ERROR_CLS, "HookNonBlockingError")?.call1((
                h.hook_name.as_deref(),
                h.hook_event.as_deref(),
                h.tool_use_id.as_deref(),
                h.command.as_deref(),
                h.stdout.as_deref(),
                h.stderr.as_deref(),
                h.exit_code,
                h.duration_ms,
            ))
        }
        AttachmentDetail::HookCancelled(h) => {
            models_type(py, &HOOK_CANCELLED_CLS, "HookCancelled")?.call1((
                h.hook_name.as_deref(),
                h.hook_event.as_deref(),
                h.tool_use_id.as_deref(),
                h.command.as_deref(),
                h.duration_ms,
                h.timed_out,
                h.timeout_ms,
            ))
        }
        AttachmentDetail::HookAdditionalContext(h) => {
            models_type(py, &HOOK_ADDITIONAL_CONTEXT_CLS, "HookAdditionalContext")?.call1((
                h.hook_name.as_deref(),
                h.hook_event.as_deref(),
                h.tool_use_id.as_deref(),
                PyTuple::new(py, &h.content)?,
            ))
        }
        AttachmentDetail::AsyncHookResponse(h) => {
            let response = match &h.response {
                Some(v) => json_to_py(py, v)?,
                None => py.None().into_bound(py),
            };
            models_type(py, &ASYNC_HOOK_RESPONSE_CLS, "AsyncHookResponse")?.call1((
                h.hook_name.as_deref(),
                h.hook_event.as_deref(),
                h.process_id.as_deref(),
                h.stdout.as_deref(),
                h.stderr.as_deref(),
                h.exit_code,
                response,
            ))
        }
        AttachmentDetail::QueuedCommand(q) => {
            models_type(py, &QUEUED_COMMAND_CLS, "QueuedCommand")?
                .call1((q.prompt.as_deref(), q.command_mode.as_deref()))
        }
        AttachmentDetail::Other(raw) => models_type(py, &OTHER_ATTACHMENT_CLS, "OtherAttachment")?
            .call1((json_to_py(py, raw)?,)),
    }
}

pub fn build_event<'py>(py: Python<'py>, entry: &Entry) -> PyResult<Bound<'py, PyAny>> {
    match entry {
        Entry::User(user) => {
            let text = user.content.text();
            let interrupted =
                interrupt_marker(&text).is_some() || user.interrupted_message_id.is_some();
            let is_agent_injected = is_agent_injection(&text);
            let image_paste_ids = match &user.image_paste_ids {
                Some(ids) => PyTuple::new(py, ids.iter().copied())?.into_any(),
                None => py.None().into_bound(py),
            };
            let mcp_meta = match &user.mcp_meta {
                Some(value) => json_to_py(py, value)?,
                None => py.None().into_bound(py),
            };
            let args: [Bound<'py, PyAny>; 14] = [
                build_meta(py, &user.meta)?,
                text.into_bound_py_any(py)?,
                build_user_blocks(py, &user.content)?.into_any(),
                interrupted.into_bound_py_any(py)?,
                is_agent_injected.into_bound_py_any(py)?,
                user.prompt_id.as_deref().into_bound_py_any(py)?,
                user.prompt_source.as_deref().into_bound_py_any(py)?,
                user.queue_priority.as_deref().into_bound_py_any(py)?,
                image_paste_ids,
                user.source_tool_use_id.as_deref().into_bound_py_any(py)?,
                user.source_tool_assistant_uuid
                    .as_deref()
                    .into_bound_py_any(py)?,
                mcp_meta,
                user.permission_mode.as_deref().into_bound_py_any(py)?,
                user.interrupted_message_id
                    .as_deref()
                    .into_bound_py_any(py)?,
            ];
            models_type(py, &USER_EVENT_CLS, "UserEvent")?.call1(PyTuple::new(py, args)?)
        }
        Entry::Assistant(assistant) => {
            let attribution = build_attribution(py, assistant.attribution.as_ref())?;
            let api_error = build_api_error(py, assistant.api_error.as_ref())?;
            models_type(py, &ASSISTANT_EVENT_CLS, "AssistantEvent")?.call1((
                build_meta(py, &assistant.meta)?,
                &assistant.model,
                joined_text(&assistant.blocks),
                build_assistant_blocks(py, &assistant.blocks)?,
                assistant.stop_reason.as_deref(),
                build_usage(py, assistant.usage.as_ref())?,
                assistant.request_id.as_deref(),
                assistant.forked_from.as_deref(),
                attribution,
                api_error,
            ))
        }
        Entry::System(system) => {
            let detail = build_system_detail(py, &system.detail)?;
            models_type(py, &SYSTEM_EVENT_CLS, "SystemEvent")?.call1((
                build_meta(py, &system.meta)?,
                &system.subtype,
                system.content.as_deref(),
                system.level.as_deref(),
                detail,
            ))
        }
        Entry::Mode(mode) => models_type(py, &MODE_EVENT_CLS, "ModeEvent")?.call1((
            &mode.session_id,
            mode.channel.as_str(),
            &mode.value,
        )),
        Entry::Other(other) => models_type(py, &OTHER_EVENT_CLS, "OtherEvent")?
            .call1((&other.ty, json_to_py(py, &other.raw)?)),
        Entry::Attachment(att) => {
            let detail = build_attachment_detail(py, &att.detail)?;
            models_type(py, &ATTACHMENT_EVENT_CLS, "AttachmentEvent")?.call1((
                build_meta(py, &att.meta)?,
                &att.attachment_type,
                detail,
            ))
        }
    }
}

fn build_model_usage<'py>(py: Python<'py>, usage: &ModelUsage) -> PyResult<Bound<'py, PyAny>> {
    models_type(py, &MODEL_USAGE_CLS, "ModelUsage")?.call1((
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
        usage.web_search_requests,
        usage.cost_usd,
        usage.context_window,
        usage.max_output_tokens,
    ))
}

fn build_init<'py>(py: Python<'py>, init: &InitInfo) -> PyResult<Bound<'py, PyAny>> {
    let mcp_servers = init
        .mcp_servers
        .iter()
        .map(|s| models_type(py, &MCP_SERVER_CLS, "McpServer")?.call1((&s.name, &s.status)))
        .collect::<PyResult<Vec<_>>>()?;
    let plugins = init
        .plugins
        .iter()
        .map(|p| models_type(py, &PLUGIN_CLS, "Plugin")?.call1((&p.name, &p.path, &p.source)))
        .collect::<PyResult<Vec<_>>>()?;
    models_type(py, &INIT_INFO_CLS, "InitInfo")?.call1((
        PyTuple::new(py, mcp_servers)?,
        PyTuple::new(py, plugins)?,
        PyTuple::new(py, &init.tools)?,
        PyTuple::new(py, &init.skills)?,
    ))
}

fn build_print_message<'py>(
    py: Python<'py>,
    message: &PrintMessage,
) -> PyResult<Bound<'py, PyAny>> {
    let (role, model, text, blocks): (&str, Bound<'py, PyAny>, String, Bound<'py, PyTuple>) =
        match &message.body {
            PrintBody::Assistant { model, blocks } => (
                "assistant",
                model.as_deref().into_bound_py_any(py)?,
                joined_text(blocks),
                build_assistant_blocks(py, blocks)?,
            ),
            PrintBody::User(content) => (
                "user",
                py.None().into_bound(py),
                content.text(),
                build_user_blocks(py, content)?,
            ),
        };
    models_type(py, &PRINT_MESSAGE_CLS, "PrintMessage")?.call1((
        role,
        model,
        text,
        blocks,
        message.uuid.as_deref(),
        &message.session_id,
    ))
}

pub fn build_print_result<'py>(
    py: Python<'py>,
    result: &PrintResult,
) -> PyResult<Bound<'py, PyAny>> {
    let model_usage = PyDict::new(py);
    for (model, usage) in &result.model_usage {
        model_usage.set_item(model, build_model_usage(py, usage)?)?;
    }
    let structured_output = match &result.structured_output {
        Some(s) => json_to_py(py, s)?,
        None => py.None().into_bound(py),
    };
    let permission_denials = result
        .permission_denials
        .iter()
        .map(|d| json_to_py(py, d))
        .collect::<PyResult<Vec<_>>>()?;
    let init_obj = match &result.init {
        Some(init) => build_init(py, init)?,
        None => py.None().into_bound(py),
    };
    let messages = result
        .messages
        .iter()
        .map(|m| build_print_message(py, m))
        .collect::<PyResult<Vec<_>>>()?;

    let args: [Bound<'py, PyAny>; 13] = [
        result.total_cost_usd.into_bound_py_any(py)?,
        model_usage.into_any(),
        build_usage_value(py, &result.usage)?,
        structured_output,
        result.num_turns.into_bound_py_any(py)?,
        result.is_error.into_bound_py_any(py)?,
        result.result.as_deref().into_bound_py_any(py)?,
        result.session_id.as_str().into_bound_py_any(py)?,
        result.fast_mode_state.as_deref().into_bound_py_any(py)?,
        result.stop_reason.as_deref().into_bound_py_any(py)?,
        PyTuple::new(py, permission_denials)?.into_any(),
        init_obj,
        PyTuple::new(py, messages)?.into_any(),
    ];
    models_type(py, &PRINT_RESULT_CLS, "PrintResult")?.call1(PyTuple::new(py, args)?)
}
