//! Token cost from model pricing.
//!
//! `pricing_json` uses per-million-token prices:
//!
//! ```json
//! {"currency": "USD", "input_per_million": 3.0, "output_per_million": 15.0,
//!  "cache_read_per_million": 0.3, "cache_write_per_million": 3.75}
//! ```
//!
//! Costs are stored as millionths of the pricing currency. Since the price is
//! quoted per million tokens, `tokens * price_per_million` is already in that
//! unit, with no division needed.

use octra_llm_core::Usage;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cost {
    pub micros: i64,
    pub currency: String,
}

/// Computes the cost of a usage record, or `None` when the pricing document
/// names no price for any component (the `pricing_missing` case).
pub fn compute(usage: &Usage, pricing: &Value) -> Option<Cost> {
    let mut priced = false;
    let mut micros = 0.0_f64;

    let mut add = |tokens: u64, key: &str| {
        if let Some(price) = pricing.get(key).and_then(Value::as_f64) {
            priced = true;
            micros += tokens as f64 * price;
        }
    };
    let cached = usage.cache_read_tokens.unwrap_or(0);
    let billable_input = if usage.cache_read_included_in_input {
        usage.input_tokens.saturating_sub(cached)
    } else {
        usage.input_tokens
    };
    add(billable_input, "input_per_million");
    add(usage.output_tokens, "output_per_million");
    add(cached, "cache_read_per_million");
    add(
        usage.cache_write_tokens.unwrap_or(0),
        "cache_write_per_million",
    );

    priced.then(|| Cost {
        micros: micros.round().clamp(0.0, i64::MAX as f64) as i64,
        currency: pricing
            .get("currency")
            .and_then(Value::as_str)
            .unwrap_or("USD")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prices_each_token_class_per_million() {
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_read_tokens: Some(2_000),
            cache_write_tokens: Some(100),
            ..Usage::default()
        };
        let pricing = json!({
            "currency": "USD",
            "input_per_million": 3.0,
            "output_per_million": 15.0,
            "cache_read_per_million": 0.3,
            "cache_write_per_million": 3.75
        });
        let cost = compute(&usage, &pricing).unwrap();
        // 1000*3 + 500*15 + 2000*0.3 + 100*3.75 = 3000 + 7500 + 600 + 375
        assert_eq!(cost.micros, 11_475);
        assert_eq!(cost.currency, "USD");
    }

    #[test]
    fn cached_input_is_not_charged_twice() {
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 0,
            cache_read_tokens: Some(400),
            cache_read_included_in_input: true,
            ..Usage::default()
        };
        let pricing = json!({
            "input_per_million": 3.0,
            "cache_read_per_million": 0.3
        });
        let cost = compute(&usage, &pricing).unwrap();
        // 600 uncached input * 3 + 400 cached * 0.3 = 1,920
        assert_eq!(cost.micros, 1_920);
    }

    #[test]
    fn missing_prices_have_no_cost() {
        let usage = Usage {
            input_tokens: 10,
            ..Usage::default()
        };
        assert!(compute(&usage, &json!({})).is_none());
        assert!(compute(&usage, &json!({"currency": "USD"})).is_none());
    }

    #[test]
    fn partial_pricing_still_counts_known_components() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 10,
            ..Usage::default()
        };
        let cost = compute(&usage, &json!({"input_per_million": 2.0})).unwrap();
        assert_eq!(cost.micros, 20);
    }
}
