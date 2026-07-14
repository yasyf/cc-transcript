//! Token-usage → USD cost model, ported from `cc_transcript/cost.py`.
//!
//! Feature-free. Costs the raw usage JSON object directly (not the i64 `Usage`) so it
//! mirrors Python's arbitrary-precision int / true-division and lenient `.get()` access
//! exactly: big-int token counts divide through the raw decimal text, floats are priced,
//! and a null/`{}` per-TTL split falls back to the flat field (cc-notes 62cd44cd §7/§10).
//! The caller passes a last-wins-normalized usage value, so field reads are plain.

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::value::{field, is_py_truthy};

const MTOK: f64 = 1_000_000.0;

/// USD-per-million-token rates for a model family (cc_transcript/cost.py ModelPricing).
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

/// The per-component and total USD cost of a turn's token usage
/// (cc_transcript/cost.py CostBreakdown).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostBreakdown {
    pub input_cost: f64,
    pub output_cost: f64,
    pub cache_read_cost: f64,
    pub cache_write_cost: f64,
    pub total: f64,
}

/// Why a cost could not be computed, mapped to the matching Python exception at the pyo3
/// boundary: `NoPricing`/`MissingKey` → `KeyError` (unknown model / absent required field);
/// `CacheCreationNotObject` → `TypeError` (Python subscripts a non-dict cache_creation);
/// `NumberOverflow` → `ValueError` (a token that overflows f64, which orjson fails to load).
#[derive(Debug, PartialEq)]
pub enum CostError {
    NoPricing(String),
    MissingKey(&'static str),
    CacheCreationNotObject,
    NumberOverflow,
}

/// The family-keyed pricing table (cc_transcript/cost.py PRICING), in resolution
/// order. Hand-owned, not generated; `tests/test_cost_parity.py` is the drift guard.
pub static PRICING: &[(&str, ModelPricing)] = &[
    (
        "fable",
        ModelPricing {
            input: 10.0,
            output: 50.0,
            cache_read: 1.0,
            cache_write_5m: 12.5,
            cache_write_1h: 20.0,
        },
    ),
    (
        "opus",
        ModelPricing {
            input: 5.0,
            output: 25.0,
            cache_read: 0.5,
            cache_write_5m: 6.25,
            cache_write_1h: 10.0,
        },
    ),
    (
        "sonnet",
        ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write_5m: 3.75,
            cache_write_1h: 6.0,
        },
    ),
    (
        "haiku",
        ModelPricing {
            input: 1.0,
            output: 5.0,
            cache_read: 0.1,
            cache_write_5m: 1.25,
            cache_write_1h: 2.0,
        },
    ),
];

/// resolve_pricing (cc_transcript/cost.py): the first pricing row whose family key
/// is a substring of `model`. Family keys are mutually exclusive substrings, so at
/// most one matches; None when none does (Python raises KeyError).
pub fn resolve_pricing(model: &str) -> Option<&'static ModelPricing> {
    PRICING
        .iter()
        .find(|(family, _)| model.contains(family))
        .map(|(_, row)| row)
}

/// Python `<number> / MTOK`, mirroring orjson's JSON load: an integer within
/// [i64::MIN, u64::MAX] stays exact (Python int, decimal-shift division); a larger integer
/// decodes to f64 with float rounding; a value that overflows f64 raises (orjson's
/// JSONDecodeError / Python's OverflowError). A JSON float literal always divides as f64.
fn number_over_mtok(value: &Value) -> Result<f64, CostError> {
    let raw = value
        .as_raw_number()
        .expect("a JSON number carries raw text under arbitrary_precision");
    let text = raw.as_str();
    if !text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E'))
        && (text.parse::<i64>().is_ok() || text.parse::<u64>().is_ok())
    {
        return Ok(int_over_mtok(text));
    }
    let value = text
        .parse::<f64>()
        .expect("JSON number text parses as f64 (maybe infinite)");
    if value.is_infinite() {
        return Err(CostError::NumberOverflow);
    }
    Ok(value / MTOK)
}

/// `<int text> / 1_000_000` as the correctly-rounded f64, via a decimal-point shift so the
/// division is exact even beyond 2^53. orjson decodes `-0` to int 0, so a zero result is +0.
fn int_over_mtok(text: &str) -> f64 {
    let (sign, mag) = text.strip_prefix('-').map_or(("", text), |m| ("-", m));
    let shifted = if mag.len() > 6 {
        format!("{}.{}", &mag[..mag.len() - 6], &mag[mag.len() - 6..])
    } else {
        format!("0.{}{mag}", "0".repeat(6 - mag.len()))
    };
    match format!("{sign}{shifted}")
        .parse::<f64>()
        .expect("shifted decimal text parses")
    {
        result if result == 0.0 => 0.0,
        result => result,
    }
}

fn required<'a>(usage: &'a Value, key: &'static str) -> Result<&'a Value, CostError> {
    field(usage, key).ok_or(CostError::MissingKey(key))
}

