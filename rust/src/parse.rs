use chrono::{DateTime, FixedOffset};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::types::{
    AssistantEntry, CacheCreation, ContentBlock, Entry, EntryMeta, FallbackBlock, InitInfo,
    McpServer, ModeChannel, ModeEntry, ModelUsage, OtherEntry, Plugin, PrintBody, PrintMessage,
    PrintResult, ServerToolUse, SystemEntry, ToolResultBlock, ToolUseBlock, Usage, UserContent,
    UserEntry,
};
use crate::value::{block_type, field, field_bool, field_str};

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
    })
}

fn parse_tool_result(block: &Value, is_async: bool) -> Result<ContentBlock, ParseError> {
    Ok(ContentBlock::ToolResult(ToolResultBlock {
        tool_use_id: require_str(block, "tool_use_id")?.to_string(),
        content: flatten_result_content(require(block, "content")?),
        is_error: field_bool(block, "is_error"),
        is_async,
    }))
}

fn parse_user_content(content: &Value, is_async: bool) -> Result<UserContent, ParseError> {
    match content.as_str() {
        Some(s) => Ok(UserContent::Plain(s.to_string())),
        None => {
            let blocks = require_array(content)?
                .iter()
                .filter_map(|b| match block_type(b) {
                    Some("text") => field_str(b, "text").map(|t| Ok(ContentBlock::Text(t.to_string()))),
                    Some("tool_result") => Some(parse_tool_result(b, is_async)),
                    _ => None,
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(UserContent::Blocks(blocks))
        }
    }
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

/// Parse one JSONL transcript line into the typed model. Consumes the value so
/// unrecognized entry kinds keep their payload verbatim without a copy.
pub fn parse_entry(data: Value) -> Result<Entry, ParseError> {
    let ty = require_str(&data, "type")?.to_string();
    match ty.as_str() {
        "user" => {
            let is_async = field(&data, "toolUseResult")
                .map(|tur| field_bool(tur, "isAsync"))
                .unwrap_or(false);
            let content =
                parse_user_content(require(require(&data, "message")?, "content")?, is_async)?;
            return Ok(Entry::User(UserEntry {
                meta: parse_meta(&data)?,
                content,
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
            }));
        }
        "system" => {
            return Ok(Entry::System(SystemEntry {
                meta: parse_meta(&data)?,
                subtype: require_str(&data, "subtype")?.to_string(),
                content: field_str(&data, "content").map(str::to_string),
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
        "user" => PrintBody::User(parse_user_content(require(message, "content")?, false)?),
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
                {{"type":"tool_use","id":"t2","name":"Task","input":{{"subagent_type":"Explore"}}}}
            ]}}}}"#
        );
        let entry = parse_entry(parse(&raw)).unwrap();
        let Entry::Assistant(assistant) = &entry else {
            panic!("expected assistant entry")
        };
        assert_eq!(assistant.model, "m1");
        assert!(assistant.usage.is_none());
        let uses: Vec<_> = entry.tool_uses().collect();
        assert_eq!(uses.len(), 2);
        assert_eq!(uses[0].name, "Bash");
        assert_eq!(uses[0].run_in_background, Some(true));
        assert_eq!(uses[0].subagent_type, None);
        assert_eq!(uses[0].input, parse(r#"{"command":"ls","run_in_background":true}"#));
        assert_eq!(uses[1].run_in_background, None);
        assert_eq!(uses[1].subagent_type.as_deref(), Some("Explore"));
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
