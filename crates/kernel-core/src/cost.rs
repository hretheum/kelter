//! Per-run dollar cost (Phase 8, item B). A config-driven price table maps a
//! model (the policy `describe()` string, e.g. `openrouter:anthropic/…`) to a
//! per-million-token price; the run's journaled token usage is multiplied
//! through it. Honest by construction: a model **not** in the table yields
//! `None` (surfaced as JSON `null`) — never a fabricated number, and prices
//! are static config (no live fetch).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::journal::Usage;

/// Price of one model, in US dollars per **million** tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// Model → price. Built-in defaults can be extended/overridden from config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingTable {
    #[serde(flatten)]
    models: HashMap<String, ModelPrice>,
}

impl PricingTable {
    pub fn new(models: HashMap<String, ModelPrice>) -> Self {
        Self { models }
    }

    /// Built-in public prices. Local (self-hosted) inference has **no** API
    /// dollar cost, so it is priced at $0 — honest, not a guess. Frontier
    /// prices are public list prices captured on the date below; an operator
    /// overrides them from `[pricing]` in the daemon config.
    ///
    /// Prices captured 2026-07-19 (USD / 1M tokens), publicly listed:
    ///   - anthropic/claude-sonnet-5:  input 3.00, output 15.00
    ///   - anthropic/claude-opus-4-8:  input 5.00, output 25.00
    ///
    /// Only the models this project actually drives are seeded; everything
    /// else is `None` until an operator prices it.
    pub fn builtin() -> Self {
        let mut models = HashMap::new();
        // Local models: self-hosted, no per-token API bill.
        for local in ["local:qwen2.5-0.5b-instruct", "local:qwen2.5-0.5b", "local"] {
            models.insert(
                local.to_string(),
                ModelPrice {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                },
            );
        }
        models.insert(
            "openrouter:anthropic/claude-sonnet-5".to_string(),
            ModelPrice {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
            },
        );
        models.insert(
            "openrouter:anthropic/claude-opus-4-8".to_string(),
            ModelPrice {
                input_per_mtok: 5.0,
                output_per_mtok: 25.0,
            },
        );
        Self { models }
    }

    /// Config entries override/extend the built-in defaults, keyed by model.
    pub fn merged_with(mut self, overrides: HashMap<String, ModelPrice>) -> Self {
        self.models.extend(overrides);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    fn price_for(&self, model: &str) -> Option<&ModelPrice> {
        // Exact match first, then a `scheme:` prefix fallback (e.g. any
        // unpriced `local:*` still resolves to the generic `local` entry).
        self.models.get(model).or_else(|| {
            model
                .split_once(':')
                .and_then(|(scheme, _)| self.models.get(scheme))
        })
    }

    /// Dollar cost for a run's usage under this table, or `None` when the
    /// model is unpriced (→ JSON `null`; never a made-up number).
    pub fn cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        let price = self.price_for(model)?;
        let dollars = price.input_per_mtok * (usage.prompt_tokens as f64) / 1_000_000.0
            + price.output_per_mtok * (usage.completion_tokens as f64) / 1_000_000.0;
        // Round to cents-of-a-milli ($1e-6) to avoid float noise in the API.
        Some((dollars * 1_000_000.0).round() / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(p: u64, c: u64) -> Usage {
        Usage {
            prompt_tokens: p,
            completion_tokens: c,
            calls: 1,
        }
    }

    #[test]
    fn known_model_costs_tokens_at_list_price() {
        let t = PricingTable::builtin();
        // 1M input + 1M output of sonnet-5 = 3 + 15 = $18.
        let c = t
            .cost(
                "openrouter:anthropic/claude-sonnet-5",
                &usage(1_000_000, 1_000_000),
            )
            .unwrap();
        assert!((c - 18.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn local_model_is_zero_not_null() {
        let t = PricingTable::builtin();
        assert_eq!(
            t.cost("local:qwen2.5-0.5b-instruct", &usage(500, 500)),
            Some(0.0)
        );
        // Prefix fallback: an unseeded local variant still resolves to $0.
        assert_eq!(t.cost("local:some-other-gguf", &usage(500, 500)), Some(0.0));
    }

    #[test]
    fn unknown_model_is_none_never_fabricated() {
        let t = PricingTable::builtin();
        assert_eq!(
            t.cost("openrouter:mystery/model-x", &usage(1000, 1000)),
            None
        );
        assert_eq!(t.cost("openai:gpt-whatever", &usage(1000, 1000)), None);
    }

    #[test]
    fn config_overrides_builtin() {
        let mut over = HashMap::new();
        over.insert(
            "openrouter:anthropic/claude-sonnet-5".to_string(),
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
            },
        );
        let t = PricingTable::builtin().merged_with(over);
        assert_eq!(
            t.cost("openrouter:anthropic/claude-sonnet-5", &usage(1_000_000, 0)),
            Some(1.0)
        );
    }
}
