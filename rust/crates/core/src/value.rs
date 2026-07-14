use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

pub fn field<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    key.value_index_into(data)
}

pub fn field_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field(data, key).and_then(JsonValueTrait::as_str)
}

/// Object field access with Python ``dict`` last-wins semantics: the last value for
/// ``key``, where sonic's index (``field``) returns the first — json.loads dedupes to
/// last, so a raw payload with duplicate keys resolves the same here.
pub fn field_last<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    data.as_object()?
        .iter()
        .filter(|(k, _)| *k == key)
        .last()
        .map(|(_, v)| v)
}

pub fn field_str_last<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field_last(data, key).and_then(JsonValueTrait::as_str)
}

pub fn field_bool(data: &Value, key: &str) -> bool {
    field(data, key)
        .and_then(JsonValueTrait::as_bool)
        .unwrap_or(false)
}

pub(crate) fn block_type(block: &Value) -> Option<&str> {
    field_str(block, "type")
}
