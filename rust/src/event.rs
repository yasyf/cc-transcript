use chrono::DateTime;
use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use pyo3::IntoPyObjectExt;
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::model::{
    models_type, ASSISTANT_EVENT_CLS, CACHE_CREATION_CLS, ENTRY_META_CLS, FALLBACK_BLOCK_CLS,
    INIT_INFO_CLS, MCP_SERVER_CLS, MODE_EVENT_CLS, MODEL_USAGE_CLS, OTHER_BLOCK_CLS, OTHER_EVENT_CLS,
    PLUGIN_CLS, PRINT_MESSAGE_CLS, PRINT_RESULT_CLS, SERVER_TOOL_USE_CLS, SYSTEM_EVENT_CLS,
    TEXT_BLOCK_CLS, THINKING_BLOCK_CLS, TOOL_RESULT_BLOCK_CLS, TOOL_USE_BLOCK_CLS, USAGE_CLS,
    USER_EVENT_CLS,
};
use crate::value::{block_type, field, field_bool, field_str};

const INTERRUPT_MARKER: &str = "[Request interrupted by user";

fn truthy_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field_str(data, key).filter(|s| !s.is_empty())
}

fn require<'a>(data: &'a Value, key: &str) -> PyResult<&'a Value> {
    field(data, key).ok_or_else(|| PyKeyError::new_err(format!("'{key}'")))
}

fn require_str<'a>(data: &'a Value, key: &str) -> PyResult<&'a str> {
    require(data, key)?
        .as_str()
        .ok_or_else(|| PyKeyError::new_err(format!("'{key}'")))
}

fn require_i64(data: &Value, key: &str) -> PyResult<i64> {
    require(data, key)?
        .as_i64()
        .ok_or_else(|| PyKeyError::new_err(format!("'{key}'")))
}

fn require_f64(data: &Value, key: &str) -> PyResult<f64> {
    require(data, key)?
        .as_f64()
        .ok_or_else(|| PyKeyError::new_err(format!("'{key}'")))
}

fn require_bool(data: &Value, key: &str) -> PyResult<bool> {
    require(data, key)?
        .as_bool()
        .ok_or_else(|| PyKeyError::new_err(format!("'{key}'")))
}

fn parse_timestamp(raw: &str) -> PyResult<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(raw)
        .map_err(|e| PyValueError::new_err(format!("invalid timestamp {raw:?}: {e}")))
}

fn require_array(content: &Value) -> PyResult<&[Value]> {
    content
        .as_array()
        .map(|a| a.as_slice())
        .ok_or_else(|| PyValueError::new_err("message content is neither a string nor a list"))
}

fn json_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    match value.get_type() {
        JsonType::Null => Ok(py.None().into_bound(py)),
        JsonType::Boolean => value.as_bool().unwrap().into_bound_py_any(py),
        JsonType::Number => match (value.as_i64(), value.as_u64(), value.as_f64()) {
            (Some(i), _, _) => i.into_bound_py_any(py),
            (_, Some(u), _) => u.into_bound_py_any(py),
            (_, _, Some(f)) => f.into_bound_py_any(py),
            _ => unreachable!("a JSON number is i64, u64, or f64"),
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

fn parse_meta<'py>(py: Python<'py>, data: &Value) -> PyResult<Bound<'py, PyAny>> {
    models_type(py, &ENTRY_META_CLS, "EntryMeta")?.call1((
        require_str(data, "uuid")?,
        truthy_str(data, "parentUuid"),
        require_str(data, "sessionId")?,
        parse_timestamp(require_str(data, "timestamp")?)?,
        field_str(data, "cwd"),
        field_str(data, "gitBranch"),
        truthy_str(data, "version"),
        field_bool(data, "isSidechain"),
        field_bool(data, "isMeta"),
        field_str(data, "entrypoint"),
        field_bool(data, "isCompactSummary"),
        field_bool(data, "isVisibleInTranscriptOnly"),
    ))
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

fn text_block<'py>(py: Python<'py>, text: &str) -> PyResult<Bound<'py, PyAny>> {
    models_type(py, &TEXT_BLOCK_CLS, "TextBlock")?.call1((text,))
}

