use cc_transcript_core::render::py_float_repr;
use sonic_rs::{JsonContainerTrait, JsonType, JsonValueTrait, Value};

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Clone, Copy)]
pub struct Style {
    pub compact: bool,
    pub sort_keys: bool,
    pub ensure_ascii: bool,
}

pub const WIRE: Style = Style {
    compact: true,
    sort_keys: false,
    ensure_ascii: false,
};

pub const CANONICAL: Style = Style {
    compact: true,
    sort_keys: true,
    ensure_ascii: true,
};

pub const SORTED: Style = Style {
    compact: false,
    sort_keys: true,
    ensure_ascii: true,
};

pub fn dumps(value: &Value, style: Style) -> Result<String, String> {
    let mut out = String::new();
    write(value, style, &mut out)?;
    Ok(out)
}

pub fn write(value: &Value, style: Style, out: &mut String) -> Result<(), String> {
    match value.get_type() {
        JsonType::Null => out.push_str("null"),
        JsonType::Boolean => out.push_str(if value.as_bool().expect("boolean") {
            "true"
        } else {
            "false"
        }),
        JsonType::Number => out.push_str(&number(value)?),
        JsonType::String => string(value.as_str().expect("string"), style, out),
        JsonType::Array => {
            out.push('[');
            for (index, item) in value.as_array().expect("array").iter().enumerate() {
                if index > 0 {
                    out.push_str(if style.compact { "," } else { ", " });
                }
                write(item, style, out)?;
            }
            out.push(']');
        }
        JsonType::Object => {
            let object = value.as_object().expect("object");
            let mut pairs: Vec<_> = object.iter().collect();
            if style.sort_keys {
                pairs.sort_by(|left, right| left.0.cmp(right.0));
            }
            out.push('{');
            for (index, (key, item)) in pairs.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(if style.compact { "," } else { ", " });
                }
                string(key, style, out);
                out.push_str(if style.compact { ":" } else { ": " });
                write(item, style, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn number(value: &Value) -> Result<String, String> {
    if let Some(raw) = value.as_raw_number() {
        let lexeme = raw.as_str();
        if !lexeme
            .bytes()
            .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
        {
            return Ok(integer(lexeme));
        }
        return float(lexeme.parse().map_err(|_| "invalid number lexeme")?);
    }
    if let Some(signed) = value.as_i64() {
        return Ok(signed.to_string());
    }
    if let Some(unsigned) = value.as_u64() {
        return Ok(unsigned.to_string());
    }
    float(value.as_f64().expect("number"))
}

fn integer(lexeme: &str) -> String {
    if lexeme
        .trim_start_matches('-')
        .bytes()
        .all(|byte| byte == b'0')
    {
        "0".to_owned()
    } else {
        lexeme.to_owned()
    }
}

fn float(value: f64) -> Result<String, String> {
    if !value.is_finite() {
        return Err("snapshot number must be finite".to_owned());
    }
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        return Ok((value as i64).to_string());
    }
    Ok(py_float_repr(value, true))
}

fn string(text: &str, style: Style, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            wide if style.ensure_ascii && !wide.is_ascii() => {
                let mut units = [0u16; 2];
                for unit in wide.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

pub fn escaped_len(text: &str) -> usize {
    2 + text
        .chars()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            control if (control as u32) < 0x20 => 6,
            other => other.len_utf8(),
        })
        .sum::<usize>()
}

pub fn check_ingress(value: &Value) -> Result<(), String> {
    match value.get_type() {
        JsonType::Array => value
            .as_array()
            .expect("array")
            .iter()
            .try_for_each(check_ingress),
        JsonType::Object => value
            .as_object()
            .expect("object")
            .iter()
            .try_for_each(|(_, item)| check_ingress(item)),
        JsonType::Number => match value.as_raw_number() {
            Some(raw) => check_lexeme(raw.as_str()),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

fn check_lexeme(lexeme: &str) -> Result<(), String> {
    if !lexeme
        .bytes()
        .any(|byte| matches!(byte, b'.' | b'e' | b'E'))
    {
        return Ok(());
    }
    let magnitude: f64 = lexeme.parse().map_err(|_| "invalid number lexeme")?;
    let zero = lexeme
        .split(['e', 'E'])
        .next()
        .expect("mantissa")
        .bytes()
        .all(|byte| matches!(byte, b'0' | b'.' | b'-' | b'+'));
    if !zero && (!magnitude.is_finite() || magnitude == 0.0) {
        return Err("snapshot number exceeds finite nonzero binary64 magnitude".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(text: &str) -> String {
        dumps(&sonic_rs::from_str(text).unwrap(), WIRE).unwrap()
    }

    #[test]
    fn wire_numbers_match_python_json_dumps() {
        for (input, expected) in [
            (
                "[1.0, -0.0, 2.5, 1e-05, 0.0001, 1e16, 9007199254740993.0]",
                "[1,0,2.5,1e-05,0.0001,1e+16,9007199254740992.0]",
            ),
            (
                "[-0, 12, 18446744073709551615]",
                "[0,12,18446744073709551615]",
            ),
            (
                "[1e22, 123456789012345680000.0]",
                "[1e+22,1.2345678901234568e+20]",
            ),
        ] {
            assert_eq!(wire(input), expected, "{input}");
        }
    }

    #[test]
    fn wire_strings_escape_like_python_without_ascii_folding() {
        assert_eq!(
            wire(r#"{"ké":"a\"b\\c\n\r\t\b\f\u0001\u001f\u007f é😀"}"#),
            "{\"k\u{e9}\":\"a\\\"b\\\\c\\n\\r\\t\\b\\f\\u0001\\u001f\u{7f}\u{2028}é😀\"}"
        );
        assert_eq!(
            dumps(
                &sonic_rs::from_str(r#"{"b":"é😀","a":[1, 2]}"#).unwrap(),
                SORTED
            )
            .unwrap(),
            r#"{"a": [1, 2], "b": "\u00e9\ud83d\ude00"}"#
        );
    }

    #[test]
    fn escaped_length_is_the_written_length() {
        let text = "a\"b\\c\n\u{1}é😀";
        let mut out = String::new();
        string(text, WIRE, &mut out);
        assert_eq!(escaped_len(text), out.len());
    }

    #[test]
    fn ingress_refuses_nonfinite_and_underflowing_lexemes() {
        for (lexeme, ok) in [
            ("1e400", false),
            ("1e-400", false),
            ("0.0e-400", true),
            ("1.5", true),
            ("7", true),
        ] {
            let value: Value = sonic_rs::from_str(&format!("{{\"n\":[{lexeme}]}}")).unwrap();
            assert_eq!(check_ingress(&value).is_ok(), ok, "{lexeme}");
        }
    }
}
