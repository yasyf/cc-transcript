use std::mem::size_of;

use chrono::{DateTime, Datelike, FixedOffset, Timelike};
use memchr::memchr_iter;
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::protocol::{DENIAL_KIND_USER_REJECTED, DENIAL_PREFIX};
use crate::snapshot_ledger::hashbrown_tier;
use crate::snapshot_memory::{
    dom_parse_bound, pins_arena, value_charge, SourceArena, SONIC_SERIALIZER_BYTES,
};
use crate::types::{
    ApiError, AssistantEntry, AsyncHookResponse, AttachmentDetail, AttachmentEntry, Attribution,
    CacheCreation, CompactBoundary, ContentBlock, DeferredToolsDelta, Entry, EntryMeta,
    FallbackBlock, HookAdditionalContext, HookBlockingError, HookCancelled, HookInfo,
    HookNonBlockingError, HookSuccess, InitInfo, McpServer, ModeChannel, ModeEntry,
    ModelRefusalFallback, ModelUsage, OtherEntry, Plugin, PreservedMessages, PreservedSegment,
    PrintBody, PrintMessage, PrintResult, Question, QueuedCommand, ServerToolUse, StopHookSummary,
    SystemDetail, SystemEntry, ToolResultBlock, ToolUseBlock, TurnDuration, Usage, UserContent,
    UserEntry,
};
use crate::value::{
    block_type, deduplicated, field, field_bool, field_str, field_truthy, is_py_truthy,
    normalized_owned, subtree_has_duplicate_keys,
};

const AVG_LINE_BYTES: usize = 1400;

/// A malformed entry or envelope. Mapped to the matching Python exception
/// (``KeyError`` / ``ValueError``) at the pyo3 boundary in ``event.rs``.
#[derive(Debug)]
pub enum ParseError {
    Key(String),
    Value(String),
}

#[derive(Debug, Default)]
pub(crate) struct Retained {
    line: bool,
    private: Vec<SourceArena>,
}

impl Retained {
    fn share(&mut self, value: &Value) -> Value {
        self.line |= pins_arena(value);
        value.clone()
    }

    fn normalized(&mut self, value: &Value) -> Value {
        match deduplicated(value) {
            Some((normalized, source_len)) => {
                self.private.push(SourceArena {
                    root: normalized.clone(),
                    source_len,
                });
                normalized
            }
            None => self.share(value),
        }
    }

    pub(crate) fn arenas(self, root: Value, source_len: usize) -> Vec<SourceArena> {
        let Self { line, mut private } = self;
        if line {
            private.push(SourceArena { root, source_len });
        }
        private
    }
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
    let dt = DateTime::parse_from_rfc3339(raw)
        .map_err(|e| ParseError::Value(format!("invalid timestamp {raw:?}: {e}")))?;
    // Parity: Python datetime.fromisoformat rejects a :60 leap second (ValueError); chrono
    // accepts it, carrying the extra second as nanosecond >= 1e9. Reject to match.
    if dt.nanosecond() >= 1_000_000_000 {
        return Err(ParseError::Value(format!(
            "invalid timestamp {raw:?}: leap second"
        )));
    }
    // Parity: Python datetime is µs-precision — truncate sub-µs so near-tie events keep
    // Python's stable sort order (chrono otherwise retains nanoseconds).
    Ok(dt
        .with_nanosecond(dt.nanosecond() / 1000 * 1000)
        .expect("µs-truncated nanos are valid"))
}

fn require_array(content: &Value) -> Result<&[Value], ParseError> {
    content.as_array().map(|a| a.as_slice()).ok_or_else(|| {
        ParseError::Value("message content is neither a string nor a list".to_string())
    })
}

// Python type name (`type(x).__name__`) of a JSON value, for parity error messages.
fn py_type_name(value: &Value) -> &'static str {
    match value.get_type() {
        JsonType::Null => "NoneType",
        JsonType::Boolean => "bool",
        JsonType::Number if value.as_i64().is_some() || value.as_u64().is_some() => "int",
        JsonType::Number => "float",
        JsonType::String => "str",
        JsonType::Array => "list",
        JsonType::Object => "dict",
    }
}

fn flatten_result_content(content: &Value) -> Result<String, ParseError> {
    if let Some(s) = content.as_str() {
        return Ok(s.to_string());
    }
    // Parity: Python flatten_result_content raises ValueError on any non-str/non-list shape;
    // mirror it so a malformed tool-result content fails the whole file, not an empty string.
    match content.as_array() {
        Some(blocks) => Ok(blocks
            .iter()
            .filter(|b| block_type(b) == Some("text"))
            .filter_map(|b| field_str(b, "text"))
            .collect()),
        None => Err(ParseError::Value(format!(
            "unexpected result content shape: {}",
            py_type_name(content)
        ))),
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
        is_sidechain: field_truthy(data, "isSidechain"),
        is_meta: field_truthy(data, "isMeta"),
        entrypoint: field_str(data, "entrypoint").map(str::to_string),
        is_compact_summary: field_truthy(data, "isCompactSummary"),
        is_visible_in_transcript_only: field_truthy(data, "isVisibleInTranscriptOnly"),
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
    if !field_truthy(data, "isApiErrorMessage") {
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
    retained: &mut Retained,
) -> Result<ContentBlock, ParseError> {
    let content = flatten_result_content(require(block, "content")?)?;
    let is_error = field_truthy(block, "is_error");
    let denial_kind = tool_denial_kind.map(str::to_string).or_else(|| {
        (is_error && content.starts_with(DENIAL_PREFIX))
            .then(|| DENIAL_KIND_USER_REJECTED.to_string())
    });
    Ok(ContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: require_str(block, "tool_use_id")?.to_string(),
        content,
        is_error,
        is_async: tool_use_result.is_some_and(|tur| field_bool(tur, "isAsync")),
        tool_use_result: tool_use_result.map(|value| retained.share(value)),
        denial_kind,
    }))
}