fn parse_user_blocks<'py>(
    py: Python<'py>,
    content: &Value,
    is_async: bool,
) -> PyResult<(String, Bound<'py, PyTuple>)> {
    match content.as_str() {
        Some(s) => Ok((s.to_string(), PyTuple::empty(py))),
        None => {
            let array = require_array(content)?;
            let texts: Vec<&str> = array
                .iter()
                .filter(|b| block_type(b) == Some("text"))
                .filter_map(|b| field_str(b, "text"))
                .collect();
            let blocks = texts
                .iter()
                .map(|t| text_block(py, t))
                .chain(
                    array
                        .iter()
                        .filter(|b| block_type(b) == Some("tool_result"))
                        .map(|b| {
                            models_type(py, &TOOL_RESULT_BLOCK_CLS, "ToolResultBlock")?.call1((
                                require_str(b, "tool_use_id")?,
                                flatten_result_content(require(b, "content")?),
                                field_bool(b, "is_error"),
                                is_async,
                            ))
                        }),
                )
                .collect::<PyResult<Vec<_>>>()?;
            Ok((texts.join(" "), PyTuple::new(py, blocks)?))
        }
    }
}

fn build_usage<'py>(py: Python<'py>, message: &Value) -> PyResult<Bound<'py, PyAny>> {
    match field(message, "usage").filter(|u| u.is_object()) {
        Some(usage) => build_usage_value(py, usage),
        None => Ok(py.None().into_bound(py)),
    }
}

fn build_usage_value<'py>(py: Python<'py>, usage: &Value) -> PyResult<Bound<'py, PyAny>> {
    let cache_creation = match field(usage, "cache_creation").filter(|cc| cc.is_object()) {
        Some(cc) => models_type(py, &CACHE_CREATION_CLS, "CacheCreation")?.call1((
            require_i64(cc, "ephemeral_5m_input_tokens")?,
            require_i64(cc, "ephemeral_1h_input_tokens")?,
        ))?,
        None => py.None().into_bound(py),
    };
    let server_tool_use = match field(usage, "server_tool_use").filter(|s| s.is_object()) {
        Some(s) => models_type(py, &SERVER_TOOL_USE_CLS, "ServerToolUse")?.call1((
            require_i64(s, "web_search_requests")?,
            require_i64(s, "web_fetch_requests")?,
        ))?,
        None => py.None().into_bound(py),
    };
    models_type(py, &USAGE_CLS, "Usage")?.call1((
        require_i64(usage, "input_tokens")?,
        require_i64(usage, "output_tokens")?,
        require_i64(usage, "cache_read_input_tokens")?,
        require_i64(usage, "cache_creation_input_tokens")?,
        cache_creation,
        field_str(usage, "service_tier"),
        field_str(usage, "inference_geo"),
        server_tool_use,
    ))
}

fn parse_assistant_block<'py>(py: Python<'py>, block: &Value) -> PyResult<Bound<'py, PyAny>> {
    match block_type(block) {
        Some("text") => text_block(py, require_str(block, "text")?),
        Some("thinking") => models_type(py, &THINKING_BLOCK_CLS, "ThinkingBlock")?
            .call1((require_str(block, "thinking")?,)),
        Some("tool_use") => models_type(py, &TOOL_USE_BLOCK_CLS, "ToolUseBlock")?.call1((
            require_str(block, "id")?,
            require_str(block, "name")?,
            json_to_py(py, require(block, "input")?)?,
        )),
        Some("fallback") => models_type(py, &FALLBACK_BLOCK_CLS, "FallbackBlock")?.call1((
            require_str(require(block, "from")?, "model")?,
            require_str(require(block, "to")?, "model")?,
        )),
        Some(other) => {
            models_type(py, &OTHER_BLOCK_CLS, "OtherBlock")?.call1((other, json_to_py(py, block)?))
        }
        None => Err(PyKeyError::new_err("'type'")),
    }
}

fn parse_assistant_blocks<'py>(
    py: Python<'py>,
    content: &Value,
) -> PyResult<(String, Bound<'py, PyTuple>)> {
    let array = require_array(content)?;
    let text = array
        .iter()
        .filter(|b| block_type(b) == Some("text"))
        .filter_map(|b| field_str(b, "text"))
        .collect::<Vec<_>>()
        .join(" ");
    let blocks = array
        .iter()
        .map(|b| parse_assistant_block(py, b))
        .collect::<PyResult<Vec<_>>>()?;
    Ok((text, PyTuple::new(py, blocks)?))
}

