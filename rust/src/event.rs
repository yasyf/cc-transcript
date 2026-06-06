use chrono::DateTime;
use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use pyo3::IntoPyObjectExt;
use sonic_rs::{Index, JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::model::{
    models_type, ASSISTANT_EVENT_CLS, ENTRY_META_CLS, MODE_EVENT_CLS, OTHER_EVENT_CLS,
    SYSTEM_EVENT_CLS, TEXT_BLOCK_CLS, THINKING_BLOCK_CLS, TOOL_RESULT_BLOCK_CLS,
    TOOL_USE_BLOCK_CLS, USER_EVENT_CLS,
};

const INTERRUPT_MARKER: &str = "[Request interrupted by user";

fn field<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    key.value_index_into(data)
}

fn field_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field(data, key).and_then(JsonValueTrait::as_str)
}

fn field_bool(data: &Value, key: &str) -> bool {
    field(data, key)
        .and_then(JsonValueTrait::as_bool)
        .unwrap_or(false)
}

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

fn parse_timestamp(raw: &str) -> PyResult<DateTime<chrono::FixedOffset>> {
    DateTime::parse_from_rfc3339(raw)
        .map_err(|e| PyValueError::new_err(format!("invalid timestamp {raw:?}: {e}")))
}

fn block_type(block: &Value) -> Option<&str> {
    field_str(block, "type")
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
) -> PyResult<(String, Bound<'py, PyTuple>)> {
    match content.as_str() {
        Some(s) => Ok((s.to_string(), PyTuple::empty(py))),
        None => {
            let array = content.as_array().unwrap();
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
                            ))
                        }),
                )
                .collect::<PyResult<Vec<_>>>()?;
            Ok((texts.join(" "), PyTuple::new(py, blocks)?))
        }
    }
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
        other => Err(PyValueError::new_err(format!(
            "unexpected assistant block type: {}",
            other.unwrap_or("None")
        ))),
    }
}

fn parse_assistant_blocks<'py>(
    py: Python<'py>,
    content: &Value,
) -> PyResult<(String, Bound<'py, PyTuple>)> {
    let array = content.as_array().unwrap();
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
            let (text, blocks) =
                parse_user_blocks(py, require(require(data, "message")?, "content")?)?;
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
