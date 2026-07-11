use chrono::{DateTime, FixedOffset};
use memchr::memchr_iter;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::protocol::{DENIAL_KIND_USER_REJECTED, DENIAL_PREFIX};
use crate::types::{
    ApiError, AssistantEntry, Attribution, CacheCreation, CompactBoundary, ContentBlock, Entry,
    EntryMeta, FallbackBlock, HookInfo, InitInfo, McpServer, ModeChannel, ModeEntry, ModelRefusalFallback,
    ModelUsage, OtherEntry, Plugin, PreservedMessages, PreservedSegment, PrintBody, PrintMessage,
    PrintResult, Question, ServerToolUse, StopHookSummary, SystemDetail, SystemEntry, ToolResultBlock,
    ToolUseBlock, TurnDuration, Usage, UserContent, UserEntry,
};
use crate::value::{block_type, field, field_bool, field_str};

const AVG_LINE_BYTES: usize = 1400;

/// A malformed entry or envelope. Mapped to the matching Python exception
/// (``KeyError`` / ``ValueError``) at the pyo3 boundary in ``event.rs``.
#[derive(Debug)]
pub enum ParseError {
    Key(String),
    Value(String),
}

pub(crate) fn truthy_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field_str(data, key).filter(|s| !s.is_empty())
}

fn require<'a>(data: &'a Value, key: &str) -> Result<&'a Value, ParseError> {
    field(data, key).ok_or_else(|| ParseError::Key(key.to_string()))
}

pub(crate) fn require_str<'a>(data: &'a Value, key: &str) -> Result<&'a str, ParseError> {
    require(data, key)?
        .as_str()
        .ok_or_else(|| ParseError::Key(key.to_string()))
}

fn require_i64(data: &Value, key: &str) -> Result<i64, ParseError> {
    require(data, key)?
        .as_i64()
        .ok_or_else(|| ParseError::Key(key.to_string()))
}

fn require_f64(data: &Value, key: &str) -> Result<f64, ParseError> {
    require(data, key)?
        .as_f64()
        .ok_or_else(|| ParseError::Key(key.to_string()))
}

fn require_bool(data: &Value, key: &str) -> Result<bool, ParseError> {
    require(data, key)?
        .as_bool()
        .ok_or_else(|| ParseError::Key(key.to_string()))
}

pub(crate) fn parse_timestamp(raw: &str) -> Result<DateTime<FixedOffset>, ParseError> {
    DateTime::parse_from_rfc3339(raw)
        .map_err(|e| ParseError::Value(format!("invalid timestamp {raw:?}: {e}")))
}

fn require_array(content: &Value) -> Result<&[Value], ParseError> {
    content
        .as_array()
        .map(|a| a.as_slice())
        .ok_or_else(|| ParseError::Value("message content is neither a string nor a list".to_string()))
}

fn flatten_result_content(content: &Value) -> String {
    match content.as_str() {
        Some(s) => s.to_string(),
        None => content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| block_type(b) == Some("text"))
            .filter_map(|b| field_str(b, "text"))
            .collect(),
    }
}

fn parse_meta(data: &Value) -> Result<EntryMeta, ParseError> {
    Ok(EntryMeta {
        uuid: require_str(data, "uuid")?.to_string(),
        parent_uuid: truthy_str(data, "parentUuid").map(str::to_string),
        session_id: require_str(data, "sessionId")?.to_string(),
        timestamp: parse_timestamp(require_str(data, "timestamp")?)?,
        cwd: field_str(data, "cwd").map(str::to_string),
        git_branch: field_str(data, "gitBranch").map(str::to_string),
        version: truthy_str(data, "version").map(str::to_string),
        is_sidechain: field_bool(data, "isSidechain"),
        is_meta: field_bool(data, "isMeta"),
        entrypoint: field_str(data, "entrypoint").map(str::to_string),
        is_compact_summary: field_bool(data, "isCompactSummary"),
        is_visible_in_transcript_only: field_bool(data, "isVisibleInTranscriptOnly"),
        user_type: field_str(data, "userType").map(str::to_string),
        slug: field_str(data, "slug").map(str::to_string),
    })
}