pub fn build_event<'py>(py: Python<'py>, data: &Value) -> PyResult<Bound<'py, PyAny>> {
    match require_str(data, "type")? {
        "user" => {
            let is_async = field(data, "toolUseResult")
                .map(|tur| field_bool(tur, "isAsync"))
                .unwrap_or(false);
            let (text, blocks) =
                parse_user_blocks(py, require(require(data, "message")?, "content")?, is_async)?;
            let interrupted = text.contains(INTERRUPT_MARKER);
            models_type(py, &USER_EVENT_CLS, "UserEvent")?.call1((
                parse_meta(py, data)?,
                text,
                blocks,
                interrupted,
            ))
        }
        "assistant" => {
            let message = require(data, "message")?;
            let (text, blocks) = parse_assistant_blocks(py, require(message, "content")?)?;
            models_type(py, &ASSISTANT_EVENT_CLS, "AssistantEvent")?.call1((
                parse_meta(py, data)?,
                require_str(message, "model")?,
                text,
                blocks,
                field_str(message, "stop_reason"),
                build_usage(py, message)?,
            ))
        }
        "system" => models_type(py, &SYSTEM_EVENT_CLS, "SystemEvent")?.call1((
            parse_meta(py, data)?,
            require_str(data, "subtype")?,
            field_str(data, "content"),
        )),
        "mode" => models_type(py, &MODE_EVENT_CLS, "ModeEvent")?.call1((
            require_str(data, "sessionId")?,
            "mode",
            require_str(data, "mode")?,
        )),
        "permission-mode" => models_type(py, &MODE_EVENT_CLS, "ModeEvent")?.call1((
            require_str(data, "sessionId")?,
            "permission-mode",
            require_str(data, "permissionMode")?,
        )),
        other => {
            let kind = other.to_string();
            models_type(py, &OTHER_EVENT_CLS, "OtherEvent")?.call1((kind, json_to_py(py, data)?))
        }
    }
}

fn build_model_usage<'py>(py: Python<'py>, usage: &Value) -> PyResult<Bound<'py, PyAny>> {
    models_type(py, &MODEL_USAGE_CLS, "ModelUsage")?.call1((
        require_i64(usage, "inputTokens")?,
        require_i64(usage, "outputTokens")?,
        require_i64(usage, "cacheReadInputTokens")?,
        require_i64(usage, "cacheCreationInputTokens")?,
        require_i64(usage, "webSearchRequests")?,
        require_f64(usage, "costUSD")?,
        require_i64(usage, "contextWindow")?,
        require_i64(usage, "maxOutputTokens")?,
    ))
}

fn build_init<'py>(py: Python<'py>, init: &Value) -> PyResult<Bound<'py, PyAny>> {
    let mcp_servers = require(init, "mcp_servers")?
        .as_array()
        .ok_or_else(|| PyKeyError::new_err("'mcp_servers'"))?
        .iter()
        .map(|s| {
            models_type(py, &MCP_SERVER_CLS, "McpServer")?
                .call1((require_str(s, "name")?, require_str(s, "status")?))
        })
        .collect::<PyResult<Vec<_>>>()?;
    let plugins = require(init, "plugins")?
        .as_array()
        .ok_or_else(|| PyKeyError::new_err("'plugins'"))?
        .iter()
        .map(|p| {
            models_type(py, &PLUGIN_CLS, "Plugin")?.call1((
                require_str(p, "name")?,
                require_str(p, "path")?,
                require_str(p, "source")?,
            ))
        })
        .collect::<PyResult<Vec<_>>>()?;
    let tools = require(init, "tools")?
        .as_array()
        .ok_or_else(|| PyKeyError::new_err("'tools'"))?
        .iter()
        .map(|t| t.as_str().ok_or_else(|| PyKeyError::new_err("'tools'")))
        .collect::<PyResult<Vec<_>>>()?;
    let skills = require(init, "skills")?
        .as_array()
        .ok_or_else(|| PyKeyError::new_err("'skills'"))?
        .iter()
        .map(|s| s.as_str().ok_or_else(|| PyKeyError::new_err("'skills'")))
        .collect::<PyResult<Vec<_>>>()?;
    models_type(py, &INIT_INFO_CLS, "InitInfo")?.call1((
        PyTuple::new(py, mcp_servers)?,
        PyTuple::new(py, plugins)?,
        PyTuple::new(py, tools)?,
        PyTuple::new(py, skills)?,
    ))
}

