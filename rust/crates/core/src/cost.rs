//! Token-usage → USD cost model, ported from `cc_transcript/cost.py`.
//!
//! Feature-free: the pricing table is core data and the arithmetic mirrors the
//! Python reference operation-for-operation so both backends yield bit-identical
//! f64 costs.

use crate::types::Usage;

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

/// cost_of (cc_transcript/cost.py): the per-component and total USD cost of a turn's
/// token usage under a model's rates, or None when no pricing family matches `model`.
///
/// The cache-creation split prefers the per-TTL `usage.cache_creation` when present,
/// falling back to the flat `cache_creation_input_tokens` as 5-minute writes with no
/// 1-hour share.
pub fn cost_of(usage: &Usage, model: &str) -> Option<CostBreakdown> {
    let row = resolve_pricing(model)?;
    let (write_5m, write_1h) = match &usage.cache_creation {
        Some(cc) => (cc.ephemeral_5m_input_tokens, cc.ephemeral_1h_input_tokens),
        None => (usage.cache_creation_input_tokens, 0),
    };
    let input_cost = usage.input_tokens as f64 / MTOK * row.input;
    let output_cost = usage.output_tokens as f64 / MTOK * row.output;
    let cache_read_cost = usage.cache_read_input_tokens as f64 / MTOK * row.cache_read;
    let cache_write_cost =
        write_5m as f64 / MTOK * row.cache_write_5m + write_1h as f64 / MTOK * row.cache_write_1h;
    Some(CostBreakdown {
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
    use crate::types::CacheCreation;

    fn usage(cache_creation: Option<CacheCreation>) -> Usage {
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 2_000_000,
            cache_read_input_tokens: 4_000_000,
            cache_creation_input_tokens: 8_000_000,
            cache_creation,
            service_tier: None,
            inference_geo: None,
            server_tool_use: None,
        }
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
        let cost = cost_of(&usage(None), "claude-opus-4-8").unwrap();
        // input 1M*5, output 2M*25, cache_read 4M*0.5, cache_write 8M*6.25 (flat → 5m).
        assert_eq!(cost.input_cost, 5.0);
        assert_eq!(cost.output_cost, 50.0);
        assert_eq!(cost.cache_read_cost, 2.0);
        assert_eq!(cost.cache_write_cost, 50.0);
        assert_eq!(cost.total, 107.0);
    }

    #[test]
    fn per_ttl_split_overrides_flat_total() {
        let split = CacheCreation {
            ephemeral_5m_input_tokens: 1_000_000,
            ephemeral_1h_input_tokens: 2_000_000,
        };
        let cost = cost_of(&usage(Some(split)), "claude-opus-4-8").unwrap();
        // cache_write 1M*6.25 + 2M*10 = 26.25; flat 8M is ignored.
        assert_eq!(cost.cache_write_cost, 26.25);
    }

    #[test]
    fn unknown_model_is_none() {
        assert!(cost_of(&usage(None), "gpt-5").is_none());
    }
}