fn parse_image_paste_ids(data: &Value) -> Option<Vec<i64>> {
    Some(
        field(data, "imagePasteIds")?
            .as_array()?
            .iter()
            .filter_map(JsonValueTrait::as_i64)
            .collect(),
    )
}

fn parse_attribution(data: &Value) -> Option<Attribution> {
    let plugin = field_str(data, "attributionPlugin").map(str::to_string);
    let skill = field_str(data, "attributionSkill").map(str::to_string);
    let mcp_server = field_str(data, "attributionMcpServer").map(str::to_string);
    let mcp_tool = field_str(data, "attributionMcpTool").map(str::to_string);
    if plugin.is_none() && skill.is_none() && mcp_server.is_none() && mcp_tool.is_none() {
        return None;
    }
    Some(Attribution {
        plugin,
        skill,
        mcp_server,
        mcp_tool,
    })
}

fn parse_api_error(data: &Value) -> Option<ApiError> {
    if !field_bool(data, "isApiErrorMessage") {
        return None;
    }
    Some(ApiError {
        error: field_str(data, "error").map(str::to_string),
        status: field(data, "apiErrorStatus").and_then(JsonValueTrait::as_i64),
        details: field_str(data, "errorDetails").map(str::to_string),
    })
}

fn parse_tool_result(
    block: &Value,
    tool_use_result: Option<&Value>,
    tool_denial_kind: Option<&str>,
) -> Result<ContentBlock, ParseError> {
    let content = flatten_result_content(require(block, "content")?);
    let is_error = field_bool(block, "is_error");
    let denial_kind = tool_denial_kind.map(str::to_string).or_else(|| {
        (is_error && content.starts_with(DENIAL_PREFIX)).then(|| DENIAL_KIND_USER_REJECTED.to_string())
    });
    Ok(ContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: require_str(block, "tool_use_id")?.to_string(),
        content,
        is_error,
        is_async: tool_use_result.is_some_and(|tur| field_bool(tur, "isAsync")),
        tool_use_result: tool_use_result.cloned(),
        denial_kind,
    }))
}