fn build_print_message<'py>(py: Python<'py>, element: &Value) -> PyResult<Bound<'py, PyAny>> {
    let role = require_str(element, "type")?;
    let message = require(element, "message")?;
    let (text, blocks, model): (String, Bound<'py, PyTuple>, Bound<'py, PyAny>) = match role {
        "assistant" => {
            let (text, blocks) = parse_assistant_blocks(py, require(message, "content")?)?;
            (text, blocks, field_str(message, "model").into_bound_py_any(py)?)
        }
        "user" => {
            let (text, blocks) = parse_user_blocks(py, require(message, "content")?, false)?;
            (text, blocks, py.None().into_bound(py))
        }
        other => return Err(PyValueError::new_err(format!("not a conversational element: {other:?}"))),
    };
    models_type(py, &PRINT_MESSAGE_CLS, "PrintMessage")?.call1((
        role,
        model,
        text,
        blocks,
        field_str(element, "uuid"),
        require_str(element, "session_id")?,
    ))
}

pub fn build_print_result<'py>(py: Python<'py>, array: &Value) -> PyResult<Bound<'py, PyAny>> {
    let elements = array
        .as_array()
        .ok_or_else(|| PyValueError::new_err("envelope is not a JSON array"))?;
    let result = elements
        .iter()
        .find(|e| block_type(e) == Some("result"))
        .ok_or_else(|| PyValueError::new_err("envelope has no result element"))?;
    let init = elements
        .iter()
        .find(|e| block_type(e) == Some("system") && field_str(e, "subtype") == Some("init"));

    let model_usage = PyDict::new(py);
    for (model, usage) in require(result, "modelUsage")?
        .as_object()
        .ok_or_else(|| PyKeyError::new_err("'modelUsage'"))?
    {
        model_usage.set_item(model, build_model_usage(py, usage)?)?;
    }

    let structured_output = match field(result, "structured_output").filter(|s| !s.is_null()) {
        Some(s) => json_to_py(py, s)?,
        None => py.None().into_bound(py),
    };
    let permission_denials = require(result, "permission_denials")?
        .as_array()
        .ok_or_else(|| PyKeyError::new_err("'permission_denials'"))?
        .iter()
        .map(|d| json_to_py(py, d))
        .collect::<PyResult<Vec<_>>>()?;
    let init_obj = match init {
        Some(i) => build_init(py, i)?,
        None => py.None().into_bound(py),
    };
    let messages = elements
        .iter()
        .filter(|e| matches!(block_type(e), Some("user") | Some("assistant")))
        .map(|e| build_print_message(py, e))
        .collect::<PyResult<Vec<_>>>()?;

    let args: [Bound<'py, PyAny>; 13] = [
        require_f64(result, "total_cost_usd")?.into_bound_py_any(py)?,
        model_usage.into_any(),
        build_usage_value(py, require(result, "usage")?)?,
        structured_output,
        require_i64(result, "num_turns")?.into_bound_py_any(py)?,
        require_bool(result, "is_error")?.into_bound_py_any(py)?,
        field_str(result, "result").into_bound_py_any(py)?,
        require_str(result, "session_id")?.into_bound_py_any(py)?,
        field_str(result, "fast_mode_state").into_bound_py_any(py)?,
        field_str(result, "stop_reason").into_bound_py_any(py)?,
        PyTuple::new(py, permission_denials)?.into_any(),
        init_obj,
        PyTuple::new(py, messages)?.into_any(),
    ];
    models_type(py, &PRINT_RESULT_CLS, "PrintResult")?.call1(PyTuple::new(py, args)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Value {
        sonic_rs::from_str(raw).unwrap()
    }

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
}
