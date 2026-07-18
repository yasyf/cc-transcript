use std::collections::HashMap;

use sonic_rs::{Index, JsonContainerTrait, JsonType, JsonValueTrait, Value};

pub fn field<'a>(data: &'a Value, key: &str) -> Option<&'a Value> {
    key.value_index_into(data)
}

/// Collapse duplicate object keys to Python ``json.loads`` semantics (last value wins,
/// kept at the first slot), recursively, so downstream ``field`` reads mirror ``dict``
/// lookups. Rebuilds only dup-carrying values, via a re-serialize + re-parse round trip:
/// sonic's mutable object API is hash-ordered, but ``from_str`` preserves insertion order
/// (which render and other consumers observe). Dup-free values are left untouched.
pub fn normalize_last_wins(value: &mut Value) {
    if subtree_has_duplicate_keys(value) {
        let mut out = String::new();
        write_deduped(value, &mut out);
        *value = sonic_rs::from_str(&out).expect("re-serialized JSON reparses");
    }
}

/// Clone `value` with last-wins key normalization applied — for the retained payloads
/// parse.rs holds by borrow (attachment details, mcp meta, print-result fields).
pub fn normalized_owned(value: &Value) -> Value {
    let mut owned = value.clone();
    normalize_last_wins(&mut owned);
    owned
}

fn subtree_has_duplicate_keys(value: &Value) -> bool {
    if let Some(object) = value.as_object() {
        let mut seen: Vec<&str> = Vec::new();
        for (key, item) in object.iter() {
            if seen.contains(&key) || subtree_has_duplicate_keys(item) {
                return true;
            }
            seen.push(key);
        }
        false
    } else if let Some(array) = value.as_array() {
        array.iter().any(subtree_has_duplicate_keys)
    } else {
        false
    }
}

/// An object's pairs with Python ``json.loads`` duplicate-key semantics: last value
/// wins, kept at the first occurrence's slot.
pub(crate) fn deduped_pairs(object: &sonic_rs::Object) -> Vec<(&str, &Value)> {
    let mut order: Vec<&str> = Vec::new();
    let mut last: HashMap<&str, &Value> = HashMap::new();
    for (key, item) in object.iter() {
        if last.insert(key, item).is_none() {
            order.push(key);
        }
    }
    order.into_iter().map(|key| (key, last[key])).collect()
}

// Re-serialize `value` dropping earlier duplicate object keys; leaves go through sonic's
// own encoder so raw number text and string escapes survive verbatim.
fn write_deduped(value: &Value, out: &mut String) {
    if let Some(object) = value.as_object() {
        out.push('{');
        for (i, (key, item)) in deduped_pairs(object).into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&sonic_rs::to_string(&key).unwrap());
            out.push(':');
            write_deduped(item, out);
        }
        out.push('}');
    } else if let Some(array) = value.as_array() {
        out.push('[');
        for (i, item) in array.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_deduped(item, out);
        }
        out.push(']');
    } else {
        out.push_str(&sonic_rs::to_string(value).unwrap());
    }
}

pub fn field_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field(data, key).and_then(JsonValueTrait::as_str)
}

pub fn field_bool(data: &Value, key: &str) -> bool {
    field(data, key)
        .and_then(JsonValueTrait::as_bool)
        .unwrap_or(false)
}

/// Python `bool(x)` truthiness: null/false/0/""/[]/{} are falsy, everything else truthy.
pub(crate) fn is_py_truthy(value: &Value) -> bool {
    match value.get_type() {
        JsonType::Null => false,
        JsonType::Boolean => value.as_bool().unwrap(),
        JsonType::Number => value.as_f64().is_none_or(|f| f != 0.0),
        JsonType::String => !value.as_str().unwrap().is_empty(),
        JsonType::Array => !value.as_array().unwrap().is_empty(),
        JsonType::Object => !value.as_object().unwrap().is_empty(),
    }
}

/// Field access with Python `bool(data.get(key))` semantics: coerce by truthiness (so
/// `1`/`"x"` are true), unlike `field_bool`'s strict `as_bool` for `is True` sites.
pub fn field_truthy(data: &Value, key: &str) -> bool {
    field(data, key).is_some_and(is_py_truthy)
}

pub(crate) fn block_type(block: &Value) -> Option<&str> {
    field_str(block, "type")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(json: &str) -> Value {
        let mut value: Value = sonic_rs::from_str(json).unwrap();
        normalize_last_wins(&mut value);
        value
    }

    #[test]
    fn duplicate_keys_collapse_to_last_value_first_position() {
        // json.loads keeps a re-inserted key in its first slot; render serializes in that
        // order, so the collapsed object must read {"file_path","limit"}, not {"limit",...}.
        let value = normalized(r#"{"file_path":"/first","limit":2,"file_path":"/last"}"#);
        assert_eq!(field(&value, "file_path").unwrap().as_str(), Some("/last"));
        let keys: Vec<&str> = value.as_object().unwrap().iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["file_path", "limit"]);
    }

    #[test]
    fn preserves_key_order_without_duplicates() {
        let value = normalized(r#"{"file_path":"x","limit":1,"offset":2}"#);
        let keys: Vec<&str> = value.as_object().unwrap().iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["file_path", "limit", "offset"]);
    }

    #[test]
    fn recurses_into_nested_objects_and_array_elements() {
        let value = normalized(r#"{"o":{"k":1,"k":2},"a":[{"x":1,"x":9},{"y":5}]}"#);
        assert_eq!(
            field(field(&value, "o").unwrap(), "k").unwrap().as_i64(),
            Some(2)
        );
        let array = field(&value, "a").unwrap().as_array().unwrap();
        assert_eq!(field(&array[0], "x").unwrap().as_i64(), Some(9));
        assert_eq!(field(&array[1], "y").unwrap().as_i64(), Some(5));
    }

    #[test]
    fn dup_free_objects_preserve_raw_number_text() {
        // No duplicate keys -> traversed, not rebuilt; the ES-tie float and the beyond-2^53
        // int keep their exact raw text (arbitrary_precision), never an f64 round-trip.
        let value = normalized(r#"{"n":698957826421429.2,"m":9007199254740993}"#);
        assert_eq!(
            field(&value, "n")
                .unwrap()
                .as_raw_number()
                .unwrap()
                .as_str(),
            "698957826421429.2"
        );
        assert_eq!(
            field(&value, "m")
                .unwrap()
                .as_raw_number()
                .unwrap()
                .as_str(),
            "9007199254740993"
        );
    }

    #[test]
    fn rebuild_keeps_last_raw_number_verbatim() {
        // A duplicate key forces the rebuild path; the surviving value clones verbatim.
        let value = normalized(r#"{"n":1,"n":9007199254740993}"#);
        assert_eq!(
            field(&value, "n")
                .unwrap()
                .as_raw_number()
                .unwrap()
                .as_str(),
            "9007199254740993"
        );
        let tie = normalized(r#"{"x":0,"x":698957826421429.2}"#);
        assert_eq!(
            field(&tie, "x").unwrap().as_raw_number().unwrap().as_str(),
            "698957826421429.2"
        );
    }
}