fn parse_user_content(
    content: &Value,
    tool_use_result: Option<&Value>,
    tool_denial_kind: Option<&str>,
) -> Result<UserContent, ParseError> {
    match content.as_str() {
        Some(s) => Ok(UserContent::Plain(s.to_string())),
        None => {
            let blocks = require_array(content)?
                .iter()
                .filter_map(|b| match block_type(b) {
                    Some("text") => field_str(b, "text").map(|t| Ok(ContentBlock::Text(t.to_string()))),
                    Some("tool_result") => Some(parse_tool_result(b, tool_use_result, tool_denial_kind)),
                    _ => None,
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(UserContent::Blocks(blocks))
        }
    }
}

pub(crate) fn parse_questions(input: &Value) -> Option<Vec<Question>> {
    let questions = field(input, "questions")?.as_array()?;
    Some(
        questions
            .iter()
            .filter_map(|question| {
                Some(Question {
                    question: field_str(question, "question")?.to_string(),
                    header: field_str(question, "header").map(str::to_string),
                    multi_select: field_bool(question, "multiSelect"),
                    labels: field(question, "options")
                        .and_then(JsonContainerTrait::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|option| field_str(option, "label"))
                        .map(String::from)
                        .collect(),
                })
            })
            .collect(),
    )
}

fn parse_assistant_block(block: &Value) -> Result<ContentBlock, ParseError> {
    match block_type(block) {
        Some("text") => Ok(ContentBlock::Text(require_str(block, "text")?.to_string())),
        Some("thinking") => Ok(ContentBlock::Thinking(require_str(block, "thinking")?.to_string())),
        Some("tool_use") => {
            let id = require_str(block, "id")?.to_string();
            let name = require_str(block, "name")?.to_string();
            let input = require(block, "input")?;
            Ok(ContentBlock::ToolUse(ToolUseBlock {
                id,
                name,
                run_in_background: field(input, "run_in_background").and_then(JsonValueTrait::as_bool),
                subagent_type: field_str(input, "subagent_type").map(str::to_string),
                file_path: field_str(input, "file_path").map(str::to_string),
                questions: parse_questions(input),
                input: input.clone(),
            }))
        }
        Some("fallback") => Ok(ContentBlock::Fallback(FallbackBlock {
            from_model: require_str(require(block, "from")?, "model")?.to_string(),
            to_model: require_str(require(block, "to")?, "model")?.to_string(),
        })),
        Some(other) => Ok(ContentBlock::Other {
            ty: other.to_string(),
            raw: block.clone(),
        }),
        None => Err(ParseError::Key("type".to_string())),
    }
}

fn parse_assistant_blocks(content: &Value) -> Result<Vec<ContentBlock>, ParseError> {
    require_array(content)?.iter().map(parse_assistant_block).collect()
}

fn parse_usage(message: &Value) -> Result<Option<Usage>, ParseError> {
    match field(message, "usage").filter(|u| u.is_object()) {
        Some(usage) => Ok(Some(parse_usage_value(usage)?)),
        None => Ok(None),
    }
}

fn parse_usage_value(usage: &Value) -> Result<Usage, ParseError> {
    let cache_creation = match field(usage, "cache_creation").filter(|cc| cc.is_object()) {
        Some(cc) => Some(CacheCreation {
            ephemeral_5m_input_tokens: require_i64(cc, "ephemeral_5m_input_tokens")?,
            ephemeral_1h_input_tokens: require_i64(cc, "ephemeral_1h_input_tokens")?,
        }),
        None => None,
    };
    let server_tool_use = match field(usage, "server_tool_use").filter(|s| s.is_object()) {
        Some(s) => Some(ServerToolUse {
            web_search_requests: require_i64(s, "web_search_requests")?,
            web_fetch_requests: require_i64(s, "web_fetch_requests")?,
        }),
        None => None,
    };
    Ok(Usage {
        input_tokens: require_i64(usage, "input_tokens")?,
        output_tokens: require_i64(usage, "output_tokens")?,
        cache_read_input_tokens: require_i64(usage, "cache_read_input_tokens")?,
        cache_creation_input_tokens: require_i64(usage, "cache_creation_input_tokens")?,
        cache_creation,
        service_tier: field_str(usage, "service_tier").map(str::to_string),
        inference_geo: field_str(usage, "inference_geo").map(str::to_string),
        server_tool_use,
    })
}

fn opt_str(data: &Value, key: &str) -> Option<String> {
    field_str(data, key).map(str::to_string)
}

fn opt_i64(data: &Value, key: &str) -> Option<i64> {
    field(data, key).and_then(JsonValueTrait::as_i64)
}

fn str_array(data: &Value, key: &str) -> Vec<String> {
    field(data, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .map(String::from)
        .collect()
}

fn parse_hook_infos(data: &Value) -> Vec<HookInfo> {
    field(data, "hookInfos")
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(|info| {
            Some(HookInfo {
                command: field_str(info, "command")?.to_string(),
                duration_ms: opt_i64(info, "durationMs"),
            })
        })
        .collect()
}

fn parse_preserved_segment(segment: Option<&Value>) -> Option<PreservedSegment> {
    let segment = segment.filter(|v| v.is_object())?;
    Some(PreservedSegment {
        head_uuid: truthy_str(segment, "headUuid").map(str::to_string),
        anchor_uuid: truthy_str(segment, "anchorUuid").map(str::to_string),
        tail_uuid: truthy_str(segment, "tailUuid").map(str::to_string),
    })
}

fn parse_preserved_messages(messages: Option<&Value>) -> Option<PreservedMessages> {
    let messages = messages.filter(|v| v.is_object())?;
    Some(PreservedMessages {
        anchor_uuid: truthy_str(messages, "anchorUuid").map(str::to_string),
        uuids: str_array(messages, "uuids"),
        all_uuids: str_array(messages, "allUuids"),
    })
}

fn parse_system_detail(data: &Value) -> SystemDetail {
    match field_str(data, "subtype") {
        Some("stop_hook_summary") => SystemDetail::StopHookSummary(StopHookSummary {
            hook_count: opt_i64(data, "hookCount"),
            hook_infos: parse_hook_infos(data),
            hook_errors: str_array(data, "hookErrors"),
            hook_additional_context: str_array(data, "hookAdditionalContext"),
            prevented_continuation: field_bool(data, "preventedContinuation"),
            stop_reason: opt_str(data, "stopReason"),
            has_output: field_bool(data, "hasOutput"),
            tool_use_id: truthy_str(data, "toolUseID").map(str::to_string),
        }),
        Some("compact_boundary") => {
            let empty = Value::default();
            let metadata = field(data, "compactMetadata").unwrap_or(&empty);
            SystemDetail::CompactBoundary(CompactBoundary {
                trigger: opt_str(metadata, "trigger"),
                pre_tokens: opt_i64(metadata, "preTokens"),
                post_tokens: opt_i64(metadata, "postTokens"),
                duration_ms: opt_i64(metadata, "durationMs"),
                cumulative_dropped_tokens: opt_i64(metadata, "cumulativeDroppedTokens"),
                pre_compact_discovered_tools: str_array(metadata, "preCompactDiscoveredTools"),
                preserved_segment: parse_preserved_segment(field(metadata, "preservedSegment")),
                preserved_messages: parse_preserved_messages(field(metadata, "preservedMessages")),
                logical_parent_uuid: truthy_str(data, "logicalParentUuid").map(str::to_string),
                precomputed: field(metadata, "precomputed").and_then(JsonValueTrait::as_bool),
            })
        }
        Some("turn_duration") => SystemDetail::TurnDuration(TurnDuration {
            duration_ms: opt_i64(data, "durationMs"),
            message_count: opt_i64(data, "messageCount"),
            pending_workflow_count: opt_i64(data, "pendingWorkflowCount"),
            pending_background_agent_count: opt_i64(data, "pendingBackgroundAgentCount"),
        }),
        Some("model_refusal_fallback") => SystemDetail::ModelRefusalFallback(ModelRefusalFallback {
            api_refusal_category: opt_str(data, "apiRefusalCategory"),
            api_refusal_explanation: opt_str(data, "apiRefusalExplanation"),
            trigger: opt_str(data, "trigger"),
            direction: opt_str(data, "direction"),
            original_model: opt_str(data, "originalModel"),
            fallback_model: opt_str(data, "fallbackModel"),
            retracted_message_uuids: str_array(data, "retractedMessageUuids"),
            refused_user_message_uuid: truthy_str(data, "refusedUserMessageUuid").map(str::to_string),
        }),
        _ => SystemDetail::Other(data.clone()),
    }
}

/// Parse one JSONL transcript line into the typed model. Consumes the value so
/// unrecognized entry kinds keep their payload verbatim without a copy.
pub fn parse_entry(data: Value) -> Result<Entry, ParseError> {
    let ty = require_str(&data, "type")?.to_string();
    match ty.as_str() {
        "user" => {
            let tool_use_result = field(&data, "toolUseResult");
            let tool_denial_kind = field_str(&data, "toolDenialKind");
            let content = parse_user_content(
                require(require(&data, "message")?, "content")?,
                tool_use_result,
                tool_denial_kind,
            )?;
            return Ok(Entry::User(UserEntry {
                meta: parse_meta(&data)?,
                content,
                prompt_id: field_str(&data, "promptId").map(str::to_string),
                prompt_source: field_str(&data, "promptSource").map(str::to_string),
                queue_priority: field_str(&data, "queuePriority").map(str::to_string),
                image_paste_ids: parse_image_paste_ids(&data),
                source_tool_use_id: truthy_str(&data, "sourceToolUseID").map(str::to_string),
                source_tool_assistant_uuid: truthy_str(&data, "sourceToolAssistantUUID")
                    .map(str::to_string),
                mcp_meta: field(&data, "mcpMeta").cloned(),
                permission_mode: field_str(&data, "permissionMode").map(str::to_string),
                interrupted_message_id: field_str(&data, "interruptedMessageId").map(str::to_string),
            }));
        }
        "assistant" => {
            let message = require(&data, "message")?;
            let blocks = parse_assistant_blocks(require(message, "content")?)?;
            let meta = parse_meta(&data)?;
            return Ok(Entry::Assistant(AssistantEntry {
                meta,
                model: require_str(message, "model")?.to_string(),
                blocks,
                stop_reason: field_str(message, "stop_reason").map(str::to_string),
                usage: parse_usage(message)?,
                request_id: field_str(&data, "requestId").map(str::to_string),
                forked_from: field_str(&data, "forkedFrom").map(str::to_string),
                attribution: parse_attribution(&data),
                api_error: parse_api_error(&data),
            }));
        }
        "system" => {
            return Ok(Entry::System(SystemEntry {
                meta: parse_meta(&data)?,
                subtype: require_str(&data, "subtype")?.to_string(),
                content: field_str(&data, "content").map(str::to_string),
                level: field_str(&data, "level").map(str::to_string),
                detail: parse_system_detail(&data),
            }));
        }
        "mode" => {
            return Ok(Entry::Mode(ModeEntry {
                session_id: require_str(&data, "sessionId")?.to_string(),
                channel: ModeChannel::Mode,
                value: require_str(&data, "mode")?.to_string(),
            }));
        }
        "permission-mode" => {
            return Ok(Entry::Mode(ModeEntry {
                session_id: require_str(&data, "sessionId")?.to_string(),
                channel: ModeChannel::PermissionMode,
                value: require_str(&data, "permissionMode")?.to_string(),
            }));
        }
        _ => {}
    }
    Ok(Entry::Other(OtherEntry { ty, raw: data }))
}

// Non-JSON lines and valid-JSON lines that are not objects (bare scalars or
// arrays) are skipped; a JSON object that fails the typed parse (e.g. a missing
// required field) fails the whole file — whole-file parity with PythonBackend,
// which decodes every line, skips non-objects, then parses the rest.
fn parse_line<F: Fn(&Entry) -> bool>(
    line: &[u8],
    lines: &mut Vec<Entry>,
    keep: &F,
) -> Result<(), ParseError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    if let Ok(value) = sonic_rs::from_slice::<Value>(line) {
        if !value.is_object() {
            return Ok(());
        }
        let entry = parse_entry(value)?;
        if keep(&entry) {
            lines.push(entry);
        }
    }
    Ok(())
}

pub fn parse_bytes<F: Fn(&Entry) -> bool>(bytes: &[u8], keep: F) -> Result<Vec<Entry>, ParseError> {
    let mut lines: Vec<Entry> = Vec::with_capacity(bytes.len() / AVG_LINE_BYTES + 1);
    let mut start = 0usize;
    for pos in memchr_iter(b'\n', bytes) {
        parse_line(&bytes[start..pos], &mut lines, &keep)?;
        start = pos + 1;
    }
    if start < bytes.len() {
        parse_line(&bytes[start..], &mut lines, &keep)?;
    }
    Ok(lines)
}

fn parse_model_usage(usage: &Value) -> Result<ModelUsage, ParseError> {
    Ok(ModelUsage {
        input_tokens: require_i64(usage, "inputTokens")?,
        output_tokens: require_i64(usage, "outputTokens")?,
        cache_read_input_tokens: require_i64(usage, "cacheReadInputTokens")?,
        cache_creation_input_tokens: require_i64(usage, "cacheCreationInputTokens")?,
        web_search_requests: require_i64(usage, "webSearchRequests")?,
        cost_usd: require_f64(usage, "costUSD")?,
        context_window: require_i64(usage, "contextWindow")?,
        max_output_tokens: require_i64(usage, "maxOutputTokens")?,
    })
}

fn parse_init(init: &Value) -> Result<InitInfo, ParseError> {
    let mcp_servers = require(init, "mcp_servers")?
        .as_array()
        .ok_or_else(|| ParseError::Key("mcp_servers".to_string()))?
        .iter()
        .map(|s| {
            Ok(McpServer {
                name: require_str(s, "name")?.to_string(),
                status: require_str(s, "status")?.to_string(),
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;
    let plugins = require(init, "plugins")?
        .as_array()
        .ok_or_else(|| ParseError::Key("plugins".to_string()))?
        .iter()
        .map(|p| {
            Ok(Plugin {
                name: require_str(p, "name")?.to_string(),
                path: require_str(p, "path")?.to_string(),
                source: require_str(p, "source")?.to_string(),
            })
        })
        .collect::<Result<Vec<_>, ParseError>>()?;
    let tools = require(init, "tools")?
        .as_array()
        .ok_or_else(|| ParseError::Key("tools".to_string()))?
        .iter()
        .map(|t| {
            t.as_str()
                .map(str::to_string)
                .ok_or_else(|| ParseError::Key("tools".to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let skills = require(init, "skills")?
        .as_array()
        .ok_or_else(|| ParseError::Key("skills".to_string()))?
        .iter()
        .map(|s| {
            s.as_str()
                .map(str::to_string)
                .ok_or_else(|| ParseError::Key("skills".to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(InitInfo {
        mcp_servers,
        plugins,
        tools,
        skills,
    })
}

fn parse_print_message(element: &Value) -> Result<PrintMessage, ParseError> {
    let role = require_str(element, "type")?;
    let message = require(element, "message")?;
    let body = match role {
        "assistant" => PrintBody::Assistant {
            blocks: parse_assistant_blocks(require(message, "content")?)?,
            model: field_str(message, "model").map(str::to_string),
        },
        "user" => PrintBody::User(parse_user_content(require(message, "content")?, None, None)?),
        other => {
            return Err(ParseError::Value(format!(
                "not a conversational element: {other:?}"
            )))
        }
    };
    Ok(PrintMessage {
        body,
        uuid: field_str(element, "uuid").map(str::to_string),
        session_id: require_str(element, "session_id")?.to_string(),
    })
}

/// Parse a ``--print`` JSON envelope into the typed model.
pub fn parse_print_envelope(envelope: &Value) -> Result<PrintResult, ParseError> {
    let elements = envelope
        .as_array()
        .ok_or_else(|| ParseError::Value("envelope is not a JSON array".to_string()))?;
    let result = elements
        .iter()
        .find(|e| block_type(e) == Some("result"))
        .ok_or_else(|| ParseError::Value("envelope has no result element".to_string()))?;
    let init = elements
        .iter()
        .find(|e| block_type(e) == Some("system") && field_str(e, "subtype") == Some("init"));

    let mut model_usage = Vec::new();
    for (model, usage) in require(result, "modelUsage")?
        .as_object()
        .ok_or_else(|| ParseError::Key("modelUsage".to_string()))?
    {
        model_usage.push((model.to_string(), parse_model_usage(usage)?));
    }

    let structured_output = field(result, "structured_output")
        .filter(|s| !s.is_null())
        .cloned();
    let permission_denials = require(result, "permission_denials")?
        .as_array()
        .ok_or_else(|| ParseError::Key("permission_denials".to_string()))?
        .iter()
        .cloned()
        .collect();
    let init = init.map(parse_init).transpose()?;
    let messages = elements
        .iter()
        .filter(|e| matches!(block_type(e), Some("user") | Some("assistant")))
        .map(parse_print_message)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(PrintResult {
        total_cost_usd: require_f64(result, "total_cost_usd")?,
        model_usage,
        usage: parse_usage_value(require(result, "usage")?)?,
        structured_output,
        num_turns: require_i64(result, "num_turns")?,
        is_error: require_bool(result, "is_error")?,
        result: field_str(result, "result").map(str::to_string),
        session_id: require_str(result, "session_id")?.to_string(),
        fast_mode_state: field_str(result, "fast_mode_state").map(str::to_string),
        stop_reason: field_str(result, "stop_reason").map(str::to_string),
        permission_denials,
        init,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::joined_text;

    fn parse(raw: &str) -> Value {
        sonic_rs::from_str(raw).unwrap()
    }

    const META: &str = r#""uuid":"u1","parentUuid":"","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z","isMeta":true"#;

    #[test]
    fn require_array_rejects_non_array() {
        for raw in ["{\"a\":1}", "\"text\"", "5", "null", "true"] {
            assert!(require_array(&parse(raw)).is_err(), "should reject {raw}");
        }
        assert_eq!(require_array(&parse("[1,2,3]")).unwrap().len(), 3);
    }

    #[test]
    fn flatten_result_content_joins_only_text_blocks() {
        assert_eq!(flatten_result_content(&parse("\"hi\"")), "hi");
        let blocks =
            parse(r#"[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]"#);
        assert_eq!(flatten_result_content(&blocks), "ab");
    }

    #[test]
    fn parse_timestamp_requires_offset() {
        assert!(parse_timestamp("2026-01-02T03:04:05Z").is_ok());
        assert!(parse_timestamp("2026-01-02T03:04:05+05:30").is_ok());
        assert!(parse_timestamp("2026-01-02T03:04:05").is_err());
    }

    #[test]
    fn truthy_str_drops_empty_and_nonstring() {
        let data = parse(r#"{"a":"x","b":"","c":5}"#);
        assert_eq!(truthy_str(&data, "a"), Some("x"));
        assert_eq!(truthy_str(&data, "b"), None);
        assert_eq!(truthy_str(&data, "c"), None);
        assert_eq!(truthy_str(&data, "missing"), None);
    }

    #[test]
    fn user_plain_content_keeps_text_and_empty_blocks() {
        let raw = format!(r#"{{"type":"user",{META},"message":{{"content":"hi there"}}}}"#);
        let entry = parse_entry(parse(&raw)).unwrap();
        let Entry::User(user) = &entry else {
            panic!("expected user entry")
        };
        assert_eq!(user.content.text(), "hi there");
        assert!(!user.interrupted());
        assert!(entry.blocks().is_empty());
        let meta = entry.meta().unwrap();
        assert_eq!(meta.uuid, "u1");
        assert_eq!(meta.parent_uuid, None, "empty parentUuid is dropped");
        assert!(meta.is_meta);
        assert!(!meta.is_sidechain);
        assert_eq!(entry.session_id(), Some("s1"));
    }

    #[test]
    fn user_blocks_keep_document_order_and_async_flag() {
        let raw = format!(
            r#"{{"type":"user",{META},"toolUseResult":{{"isAsync":true}},"message":{{"content":[
                {{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}},
                {{"type":"image"}},
                {{"type":"text","text":"caption"}}
            ]}}}}"#
        );
        let entry = parse_entry(parse(&raw)).unwrap();
        let Entry::User(user) = &entry else {
            panic!("expected user entry")
        };
        assert_eq!(user.content.text(), "caption");
        let blocks = entry.blocks();
        assert_eq!(blocks.len(), 2, "unknown block kinds are dropped");
        assert!(matches!(&blocks[0], ContentBlock::ToolResult(_)));
        assert!(matches!(&blocks[1], ContentBlock::Text(t) if t == "caption"));
        let result = entry.tool_results().next().unwrap();
        assert_eq!(result.tool_use_id, "t1");
        assert_eq!(result.content, "ok");
        assert!(!result.is_error);
        assert!(result.is_async);
        assert!(field_bool(result.tool_use_result.as_ref().unwrap(), "isAsync"));
    }

    #[test]
    fn user_interrupt_marker_detected() {
        let raw = format!(
            r#"{{"type":"user",{META},"message":{{"content":"[Request interrupted by user]"}}}}"#
        );
        let Entry::User(user) = parse_entry(parse(&raw)).unwrap() else {
            panic!("expected user entry")
        };
        assert!(user.interrupted());
    }

    #[test]
    fn assistant_tool_use_carries_typed_reads_and_verbatim_input() {
        let raw = format!(
            r#"{{"type":"assistant",{META},"message":{{"model":"m1","content":[
                {{"type":"text","text":"on it"}},
                {{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"ls","run_in_background":true}}}},
                {{"type":"tool_use","id":"t2","name":"Task","input":{{"subagent_type":"Explore"}}}},
                {{"type":"tool_use","id":"t3","name":"Edit","input":{{"file_path":"/x.py"}}}},
                {{"type":"tool_use","id":"t4","name":"AskUserQuestion","input":{{"questions":[
                    {{"question":"Q?","header":"H","multiSelect":true,"options":[{{"label":"A"}},{{"bad":1}}]}},
                    {{"header":"no question text"}}
                ]}}}}
            ]}}}}"#
        );
        let entry = parse_entry(parse(&raw)).unwrap();
        let Entry::Assistant(assistant) = &entry else {
            panic!("expected assistant entry")
        };
        assert_eq!(assistant.model, "m1");
        assert!(assistant.usage.is_none());
        let uses: Vec<_> = entry.tool_uses().collect();
        assert_eq!(uses.len(), 4);
        assert_eq!(uses[0].name, "Bash");
        assert_eq!(uses[0].run_in_background, Some(true));
        assert_eq!(uses[0].subagent_type, None);
        assert_eq!(uses[0].file_path, None);
        assert!(uses[0].questions.is_none());
        assert_eq!(uses[0].input, parse(r#"{"command":"ls","run_in_background":true}"#));
        assert_eq!(uses[1].run_in_background, None);
        assert_eq!(uses[1].subagent_type.as_deref(), Some("Explore"));
        assert_eq!(uses[2].file_path.as_deref(), Some("/x.py"));
        let questions = uses[3].questions.as_deref().unwrap();
        assert_eq!(questions.len(), 1, "question without string text is dropped");
        assert_eq!(questions[0].question, "Q?");
        assert_eq!(questions[0].header.as_deref(), Some("H"));
        assert!(questions[0].multi_select);
        assert_eq!(questions[0].labels, ["A"], "non-string labels are dropped");
        assert_eq!(joined_text(&assistant.blocks), "on it");
    }

    #[test]
    fn assistant_text_block_without_text_errors() {
        let raw = format!(
            r#"{{"type":"assistant",{META},"message":{{"model":"m1","content":[{{"type":"text"}}]}}}}"#
        );
        assert!(matches!(
            parse_entry(parse(&raw)).unwrap_err(),
            ParseError::Key(key) if key == "text"
        ));
    }

    #[test]
    fn mode_and_permission_mode_channels() {
        let mode = parse_entry(parse(r#"{"type":"mode","sessionId":"s1","mode":"plan"}"#)).unwrap();
        let Entry::Mode(m) = &mode else { panic!("expected mode entry") };
        assert_eq!(m.channel.as_str(), "mode");
        assert_eq!(m.value, "plan");
        assert_eq!(mode.session_id(), Some("s1"));
        assert!(mode.meta().is_none());

        let raw = r#"{"type":"permission-mode","sessionId":"s1","permissionMode":"acceptEdits"}"#;
        let Entry::Mode(m) = parse_entry(parse(raw)).unwrap() else {
            panic!("expected mode entry")
        };
        assert_eq!(m.channel.as_str(), "permission-mode");
        assert_eq!(m.value, "acceptEdits");
    }

    #[test]
    fn other_entry_keeps_raw_payload_verbatim() {
        let raw = r#"{"type":"queue-operation","operation":{"op":"enqueue","content":"later"}}"#;
        let Entry::Other(other) = parse_entry(parse(raw)).unwrap() else {
            panic!("expected other entry")
        };
        assert_eq!(other.ty, "queue-operation");
        assert_eq!(other.raw, parse(raw));
    }

    #[test]
    fn parse_bytes_skips_valid_json_non_object_lines() {
        let real = format!(r#"{{"type":"user",{META},"message":{{"content":"hi"}}}}"#);
        let bytes = format!("{real}\n42\n\"bare\"\n[1,2,3]\n{real}");
        let entries = parse_bytes(bytes.as_bytes(), |_| true).unwrap();
        assert_eq!(entries.len(), 2, "bare scalar and array lines are skipped");
        assert!(entries.iter().all(|e| matches!(e, Entry::User(_))));
    }

    #[test]
    fn print_envelope_requires_result_element() {
        assert!(matches!(
            parse_print_envelope(&parse(r#"{"a":1}"#)).unwrap_err(),
            ParseError::Value(msg) if msg == "envelope is not a JSON array"
        ));
        assert!(matches!(
            parse_print_envelope(&parse(r#"[{"type":"system"}]"#)).unwrap_err(),
            ParseError::Value(msg) if msg == "envelope has no result element"
        ));
    }
}