fn parse_user_content(
    content: &Value,
    tool_use_result: Option<&Value>,
    tool_denial_kind: Option<&str>,
    retained: &mut Retained,
) -> Result<UserContent, ParseError> {
    match content.as_str() {
        Some(s) => Ok(UserContent::Plain(s.to_string())),
        None => {
            let blocks = require_array(content)?
                .iter()
                .filter_map(|b| match block_type(b) {
                    Some("text") => {
                        field_str(b, "text").map(|t| Ok(ContentBlock::Text(t.to_string())))
                    }
                    Some("tool_result") => Some(parse_tool_result(
                        b,
                        tool_use_result,
                        tool_denial_kind,
                        retained,
                    )),
                    _ => None,
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(UserContent::Blocks(blocks))
        }
    }
}

pub fn parse_questions(input: &Value) -> Option<Vec<Question>> {
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

fn parse_assistant_block(
    block: &Value,
    retained: &mut Retained,
) -> Result<ContentBlock, ParseError> {
    match block_type(block) {
        Some("text") => Ok(ContentBlock::Text(require_str(block, "text")?.to_string())),
        Some("thinking") => Ok(ContentBlock::Thinking(
            require_str(block, "thinking")?.to_string(),
        )),
        Some("tool_use") => {
            let id = require_str(block, "id")?.to_string();
            let name = require_str(block, "name")?.to_string();
            let input = retained.normalized(require(block, "input")?);
            Ok(ContentBlock::ToolUse(ToolUseBlock {
                id,
                name,
                run_in_background: field(&input, "run_in_background")
                    .and_then(JsonValueTrait::as_bool),
                subagent_type: field_str(&input, "subagent_type").map(str::to_string),
                file_path: field_str(&input, "file_path").map(str::to_string),
                questions: parse_questions(&input),
                input,
            }))
        }
        Some("fallback") => Ok(ContentBlock::Fallback(FallbackBlock {
            from_model: require_str(require(block, "from")?, "model")?.to_string(),
            to_model: require_str(require(block, "to")?, "model")?.to_string(),
        })),
        Some(other) => Ok(ContentBlock::Other {
            ty: other.to_string(),
            raw: retained.normalized(block),
        }),
        None => Err(ParseError::Key("type".to_string())),
    }
}

fn parse_assistant_blocks(
    content: &Value,
    retained: &mut Retained,
) -> Result<Vec<ContentBlock>, ParseError> {
    require_array(content)?
        .iter()
        .map(|block| parse_assistant_block(block, retained))
        .collect()
}

fn parse_usage(message: &Value) -> Result<Option<Usage>, ParseError> {
    // Parity: Python drops a falsy `usage` (`... if (usage := msg.get("usage")) else None`),
    // so an empty `{}` yields None rather than erroring on the missing token fields.
    match field(message, "usage").filter(|&u| is_py_truthy(u)) {
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

fn parse_system_detail(data: &Value, retained: &mut Retained) -> SystemDetail {
    match field_str(data, "subtype") {
        Some("stop_hook_summary") => SystemDetail::StopHookSummary(StopHookSummary {
            hook_count: opt_i64(data, "hookCount"),
            hook_infos: parse_hook_infos(data),
            hook_errors: str_array(data, "hookErrors"),
            hook_additional_context: str_array(data, "hookAdditionalContext"),
            prevented_continuation: field_truthy(data, "preventedContinuation"),
            stop_reason: opt_str(data, "stopReason"),
            has_output: field_truthy(data, "hasOutput"),
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
        Some("model_refusal_fallback") => {
            SystemDetail::ModelRefusalFallback(ModelRefusalFallback {
                api_refusal_category: opt_str(data, "apiRefusalCategory"),
                api_refusal_explanation: opt_str(data, "apiRefusalExplanation"),
                trigger: opt_str(data, "trigger"),
                direction: opt_str(data, "direction"),
                original_model: opt_str(data, "originalModel"),
                fallback_model: opt_str(data, "fallbackModel"),
                retracted_message_uuids: str_array(data, "retractedMessageUuids"),
                refused_user_message_uuid: truthy_str(data, "refusedUserMessageUuid")
                    .map(str::to_string),
            })
        }
        _ => SystemDetail::Other(retained.share(data)),
    }
}

fn parse_attachment_detail(data: &Value, retained: &mut Retained) -> AttachmentDetail {
    let empty = Value::default();
    let att = field(data, "attachment").unwrap_or(&empty);
    match field_str(att, "type") {
        Some("hook_success") => AttachmentDetail::HookSuccess(HookSuccess {
            hook_name: opt_str(att, "hookName"),
            hook_event: opt_str(att, "hookEvent"),
            tool_use_id: truthy_str(att, "toolUseID").map(str::to_string),
            command: opt_str(att, "command"),
            content: opt_str(att, "content"),
            stdout: opt_str(att, "stdout"),
            stderr: opt_str(att, "stderr"),
            exit_code: opt_i64(att, "exitCode"),
            duration_ms: opt_i64(att, "durationMs"),
        }),
        Some("hook_blocking_error") => AttachmentDetail::HookBlockingError(HookBlockingError {
            hook_name: opt_str(att, "hookName"),
            hook_event: opt_str(att, "hookEvent"),
            tool_use_id: truthy_str(att, "toolUseID").map(str::to_string),
            blocking_error: field(att, "blockingError").map(|value| retained.normalized(value)),
        }),
        Some("hook_non_blocking_error") => {
            AttachmentDetail::HookNonBlockingError(HookNonBlockingError {
                hook_name: opt_str(att, "hookName"),
                hook_event: opt_str(att, "hookEvent"),
                tool_use_id: truthy_str(att, "toolUseID").map(str::to_string),
                command: opt_str(att, "command"),
                stdout: opt_str(att, "stdout"),
                stderr: opt_str(att, "stderr"),
                exit_code: opt_i64(att, "exitCode"),
                duration_ms: opt_i64(att, "durationMs"),
            })
        }
        Some("hook_cancelled") => AttachmentDetail::HookCancelled(HookCancelled {
            hook_name: opt_str(att, "hookName"),
            hook_event: opt_str(att, "hookEvent"),
            tool_use_id: truthy_str(att, "toolUseID").map(str::to_string),
            command: opt_str(att, "command"),
            duration_ms: opt_i64(att, "durationMs"),
            timed_out: field(att, "timedOut").and_then(JsonValueTrait::as_bool),
            timeout_ms: opt_i64(att, "timeoutMs"),
        }),
        Some("hook_additional_context") => {
            AttachmentDetail::HookAdditionalContext(HookAdditionalContext {
                hook_name: opt_str(att, "hookName"),
                hook_event: opt_str(att, "hookEvent"),
                tool_use_id: truthy_str(att, "toolUseID").map(str::to_string),
                content: str_array(att, "content"),
            })
        }
        Some("async_hook_response") => AttachmentDetail::AsyncHookResponse(AsyncHookResponse {
            hook_name: opt_str(att, "hookName"),
            hook_event: opt_str(att, "hookEvent"),
            process_id: opt_str(att, "processId"),
            stdout: opt_str(att, "stdout"),
            stderr: opt_str(att, "stderr"),
            exit_code: opt_i64(att, "exitCode"),
            response: field(att, "response").map(|value| retained.normalized(value)),
        }),
        Some("queued_command") => AttachmentDetail::QueuedCommand(QueuedCommand {
            prompt: opt_str(att, "prompt"),
            command_mode: opt_str(att, "commandMode"),
            origin: field(att, "origin").and_then(|origin| opt_str(origin, "kind")),
        }),
        Some("deferred_tools_delta") => AttachmentDetail::DeferredToolsDelta(DeferredToolsDelta {
            added_names: str_array(att, "addedNames"),
            removed_names: str_array(att, "removedNames"),
            raw: retained.share(data),
        }),
        _ => AttachmentDetail::Other(retained.share(data)),
    }
}

/// Parse one JSONL transcript line into the typed model. Consumes the value so
/// unrecognized entry kinds keep their payload verbatim without a copy.
pub fn parse_entry(data: Value) -> Result<Entry, ParseError> {
    parse_entry_retained(data, &mut Retained::default())
}

pub(crate) fn parse_entry_retained(
    data: Value,
    retained: &mut Retained,
) -> Result<Entry, ParseError> {
    // Root-level dup keys read first-wins in Rust (Python/orjson: last-wins) — accepted
    // divergence; details on task e0ab2411 item 9.
    let ty = require_str(&data, "type")?.to_string();
    match ty.as_str() {
        "user" => {
            let tool_use_result = field(&data, "toolUseResult");
            // Parity: Python `tool_denial_kind or (...)` treats an empty string as absent.
            let tool_denial_kind = truthy_str(&data, "toolDenialKind");
            let content = parse_user_content(
                require(require(&data, "message")?, "content")?,
                tool_use_result,
                tool_denial_kind,
                retained,
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
                mcp_meta: field(&data, "mcpMeta").map(|value| retained.normalized(value)),
                permission_mode: field_str(&data, "permissionMode").map(str::to_string),
                interrupted_message_id: field_str(&data, "interruptedMessageId")
                    .map(str::to_string),
            }));
        }
        "assistant" => {
            let message = require(&data, "message")?;
            let blocks = parse_assistant_blocks(require(message, "content")?, retained)?;
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
                detail: parse_system_detail(&data, retained),
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
        "attachment" => {
            let detail = parse_attachment_detail(&data, retained);
            let attachment_type = field(&data, "attachment")
                .and_then(|att| field_str(att, "type"))
                .unwrap_or("")
                .to_string();
            return Ok(Entry::Attachment(AttachmentEntry {
                meta: parse_meta(&data)?,
                attachment_type,
                detail,
            }));
        }
        _ => {}
    }
    let raw = retained.normalized(&data);
    Ok(Entry::Other(OtherEntry { ty, raw }))
}

pub(crate) fn pushed_capacity(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        len.next_power_of_two().max(4)
    }
}

fn checked_sum(parts: impl IntoIterator<Item = usize>) -> Option<usize> {
    parts.into_iter().try_fold(0usize, usize::checked_add)
}

fn widest(parts: impl IntoIterator<Item = Option<usize>>) -> Option<usize> {
    parts
        .into_iter()
        .try_fold(0usize, |widest, part| Some(widest.max(part?)))
}

fn str_len(data: &Value, key: &str) -> usize {
    field_str(data, key).map_or(0, str::len)
}

fn truthy_len(data: &Value, key: &str) -> usize {
    truthy_str(data, key).map_or(0, str::len)
}

fn values_of<'a>(data: &'a Value, key: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    #[cfg(test)]
    crate::snapshot_memory::count_dom_work(data.as_object().map_or(1, |object| object.len()));
    data.as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(move |(name, _)| *name == key)
        .map(|(_, value)| value)
}

fn widest_str(data: &Value, key: &str) -> usize {
    values_of(data, key)
        .filter_map(JsonValueTrait::as_str)
        .map(str::len)
        .max()
        .unwrap_or(0)
}

fn names_str(data: &Value, key: &str) -> bool {
    values_of(data, key).any(|value| value.as_str().is_some())
}

fn dom_bytes(value: &Value) -> usize {
    value_charge(value).opaque_dom_accounted_bytes
}

fn str_array_bound(data: &Value, key: &str) -> Option<usize> {
    let (count, bytes) = field(data, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .fold((0, 0), |(count, bytes), item| {
            (count + 1, bytes + item.len())
        });
    pushed_capacity(count)
        .checked_mul(size_of::<String>())?
        .checked_add(bytes)
}

fn joined_text_bound(total: usize) -> Option<usize> {
    if total == 0 {
        Some(0)
    } else {
        total.checked_mul(2).map(|doubled| doubled.max(8))
    }
}

fn leaf_temp(serialized: usize) -> Option<usize> {
    serialized
        .checked_mul(2)
        .map(|doubled| doubled.max(SONIC_SERIALIZER_BYTES))
}

fn serialized_bound(value: &Value) -> Option<usize> {
    #[cfg(test)]
    crate::snapshot_memory::count_dom_work(1);
    match value.get_type() {
        JsonType::Null => Some(4),
        JsonType::Boolean => Some(5),
        JsonType::Number => Some(
            value
                .as_raw_number()
                .map_or(0, |number| number.as_str().len()),
        ),
        JsonType::String => value.as_str().unwrap().len().checked_mul(6)?.checked_add(2),
        JsonType::Array => {
            let array = value.as_array().unwrap();
            array
                .iter()
                .try_fold(array.len().checked_add(2)?, |total, item| {
                    total.checked_add(serialized_bound(item)?)
                })
        }
        JsonType::Object => {
            value
                .as_object()
                .unwrap()
                .iter()
                .try_fold(2usize, |total, (key, item)| {
                    total
                        .checked_add(key.len().checked_mul(6)?.checked_add(4)?)?
                        .checked_add(serialized_bound(item)?)
                })
        }
    }
}

fn seen_scratch(value: &Value) -> Option<usize> {
    #[cfg(test)]
    crate::snapshot_memory::count_dom_work(1);
    if let Some(object) = value.as_object() {
        let own = pushed_capacity(object.len()).checked_mul(size_of::<&str>())?;
        object.iter().try_fold(own, |deepest, (_, item)| {
            Some(deepest.max(own.checked_add(seen_scratch(item)?)?))
        })
    } else if let Some(array) = value.as_array() {
        widest(array.iter().map(seen_scratch))
    } else {
        Some(0)
    }
}

fn write_scratch(value: &Value) -> Option<usize> {
    #[cfg(test)]
    crate::snapshot_memory::count_dom_work(1);
    if let Some(object) = value.as_object() {
        let pair = size_of::<(&str, &Value)>();
        let pairs = object.len().checked_mul(pair)?;
        let dedupe = pushed_capacity(object.len())
            .checked_mul(size_of::<&str>())?
            .checked_add(hashbrown_tier(object.len(), pair).checked_mul(pair)?)?;
        let deepest = object.iter().try_fold(dedupe, |deepest, (key, item)| {
            Some(
                deepest
                    .max(leaf_temp(key.len().checked_mul(6)?.checked_add(2)?)?)
                    .max(write_scratch(item)?),
            )
        })?;
        pairs.checked_add(deepest)
    } else if let Some(array) = value.as_array() {
        widest(array.iter().map(write_scratch))
    } else {
        leaf_temp(serialized_bound(value)?)
    }
}

fn normalized_bound(value: Option<&Value>) -> Option<usize> {
    let Some(value) = value else {
        return Some(0);
    };
    let dedupe = if subtree_has_duplicate_keys(value) {
        let serialized = serialized_bound(value)?;
        checked_sum([
            joined_text_bound(serialized)?,
            write_scratch(value)?,
            dom_parse_bound(serialized)?,
        ])?
    } else {
        0
    };
    checked_sum([dom_bytes(value), seen_scratch(value)?, dedupe])
}

fn meta_bound(data: &Value) -> usize {
    str_len(data, "uuid")
        + truthy_len(data, "parentUuid")
        + str_len(data, "sessionId")
        + str_len(data, "cwd")
        + str_len(data, "gitBranch")
        + truthy_len(data, "version")
        + str_len(data, "entrypoint")
        + str_len(data, "userType")
        + str_len(data, "slug")
}

fn image_paste_ids_bound(data: &Value) -> Option<usize> {
    let ids = field(data, "imagePasteIds")
        .and_then(JsonContainerTrait::as_array)
        .map_or(0, |ids| {
            ids.iter().filter(|id| id.as_i64().is_some()).count()
        });
    pushed_capacity(ids).checked_mul(size_of::<i64>())
}

fn result_content_bound(content: &Value) -> Option<usize> {
    if let Some(text) = content.as_str() {
        return Some(text.len());
    }
    let Some(blocks) = content.as_array() else {
        return Some(0);
    };
    joined_text_bound(
        blocks
            .iter()
            .filter(|block| block_type(block) == Some("text"))
            .filter_map(|block| field_str(block, "text"))
            .map(str::len)
            .sum(),
    )
}

fn user_content_bound(data: &Value, content: &Value) -> Option<usize> {
    if let Some(text) = content.as_str() {
        return Some(text.len());
    }
    let Some(blocks) = content.as_array() else {
        return Some(0);
    };
    let empty = Value::default();
    let result_payload = field(data, "toolUseResult").map_or(0, dom_bytes);
    let denial =
        truthy_str(data, "toolDenialKind").map_or(DENIAL_KIND_USER_REJECTED.len(), str::len);
    let mut kept = 0usize;
    let mut results = 0usize;
    let mut bytes = 0usize;
    for block in blocks.iter() {
        match block_type(block) {
            Some("text") => {
                if let Some(text) = field_str(block, "text") {
                    kept += 1;
                    bytes = bytes.checked_add(text.len())?;
                }
            }
            Some("tool_result") => {
                kept += 1;
                results += 1;
                bytes = checked_sum([
                    bytes,
                    str_len(block, "tool_use_id"),
                    result_content_bound(field(block, "content").unwrap_or(&empty))?,
                    denial,
                ])?;
            }
            _ => {}
        }
    }
    checked_sum([
        pushed_capacity(kept).checked_mul(size_of::<ContentBlock>())?,
        bytes,
        results.checked_mul(result_payload)?,
    ])
}

fn labels_bound(options: &Value) -> Option<usize> {
    let Some(options) = options.as_array() else {
        return Some(0);
    };
    let (count, bytes) = options
        .iter()
        .filter(|option| names_str(option, "label"))
        .fold((0, 0), |(count, bytes), option| {
            (count + 1, bytes + widest_str(option, "label"))
        });
    pushed_capacity(count)
        .checked_mul(size_of::<String>())?
        .checked_add(bytes)
}

fn questions_bound(questions: &Value) -> Option<usize> {
    let Some(items) = questions.as_array() else {
        return Some(0);
    };
    let mut kept = 0usize;
    let mut bytes = 0usize;
    for item in items.iter().filter(|item| names_str(item, "question")) {
        kept += 1;
        bytes = checked_sum([
            bytes,
            widest_str(item, "question"),
            widest_str(item, "header"),
            widest(values_of(item, "options").map(labels_bound))?,
        ])?;
    }
    pushed_capacity(kept)
        .checked_mul(size_of::<Question>())?
        .checked_add(bytes)
}

fn tool_use_input_bound(input: &Value) -> Option<usize> {
    checked_sum([
        normalized_bound(Some(input))?,
        widest_str(input, "subagent_type"),
        widest_str(input, "file_path"),
        widest(values_of(input, "questions").map(questions_bound))?,
    ])
}

fn assistant_blocks_bound(content: &Value) -> Option<usize> {
    let Some(blocks) = content.as_array() else {
        return Some(0);
    };
    let empty = Value::default();
    let mut bytes = pushed_capacity(blocks.len()).checked_mul(size_of::<ContentBlock>())?;
    for block in blocks.iter() {
        let block_bytes = match block_type(block) {
            Some("text") => str_len(block, "text"),
            Some("thinking") => str_len(block, "thinking"),
            Some("tool_use") => checked_sum([
                str_len(block, "id"),
                str_len(block, "name"),
                tool_use_input_bound(field(block, "input").unwrap_or(&empty))?,
            ])?,
            Some("fallback") => {
                str_len(field(block, "from").unwrap_or(&empty), "model")
                    + str_len(field(block, "to").unwrap_or(&empty), "model")
            }
            Some(other) => checked_sum([other.len(), normalized_bound(Some(block))?])?,
            None => 0,
        };
        bytes = bytes.checked_add(block_bytes)?;
    }
    Some(bytes)
}

fn usage_bound(message: &Value) -> usize {
    field(message, "usage")
        .filter(|usage| is_py_truthy(usage))
        .map_or(0, |usage| {
            str_len(usage, "service_tier") + str_len(usage, "inference_geo")
        })
}

fn api_error_bound(data: &Value) -> usize {
    if field_truthy(data, "isApiErrorMessage") {
        str_len(data, "error") + str_len(data, "errorDetails")
    } else {
        0
    }
}

fn hook_infos_bound(data: &Value) -> Option<usize> {
    let (count, bytes) = field(data, "hookInfos")
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(|info| field_str(info, "command"))
        .fold((0, 0), |(count, bytes), command| {
            (count + 1, bytes + command.len())
        });
    pushed_capacity(count)
        .checked_mul(size_of::<HookInfo>())?
        .checked_add(bytes)
}

fn system_detail_bound(data: &Value) -> Option<usize> {
    let empty = Value::default();
    match field_str(data, "subtype") {
        Some("stop_hook_summary") => checked_sum([
            hook_infos_bound(data)?,
            str_array_bound(data, "hookErrors")?,
            str_array_bound(data, "hookAdditionalContext")?,
            str_len(data, "stopReason"),
            truthy_len(data, "toolUseID"),
        ]),
        Some("compact_boundary") => {
            let metadata = field(data, "compactMetadata").unwrap_or(&empty);
            let segment = field(metadata, "preservedSegment")
                .filter(|segment| segment.is_object())
                .map_or(0, |segment| {
                    truthy_len(segment, "headUuid")
                        + truthy_len(segment, "anchorUuid")
                        + truthy_len(segment, "tailUuid")
                });
            let messages = match field(metadata, "preservedMessages")
                .filter(|messages| messages.is_object())
            {
                Some(messages) => checked_sum([
                    truthy_len(messages, "anchorUuid"),
                    str_array_bound(messages, "uuids")?,
                    str_array_bound(messages, "allUuids")?,
                ])?,
                None => 0,
            };
            checked_sum([
                str_len(metadata, "trigger"),
                str_array_bound(metadata, "preCompactDiscoveredTools")?,
                segment,
                messages,
                truthy_len(data, "logicalParentUuid"),
            ])
        }
        Some("turn_duration") => Some(0),
        Some("model_refusal_fallback") => checked_sum([
            str_len(data, "apiRefusalCategory"),
            str_len(data, "apiRefusalExplanation"),
            str_len(data, "trigger"),
            str_len(data, "direction"),
            str_len(data, "originalModel"),
            str_len(data, "fallbackModel"),
            str_array_bound(data, "retractedMessageUuids")?,
            truthy_len(data, "refusedUserMessageUuid"),
        ]),
        _ => Some(dom_bytes(data)),
    }
}

fn attachment_detail_bound(data: &Value, att: &Value) -> Option<usize> {
    let hook = str_len(att, "hookName") + str_len(att, "hookEvent") + truthy_len(att, "toolUseID");
    match field_str(att, "type") {
        Some("hook_success") => Some(
            hook + str_len(att, "command")
                + str_len(att, "content")
                + str_len(att, "stdout")
                + str_len(att, "stderr"),
        ),
        Some("hook_blocking_error") => {
            hook.checked_add(normalized_bound(field(att, "blockingError"))?)
        }
        Some("hook_non_blocking_error") => {
            Some(hook + str_len(att, "command") + str_len(att, "stdout") + str_len(att, "stderr"))
        }
        Some("hook_cancelled") => Some(hook + str_len(att, "command")),
        Some("hook_additional_context") => hook.checked_add(str_array_bound(att, "content")?),
        Some("async_hook_response") => checked_sum([
            str_len(att, "hookName"),
            str_len(att, "hookEvent"),
            str_len(att, "processId"),
            str_len(att, "stdout"),
            str_len(att, "stderr"),
            normalized_bound(field(att, "response"))?,
        ]),
        Some("queued_command") => Some(
            str_len(att, "prompt")
                + str_len(att, "commandMode")
                + field(att, "origin").map_or(0, |origin| str_len(origin, "kind")),
        ),
        Some("deferred_tools_delta") => checked_sum([
            str_array_bound(att, "addedNames")?,
            str_array_bound(att, "removedNames")?,
            dom_bytes(data),
        ]),
        _ => Some(dom_bytes(data)),
    }
}

pub(crate) fn retained_entry_bound(data: &Value) -> Option<usize> {
    let empty = Value::default();
    let ty = field_str(data, "type").unwrap_or("");
    match ty {
        "user" => {
            let message = field(data, "message").unwrap_or(&empty);
            checked_sum([
                ty.len(),
                meta_bound(data),
                user_content_bound(data, field(message, "content").unwrap_or(&empty))?,
                str_len(data, "promptId"),
                str_len(data, "promptSource"),
                str_len(data, "queuePriority"),
                image_paste_ids_bound(data)?,
                truthy_len(data, "sourceToolUseID"),
                truthy_len(data, "sourceToolAssistantUUID"),
                normalized_bound(field(data, "mcpMeta"))?,
                str_len(data, "permissionMode"),
                str_len(data, "interruptedMessageId"),
            ])
        }
        "assistant" => {
            let message = field(data, "message").unwrap_or(&empty);
            checked_sum([
                ty.len(),
                meta_bound(data),
                str_len(message, "model"),
                assistant_blocks_bound(field(message, "content").unwrap_or(&empty))?,
                str_len(message, "stop_reason"),
                usage_bound(message),
                str_len(data, "requestId"),
                str_len(data, "forkedFrom"),
                str_len(data, "attributionPlugin"),
                str_len(data, "attributionSkill"),
                str_len(data, "attributionMcpServer"),
                str_len(data, "attributionMcpTool"),
                api_error_bound(data),
            ])
        }
        "system" => checked_sum([
            ty.len(),
            meta_bound(data),
            str_len(data, "subtype"),
            str_len(data, "content"),
            str_len(data, "level"),
            system_detail_bound(data)?,
        ]),
        "mode" => Some(ty.len() + str_len(data, "sessionId") + str_len(data, "mode")),
        "permission-mode" => {
            Some(ty.len() + str_len(data, "sessionId") + str_len(data, "permissionMode"))
        }
        "attachment" => {
            let att = field(data, "attachment").unwrap_or(&empty);
            checked_sum([
                ty.len(),
                meta_bound(data),
                str_len(att, "type"),
                attachment_detail_bound(data, att)?,
            ])
        }
        _ => checked_sum([ty.len(), normalized_bound(Some(data))?]),
    }
}

pub(crate) fn retained_arena_bound(data: &Value) -> usize {
    1 + match field_str(data, "type").unwrap_or("") {
        "assistant" => field(data, "message")
            .and_then(|message| field(message, "content"))
            .and_then(JsonContainerTrait::as_array)
            .map_or(0, |blocks| blocks.len()),
        "system" | "mode" | "permission-mode" => 0,
        _ => 1,
    }
}

// Non-JSON lines and valid-JSON lines that are not objects (bare scalars or
// arrays) are skipped; a JSON object that fails the typed parse (e.g. a missing
// required field) fails the whole file — whole-file parity with PythonBackend,
// which decodes every line, skips non-objects, then parses the rest.
pub(crate) fn parse_line_dom(line: &[u8]) -> Option<Value> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    sonic_rs::from_slice::<Value>(line)
        .ok()
        .filter(JsonValueTrait::is_object)
}

pub(crate) fn entry_from_dom<F: Fn(&Entry) -> bool>(
    value: Value,
    retained: &mut Retained,
    keep: &F,
) -> Result<Option<Entry>, ParseError> {
    let entry = parse_entry_retained(value, retained)?;
    // Parity: a timestamp below Python datetime.MINYEAR can never convert to
    // a Python datetime — drop the line, keep the file.
    if entry.meta().is_some_and(|m| m.timestamp.year() < 1) {
        return Ok(None);
    }
    Ok(keep(&entry).then_some(entry))
}

pub(crate) fn parse_line<F: Fn(&Entry) -> bool>(
    line: &[u8],
    lines: &mut Vec<Entry>,
    keep: &F,
) -> Result<(), ParseError> {
    if let Some(entry) = parse_line_dom(line)
        .map(|value| entry_from_dom(value, &mut Retained::default(), keep))
        .transpose()?
        .flatten()
    {
        lines.push(entry);
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
    let mut retained = Retained::default();
    let body = match role {
        "assistant" => PrintBody::Assistant {
            blocks: parse_assistant_blocks(require(message, "content")?, &mut retained)?,
            model: field_str(message, "model").map(str::to_string),
        },
        "user" => PrintBody::User(parse_user_content(
            require(message, "content")?,
            None,
            None,
            &mut retained,
        )?),
        other => {
            return Err(ParseError::Value(format!(
                "not a conversational element: {other:?}"
            )))
        }
    };
    Ok(PrintMessage {
        body,
        id: field_str(message, "id").map(str::to_string),
        usage: parse_usage(message)?,
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
        .map(normalized_owned);
    let permission_denials = require(result, "permission_denials")?
        .as_array()
        .ok_or_else(|| ParseError::Key("permission_denials".to_string()))?
        .iter()
        .map(normalized_owned)
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
        assert_eq!(flatten_result_content(&parse("\"hi\"")).unwrap(), "hi");
        let blocks =
            parse(r#"[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]"#);
        assert_eq!(flatten_result_content(&blocks).unwrap(), "ab");
        // Parity: a non-str/non-list content shape raises (Python flatten_result_content).
        for bad in ["{\"x\":1}", "42", "null", "true"] {
            assert!(
                matches!(
                    flatten_result_content(&parse(bad)),
                    Err(ParseError::Value(_))
                ),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn duplicate_identity_keys_stay_first_wins_accepted_divergence() {
        // Accepted divergence (e0ab2411 item 9): every root-level dup key reads first-wins in
        // Rust (Python/orjson: last-wins) — type dispatch (both orders) and identity value.
        let queue_then_user = parse_entry(parse(
            r#"{"type":"queue-operation","operation":"enqueue","type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"role":"user","content":"hi"}}"#,
        ))
        .unwrap();
        assert!(matches!(queue_then_user, Entry::Other(o) if o.ty == "queue-operation"));

        let user_then_queue = parse_entry(parse(
            r#"{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"role":"user","content":"hi"},"type":"queue-operation","operation":"enqueue"}"#,
        ))
        .unwrap();
        assert!(matches!(user_then_queue, Entry::User(_)));

        let entry = parse_entry(parse(
            r#"{"type":"user","uuid":"first","uuid":"second","sessionId":"s1","sessionId":"s2","timestamp":"2026-01-02T03:04:05Z","message":{"role":"user","content":"hi"}}"#,
        ))
        .unwrap();
        let meta = entry.meta().unwrap();
        assert_eq!(meta.uuid, "first");
        assert_eq!(meta.session_id, "s1");
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
        assert!(field_bool(
            result.tool_use_result.as_ref().unwrap(),
            "isAsync"
        ));
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
    fn user_interrupted_message_id_detected() {
        // No text marker, but a present interruptedMessageId still counts as interrupted.
        let raw = format!(
            r#"{{"type":"user",{META},"interruptedMessageId":"m1","message":{{"content":"plain text"}}}}"#
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
        assert_eq!(
            uses[0].input,
            parse(r#"{"command":"ls","run_in_background":true}"#)
        );
        assert_eq!(uses[1].run_in_background, None);
        assert_eq!(uses[1].subagent_type.as_deref(), Some("Explore"));
        assert_eq!(uses[2].file_path.as_deref(), Some("/x.py"));
        let questions = uses[3].questions.as_deref().unwrap();
        assert_eq!(
            questions.len(),
            1,
            "question without string text is dropped"
        );
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
        let Entry::Mode(m) = &mode else {
            panic!("expected mode entry")
        };
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
    fn parse_bytes_drops_year_zero_timestamp_line_keeps_file() {
        // A timestamp below Python datetime.MINYEAR drops the line, not the file.
        let year_zero = r#"{"type":"user","uuid":"u0","parentUuid":null,"sessionId":"s","timestamp":"0000-01-01T00:00:00Z","message":{"content":"dropped"}}"#;
        let survivor = format!(r#"{{"type":"user",{META},"message":{{"content":"kept"}}}}"#);
        let bytes = format!("{year_zero}\n{survivor}");
        let entries = parse_bytes(bytes.as_bytes(), |_| true).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "the year-zero line drops, the file survives"
        );
        let Entry::User(user) = &entries[0] else {
            panic!("expected user entry")
        };
        assert_eq!(user.content.text(), "kept");
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

    #[test]
    fn parse_entry_normalizes_duplicate_keys_to_last_wins() {
        // Pre-extracted fields and first-wins reads of the retained input both see last-wins.
        let raw = format!(
            r#"{{"type":"assistant",{META},"message":{{"model":"m1","content":[
                {{"type":"tool_use","id":"t1","name":"Edit","input":{{"file_path":"/first.py","file_path":"/last.py","old_string":"a","old_string":"b"}}}}
            ]}}}}"#
        );
        let entry = parse_entry(parse(&raw)).unwrap();
        let use_ = entry.tool_uses().next().unwrap();
        assert_eq!(use_.file_path.as_deref(), Some("/last.py"));
        assert_eq!(field_str(&use_.input, "file_path"), Some("/last.py"));
        assert_eq!(field_str(&use_.input, "old_string"), Some("b"));
        assert_eq!(
            use_.input,
            parse(r#"{"file_path":"/last.py","old_string":"b"}"#)
        );
    }

    #[test]
    fn empty_usage_object_drops_to_none_like_python() {
        // Python `usage if (usage := msg.get("usage")) else None`: an empty {} is falsy, so
        // usage is None rather than an error on the absent token fields.
        let raw = format!(
            r#"{{"type":"assistant",{META},"message":{{"model":"m1","content":[],"usage":{{}}}}}}"#
        );
        let Entry::Assistant(a) = parse_entry(parse(&raw)).unwrap() else {
            panic!("expected assistant entry")
        };
        assert!(a.usage.is_none());
    }

    #[test]
    fn meta_bool_fields_coerce_by_python_truthiness() {
        // Python bool(data.get(...)): a non-bool truthy value is True (isSidechain:1), a
        // zero/empty value is False — not a strict as_bool type-filter that drops to False.
        let raw = format!(
            r#"{{"type":"user","uuid":"u1","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z","isSidechain":1,"isMeta":"yes","isCompactSummary":0,"message":{{"content":"hi"}}}}"#
        );
        let entry = parse_entry(parse(&raw)).unwrap();
        let meta = entry.meta().unwrap();
        assert!(meta.is_sidechain, "isSidechain:1 -> bool(1) is True");
        assert!(meta.is_meta, "isMeta:\"yes\" -> bool(\"yes\") is True");
        assert!(
            !meta.is_compact_summary,
            "isCompactSummary:0 -> bool(0) is False"
        );
    }

    #[test]
    fn tool_result_error_truthiness_and_empty_denial_kind() {
        // is_error coerces via Python bool() (1 -> True); an empty toolDenialKind is treated
        // as absent (Python `x or ...`), so a non-denial error keeps denial_kind None.
        let raw = format!(
            r#"{{"type":"user",{META},"toolDenialKind":"","message":{{"content":[
                {{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":1}}
            ]}}}}"#
        );
        let entry = parse_entry(parse(&raw)).unwrap();
        let result = entry.tool_results().next().unwrap();
        assert!(result.is_error, "is_error:1 -> bool(1) is True");
        assert_eq!(
            result.denial_kind, None,
            "empty toolDenialKind falls through"
        );
    }

    #[test]
    fn tool_result_non_str_non_list_content_errors_like_python() {
        // Python flatten_result_content raises ValueError on a non-str/non-list content
        // shape; the Rust parse must fail the whole file rather than yield empty content.
        let raw = format!(
            r#"{{"type":"user",{META},"message":{{"content":[
                {{"type":"tool_result","tool_use_id":"t1","content":{{"foo":"bar"}}}}
            ]}}}}"#
        );
        assert!(matches!(
            parse_entry(parse(&raw)).unwrap_err(),
            ParseError::Value(_)
        ));
    }

    #[test]
    fn parse_timestamp_rejects_leap_second_like_python() {
        // Python datetime.fromisoformat raises on a :60 leap second; chrono clamps it, so
        // reject to keep whole-file parity.
        assert!(parse_timestamp("2026-06-30T23:59:60Z").is_err());
        assert!(parse_timestamp("2026-06-30T23:59:59Z").is_ok());
    }

    // Representation-blocked divergences (accepted, e0ab2411): the typed Entry field is the
    // contract; an impossible input shape reads None / fails vs Python's preserve.

    #[test]
    fn nonstring_cwd_reads_none_representation_blocked() {
        // cwd: Option<String> cannot hold Python's raw int, so a non-string cwd reads None.
        let raw = r#"{"type":"user","uuid":"u1","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z","cwd":7,"message":{"content":"hi"}}"#;
        let entry = parse_entry(parse(raw)).unwrap();
        assert_eq!(entry.meta().unwrap().cwd, None);
    }

    #[test]
    fn out_of_i64_and_negative_zero_tokens_fail_representation_blocked() {
        // input_tokens: i64 holds neither orjson's lossy float for a >i64 count nor the "-0"
        // sonic refuses as i64; both fail the file where Python materializes.
        for tok in ["99999999999999999999999999", "-0"] {
            let raw = format!(
                r#"{{"type":"assistant","uuid":"u1","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z","message":{{"model":"m1","content":[],"usage":{{"input_tokens":{tok},"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
            );
            assert!(parse_entry(parse(&raw)).is_err(), "token {tok} should fail");
        }
    }

    #[test]
    fn naive_offset_less_timestamp_fails_representation_blocked() {
        // timestamp: DateTime<FixedOffset> cannot hold Python's offset-less naive datetime.
        assert!(parse_timestamp("2026-01-02T03:04:05").is_err());
    }

    fn dom_work(op: impl FnOnce()) -> usize {
        crate::snapshot_memory::DOM_WORK.with(|work| work.set(0));
        op();
        crate::snapshot_memory::DOM_WORK.with(std::cell::Cell::get)
    }

    fn repeated_results_line(blocks: usize, payload: usize) -> String {
        let block = r#"{"type":"tool_result","tool_use_id":"call","content":"ok"}"#;
        let keys = (0..payload)
            .map(|key| format!(r#""k{key}":0"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"type":"user",{META},"message":{{"content":[{}]}},"toolDenialKind":"user","toolUseResult":{{"isAsync":false,"items":[{}],{keys}}}}}"#,
            vec![block; blocks].join(","),
            vec!["0"; payload].join(","),
        )
    }

    fn retained_arenas(raw: &str) -> Vec<usize> {
        let dom = parse(raw);
        let mut retained = Retained::default();
        parse_entry_retained(dom.clone(), &mut retained).unwrap();
        retained
            .arenas(dom, raw.len())
            .iter()
            .map(|arena| arena.source_len)
            .collect()
    }

    #[test]
    fn predictor_and_arena_walks_are_linear_in_the_line() {
        let walked = |blocks: usize, payload: usize| {
            let line = repeated_results_line(blocks, payload);
            let dom = parse(&line);
            let predicted = dom_work(|| {
                retained_entry_bound(&dom).unwrap();
                crate::snapshot_memory::arena_bytes(&dom, line.len());
                retained_arena_bound(&dom);
            });
            let charged = dom_work(|| {
                crate::snapshot_memory::entry_charge(&parse_entry(dom.clone()).unwrap());
            });
            assert!(
                predicted <= 2 * line.len(),
                "predicting a {}-byte line walked {predicted} units",
                line.len()
            );
            (predicted, charged)
        };
        let [few_small, many_small, few_large, many_large] =
            [(64, 64), (512, 64), (64, 1024), (512, 1024)]
                .map(|(blocks, payload)| walked(blocks, payload));
        assert_eq!(
            many_small.0 - few_small.0,
            many_large.0 - few_large.0,
            "the per-block predictor work depends on the shared result's size"
        );
        assert!(
            many_large.1 - few_large.1 > many_small.1 - few_small.1,
            "the counter does not see the per-block charge of the shared result"
        );
    }

    #[test]
    fn retained_parse_records_each_arena_an_entry_keeps() {
        let user = |extra: &str| {
            format!(
                r#"{{"type":"user",{META},"message":{{"content":[{{"type":"tool_result","tool_use_id":"c","content":"ok"}}]}}{extra}}}"#
            )
        };
        let assistant = |inputs: &str| {
            format!(
                r#"{{"type":"assistant",{META},"message":{{"model":"m","content":[{inputs}]}}}}"#
            )
        };
        let shared = user(r#","toolUseResult":{"stdout":"x"}"#);
        assert_eq!(retained_arenas(&shared), vec![shared.len()]);
        for unpinned in [
            user(""),
            user(r#","toolUseResult":true"#),
            user(r#","toolUseResult":{}"#),
            format!(r#"{{"type":"user",{META},"message":{{"content":"hi"}}}}"#),
        ] {
            assert_eq!(
                retained_arenas(&unpinned),
                Vec::<usize>::new(),
                "{unpinned}"
            );
        }
        let duplicated = assistant(
            r#"{"type":"tool_use","id":"t","name":"Bash","input":{"command":"a","command":"bb"}}"#,
        );
        assert_eq!(
            retained_arenas(&duplicated),
            vec![r#"{"command":"bb"}"#.len()]
        );
        let mixed = assistant(
            r#"{"type":"tool_use","id":"t","name":"Bash","input":{"command":"a","command":"bb"}},{"type":"tool_use","id":"u","name":"Bash","input":{"command":"c"}}"#,
        );
        assert_eq!(
            retained_arenas(&mixed),
            vec![r#"{"command":"bb"}"#.len(), mixed.len()]
        );
        let other = r#"{"type":"custom","k":1,"k":2}"#;
        assert_eq!(
            retained_arenas(other),
            vec![r#"{"type":"custom","k":2}"#.len()]
        );
    }
}
