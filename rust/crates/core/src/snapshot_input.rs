use serde_json::Value as SchemaValue;
use sonic_rs::{json, JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::snapshot::{SnapshotError, Status};

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

pub fn exact_integer(text: &str) -> Result<i64, SnapshotError> {
    const MAX_SAFE: u64 = 9_007_199_254_740_991;
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (coefficient, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let fractional = coefficient
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits: String = coefficient.chars().filter(|&digit| digit != '.').collect();
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    let significant = digits.trim_end_matches('0');
    let trailing = digits.len() - significant.len();
    let exponent: i64 = exponent
        .parse()
        .map_err(|_| invalid("integer exponent exceeds safe range"))?;
    let scale = exponent
        .checked_add(trailing as i64)
        .and_then(|value| value.checked_sub(fractional as i64))
        .ok_or_else(|| invalid("integer exponent exceeds safe range"))?;
    if scale < 0 {
        return Err(invalid("fractional metadata number"));
    }
    if scale > 16 || significant.len().saturating_add(scale as usize) > 16 {
        return Err(invalid("metadata integer exceeds safe range"));
    }
    let mut value: u64 = significant
        .parse()
        .map_err(|_| invalid("invalid metadata integer"))?;
    for _ in 0..scale {
        value *= 10;
    }
    if value > MAX_SAFE {
        return Err(invalid("metadata integer exceeds safe range"));
    }
    Ok(if negative {
        -(value as i64)
    } else {
        value as i64
    })
}

pub fn normalize_numbers(value: &mut SchemaValue) -> Result<(), SnapshotError> {
    match value {
        SchemaValue::Array(values) => {
            for value in values {
                normalize_numbers(value)?;
            }
        }
        SchemaValue::Object(values) => {
            for value in values.values_mut() {
                normalize_numbers(value)?;
            }
        }
        SchemaValue::Number(number) => {
            *number = serde_json::Number::from(exact_integer(&number.to_string())?);
        }
        _ => {}
    }
    Ok(())
}

pub fn native_value(value: &SchemaValue) -> Value {
    match value {
        SchemaValue::Null => json!(null),
        SchemaValue::Bool(value) => json!(value),
        SchemaValue::String(value) => json!(value),
        SchemaValue::Number(value) => json!(value.as_i64().expect("normalized metadata integer")),
        SchemaValue::Array(values) => {
            let mut array = sonic_rs::Array::with_capacity(values.len());
            for value in values {
                array.push(native_value(value));
            }
            array.into_value()
        }
        SchemaValue::Object(values) => {
            let mut object = sonic_rs::Object::with_capacity(values.len());
            for (key, value) in values {
                object.insert(key.as_str(), native_value(value));
            }
            object.into_value()
        }
    }
}

/// The owner's unvalidated ingress: object keys sorted and every number an exact
/// safe integer, as ``native_value`` yields after ``normalize_numbers``.
pub fn native_input(value: &Value) -> Result<Value, SnapshotError> {
    Ok(match value.get_type() {
        JsonType::Array => {
            let values = value.as_array().expect("array");
            let mut array = sonic_rs::Array::with_capacity(values.len());
            for value in values.iter() {
                array.push(native_input(value)?);
            }
            array.into_value()
        }
        JsonType::Object => {
            let values = value.as_object().expect("object");
            let mut pairs: Vec<_> = values.iter().collect();
            pairs.sort_by(|left, right| left.0.cmp(right.0));
            let mut object = sonic_rs::Object::with_capacity(pairs.len());
            for (key, value) in pairs {
                object.insert(key, native_input(value)?);
            }
            object.into_value()
        }
        JsonType::Number => json!(exact_integer(
            &value
                .as_raw_number()
                .map_or_else(|| value.to_string(), |raw| raw.as_str().to_owned())
        )?),
        _ => value.clone(),
    })
}