/// cost_of (cc_transcript/cost.py): the per-component and total USD cost of the raw
/// `usage` JSON object under a model's rates.
///
/// Mirrors Python's `if (cc := usage.get("cache_creation"))`: a falsy value (null, `{}`,
/// `[]`, `0`, …) falls back to the flat `cache_creation_input_tokens` as 5-minute writes
/// with no 1-hour share; a truthy object supplies the split; a truthy non-object errors
/// (Python subscripts it and raises TypeError).
pub fn cost_of(usage: &Value, model: &str) -> Result<CostBreakdown, CostError> {
    let row = resolve_pricing(model).ok_or_else(|| CostError::NoPricing(model.to_string()))?;
    let (write_5m, write_1h) = match field(usage, "cache_creation") {
        Some(cc) if is_py_truthy(cc) => match cc.as_object() {
            Some(_) => (
                number_over_mtok(required(cc, "ephemeral_5m_input_tokens")?)?,
                number_over_mtok(required(cc, "ephemeral_1h_input_tokens")?)?,
            ),
            None => return Err(CostError::CacheCreationNotObject),
        },
        _ => (
            number_over_mtok(required(usage, "cache_creation_input_tokens")?)?,
            0.0,
        ),
    };
    let input_cost = number_over_mtok(required(usage, "input_tokens")?)? * row.input;
    let output_cost = number_over_mtok(required(usage, "output_tokens")?)? * row.output;
    let cache_read_cost =
        number_over_mtok(required(usage, "cache_read_input_tokens")?)? * row.cache_read;
    let cache_write_cost = write_5m * row.cache_write_5m + write_1h * row.cache_write_1h;
    Ok(CostBreakdown {
        input_cost,
        output_cost,
        cache_read_cost,
        cache_write_cost,
        total: input_cost + output_cost + cache_read_cost + cache_write_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(json: &str) -> Value {
        let mut value: Value = sonic_rs::from_str(json).unwrap();
        crate::value::normalize_last_wins(&mut value);
        value
    }

    #[test]
    fn resolves_family_by_substring() {
        assert_eq!(resolve_pricing("claude-opus-4-8").unwrap().input, 5.0);
        assert_eq!(
            resolve_pricing("claude-haiku-4-5-20251001").unwrap().input,
            1.0
        );
        assert_eq!(resolve_pricing("sonnet").unwrap().input, 3.0);
        assert_eq!(resolve_pricing("fable-5").unwrap().input, 10.0);
        assert!(resolve_pricing("gpt-5").is_none());
    }

    #[test]
    fn flat_cache_creation_bills_as_5m_writes() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":1000000,"output_tokens":2000000,"cache_read_input_tokens":4000000,"cache_creation_input_tokens":8000000}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.input_cost, 5.0);
        assert_eq!(cost.output_cost, 50.0);
        assert_eq!(cost.cache_read_cost, 2.0);
        assert_eq!(cost.cache_write_cost, 50.0);
        assert_eq!(cost.total, 107.0);
    }

    #[test]
    fn empty_cache_creation_object_falls_back_to_flat() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":8000000,"cache_creation":{}}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.cache_write_cost, 50.0);
    }

    #[test]
    fn null_flat_ignored_when_per_ttl_split_present() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":null,"cache_creation":{"ephemeral_5m_input_tokens":1000000,"ephemeral_1h_input_tokens":2000000}}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.cache_write_cost, 26.25);
    }

    #[test]
    fn big_int_token_beyond_f64_mantissa_divides_exactly() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":9007199254740993,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.input_cost, 45035996273.70497);
    }

    #[test]
    fn dup_keys_resolve_last_wins() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":1,"input_tokens":5000000,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.input_cost, 25.0);
    }

    #[test]
    fn float_token_is_priced() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":1500000.0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.input_cost, 7.5);
    }

    #[test]
    fn unknown_model_and_missing_field_error() {
        let full = r#"{"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#;
        assert_eq!(
            cost_of(&usage(full), "gpt-5"),
            Err(CostError::NoPricing("gpt-5".to_string()))
        );
        assert_eq!(
            cost_of(
                &usage(
                    r#"{"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#
                ),
                "claude-opus-4-8"
            ),
            Err(CostError::MissingKey("input_tokens"))
        );
    }

    #[test]
    fn int_beyond_u64_decodes_as_float() {
        // > u64::MAX: orjson loads it as a rounded float, so cost divides the f64, not the
        // exact int — the same double Rust's f64 parse yields.
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":18446744073709552735,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.input_cost, 18446744073709552735.0 / MTOK * 5.0);
    }

    #[test]
    fn negative_zero_int_is_positive_zero() {
        let cost = cost_of(
            &usage(
                r#"{"input_tokens":-0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#,
            ),
            "claude-opus-4-8",
        )
        .unwrap();
        assert_eq!(cost.input_cost, 0.0);
        assert!(cost.input_cost.is_sign_positive());
    }

    #[test]
    fn f64_overflow_int_raises() {
        let json = format!(
            r#"{{"input_tokens":1{},"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}"#,
            "0".repeat(400)
        );
        assert_eq!(
            cost_of(&usage(&json), "claude-opus-4-8"),
            Err(CostError::NumberOverflow)
        );
    }

    #[test]
    fn truthy_non_object_cache_creation_raises() {
        assert_eq!(
            cost_of(
                &usage(
                    r#"{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"cache_creation":[1]}"#,
                ),
                "claude-opus-4-8"
            ),
            Err(CostError::CacheCreationNotObject)
        );
    }
}
