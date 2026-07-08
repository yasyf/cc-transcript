use sonic_rs::{Index, JsonValueTrait, Value};

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

pub(crate) fn block_type(block: &Value) -> Option<&str> {
    field_str(block, "type")
}
