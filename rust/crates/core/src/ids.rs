use std::collections::HashMap;
use std::fmt::Write as _;

use sha2::{Digest, Sha256};
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

pub const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

// Pure-data mirror of cc_transcript.ids.EventRef: located by session UUID, never a
// path. tool_use_id is set only when the ref names a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventRef {
    pub session_id: String,
    pub event_uuid: String,
    pub tool_use_id: Option<String>,
}

impl EventRef {
    pub fn new(session_id: String, event_uuid: String) -> Self {
        Self {
            session_id,
            event_uuid,
            tool_use_id: None,
        }
    }
}

// Parity: cc_transcript.ids.canonical_json — RFC 8785 JCS. A cross-language digest
// contract (cc-review's Go mirrors it), so byte-exactness is load-bearing.
pub fn canonical_json(value: &Value) -> Result<String, String> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

// Parity: cc_transcript.ids.tool_digest. The two keys are already UTF-16-ordered
// ("input" < "tool"), so the wrapper is assembled directly.
pub fn tool_digest(tool_name: &str, tool_input: &Value) -> Result<String, String> {
    let input = canonical_json(tool_input)?;
    let mut wrapper = String::with_capacity(input.len() + tool_name.len() + 20);
    wrapper.push_str("{\"input\":");
    wrapper.push_str(&input);
    wrapper.push_str(",\"tool\":");
    encode_string(tool_name, &mut wrapper);
    wrapper.push('}');
    Ok(hex_sha256(wrapper.as_bytes()))
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), String> {
    match value.get_type() {
        JsonType::Null => out.push_str("null"),
        JsonType::Boolean => out.push_str(if value.as_bool().unwrap() {
            "true"
        } else {
            "false"
        }),
        JsonType::Number => encode_number(value, out)?,
        JsonType::String => encode_string(value.as_str().unwrap(), out),
        JsonType::Array => {
            out.push('[');
            for (i, item) in value.as_array().unwrap().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        JsonType::Object => {
            // Python json.loads keeps the last of duplicate keys; sonic-rs preserves
            // both, so dedupe by decoded key (last wins) before the UTF-16 sort.
            let mut deduped: HashMap<&str, &Value> = HashMap::new();
            for (key, item) in value.as_object().unwrap().iter() {
                deduped.insert(key, item);
            }
            let mut entries: Vec<(&str, &Value)> = deduped.into_iter().collect();
            entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            out.push('{');
            for (i, (key, item)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode_string(key, out);
                out.push(':');
                write_canonical(item, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

// Parity: canonical_parts int/float arms. Parsed numbers carry raw text
// (arbitrary_precision); constructed Values don't, so classify by numeric type.
fn encode_number(value: &Value, out: &mut String) -> Result<(), String> {
    if let Some(raw) = value.as_raw_number() {
        let text = raw.as_str();
        return if text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) {
            push_float(text.parse().expect("JSON number text parses as f64"), out)
        } else {
            push_int(text.parse().map_err(|_| int_out_of_range(text))?, out)
        };
    }
    if let Some(int) = value.as_i64() {
        return push_int(int, out);
    }
    if let Some(uint) = value.as_u64() {
        return Err(int_out_of_range(&uint.to_string()));
    }
    push_float(
        value.as_f64().expect("a Number Value is i64, u64, or f64"),
        out,
    )
}

fn push_int(int: i64, out: &mut String) -> Result<(), String> {
    if int.unsigned_abs() > MAX_SAFE_INTEGER as u64 {
        return Err(int_out_of_range(&int.to_string()));
    }
    write!(out, "{int}").unwrap();
    Ok(())
}

fn push_float(value: f64, out: &mut String) -> Result<(), String> {
    out.push_str(&es_number(value)?);
    Ok(())
}

fn int_out_of_range(text: &str) -> String {
    format!("integer exceeds IEEE-754 double precision: {text}")
}

// Parity: cc_transcript.ids.es_number. ryu-js is ECMAScript-exact; Rust std / plain
// ryu diverge from Python/ES on shortest ties (e.g. 698957826421429.2). NaN/inf reject.
fn es_number(value: f64) -> Result<String, String> {
    if value.is_nan() || value.is_infinite() {
        return Err(format!("number cannot canonicalize: {value}"));
    }
    if value == 0.0 {
        return Ok("0".to_string());
    }
    Ok(ryu_js::Buffer::new().format_finite(value).to_string())
}

// Parity: canonical_parts str arm — json.dumps(s, ensure_ascii=False): escape " \
// and C0 controls (short escapes else lowercase \u00xx), raw UTF-8 otherwise.
pub(crate) fn encode_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(out, "{byte:02x}").unwrap();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(json: &str) -> String {
        canonical_json(&sonic_rs::from_str(json).unwrap()).unwrap()
    }

    fn digest(name: &str, input_json: &str) -> String {
        tool_digest(name, &sonic_rs::from_str(input_json).unwrap()).unwrap()
    }

    #[test]
    fn es_number_matches_ecmascript_layout() {
        for (json, expected) in [
            ("0.0", "0"),
            ("-0.0", "0"),
            ("1.0", "1"),
            ("-1.5", "-1.5"),
            ("0.5", "0.5"),
            ("0.05", "0.05"),
            ("100.0", "100"),
            ("123.456", "123.456"),
            ("333333333.3333333", "333333333.3333333"),
            ("698957826421429.2", "698957826421429.2"),
            ("1e16", "10000000000000000"),
            ("1e20", "100000000000000000000"),
            ("1e21", "1e+21"),
            ("9.999999999999997e22", "9.999999999999997e+22"),
            ("0.000001", "0.000001"),
            ("1e-7", "1e-7"),
            ("1.5e-7", "1.5e-7"),
            ("5e-324", "5e-324"),
        ] {
            assert_eq!(canon(json), expected, "{json}");
        }
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        assert!(es_number(f64::NAN).is_err());
        assert!(es_number(f64::INFINITY).is_err());
        assert!(es_number(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn integers_serialize_exactly_and_reject_beyond_double() {
        assert_eq!(
            canon(r#"{"a":1,"b":-42,"c":9007199254740991}"#),
            r#"{"a":1,"b":-42,"c":9007199254740991}"#
        );
        assert!(canonical_json(&sonic_rs::from_str("9007199254740993").unwrap()).is_err());
        assert!(
            canonical_json(&sonic_rs::from_str("100000000000000000000000000000").unwrap()).is_err()
        );
    }

    #[test]
    fn constructed_values_classify_by_numeric_type() {
        // Constructed sonic Values carry no raw text; classify by numeric type.
        assert_eq!(canonical_json(&Value::from(1_i64)).unwrap(), "1");
        assert_eq!(canonical_json(&Value::from(-42_i64)).unwrap(), "-42");
        assert_eq!(canonical_json(&sonic_rs::json!(1.5)).unwrap(), "1.5");
        // An integral float stays a float — formatted, not rejected as an oversized int.
        assert_eq!(
            canonical_json(&sonic_rs::json!(1e16)).unwrap(),
            "10000000000000000"
        );
        assert_eq!(
            canonical_json(&Value::from(MAX_SAFE_INTEGER)).unwrap(),
            "9007199254740991"
        );
        assert!(canonical_json(&Value::from(MAX_SAFE_INTEGER + 2)).is_err());
        assert!(canonical_json(&Value::from(u64::MAX)).is_err());
    }

    #[test]
    fn duplicate_keys_keep_last_occurrence() {
        assert_eq!(canon(r#"{"a":1,"a":2}"#), r#"{"a":2}"#);
        assert_eq!(canon(r#"{"b":1,"a":2,"b":3}"#), r#"{"a":2,"b":3}"#);
    }

    #[test]
    fn keys_sort_by_utf16_code_units() {
        let expected = "{\"\u{20ac}\":3,\"\u{1f600}\":2,\"\u{ffff}\":1}";
        assert_eq!(canon(r#"{"￿":1,"😀":2,"€":3}"#), expected);
    }

    #[test]
    fn string_escaping_matches_json_stringify() {
        let s = "{\"k\":\"a\\\"b\\\\c\\n\\t\\u001fé\"}";
        assert_eq!(canon(s), s);
    }

    #[test]
    fn nested_and_empty_structures_canonicalize() {
        assert_eq!(
            canon(r#"{"b":[1,{"y":null,"x":true}],"a":"s"}"#),
            r#"{"a":"s","b":[1,{"x":true,"y":null}]}"#
        );
        assert_eq!(canon("{}"), "{}");
        assert_eq!(canon("[]"), "[]");
        assert_eq!(canon("null"), "null");
    }

    #[test]
    fn tool_digest_matches_frozen_fixture_corpus() {
        // Digests captured from the Python reference in testdata/digest_fixtures.json.
        assert_eq!(
            digest("Bash", r#"{"command":"git status","timeout":5000}"#),
            "bb1ec4959bd14512342126db9066d06d92f8e3486e74b9812cb5037f3f086cc8"
        );
        assert_eq!(
            digest("Empty", "{}"),
            "b50e454c692adcd4121bab7971501fcc94656ad0247b66d2d607be05dcddd4c5"
        );
    }

    #[test]
    fn tool_digest_is_stable_across_key_order() {
        let a = digest(
            "Edit",
            r#"{"file_path":"a.py","old_string":"x","new_string":"y"}"#,
        );
        let b = digest(
            "Edit",
            r#"{"new_string":"y","old_string":"x","file_path":"a.py"}"#,
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn event_ref_defaults_tool_use_id_to_none() {
        let event_ref = EventRef::new("s".to_string(), "e".to_string());
        assert_eq!(event_ref.tool_use_id, None);
        assert_eq!(event_ref.session_id, "s");
        assert_eq!(event_ref.event_uuid, "e");
    }
}
