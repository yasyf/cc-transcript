use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

pub(crate) fn field<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    key.value_index_into(data)
}

pub(crate) fn field_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field(data, key).and_then(JsonValueTrait::as_str)
}

pub(crate) fn field_bool(data: &Value, key: &str) -> bool {
    field(data, key)
        .and_then(JsonValueTrait::as_bool)
        .unwrap_or(false)
}

pub(crate) fn field_i64(data: &Value, key: &str) -> Option<i64> {
    field(data, key).and_then(JsonValueTrait::as_i64)
}

pub(crate) fn field_f64(data: &Value, key: &str) -> Option<f64> {
    field(data, key).and_then(JsonValueTrait::as_f64)
}

pub(crate) fn block_type(block: &Value) -> Option<&str> {
    field_str(block, "type")
}

/// The joined text a built event would carry: the message content verbatim when
/// it is a string, else the space-joined text of its ``text`` blocks. Mirrors
/// the Python parser's ``UserEvent.text`` / ``AssistantEvent.text``.
pub(crate) fn content_text(content: &Value) -> String {
    match content.as_str() {
        Some(s) => s.to_string(),
        None => content
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| block_type(b) == Some("text"))
            .filter_map(|b| field_str(b, "text"))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub(crate) fn has_block_type(content: &Value, kind: &str) -> bool {
    content
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| block_type(b) == Some(kind))
}
