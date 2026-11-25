use crate::models::UsageEntry;

/// API-equivalent dollar pricing per 1M tokens for a model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub usd_per_1m_in: f64,
    pub usd_per_1m_out: f64,
    pub usd_per_1m_cached_in: f64,
}

impl ModelPricing {
    /// Computes the API-equivalent cost in USD for the given token counts.
    pub fn cost_usd(&self, tokens_in: u64, tokens_out: u64, tokens_cached_in: u64) -> f64 {
        (tokens_in as f64 * self.usd_per_1m_in
            + tokens_out as f64 * self.usd_per_1m_out
            + tokens_cached_in as f64 * self.usd_per_1m_cached_in)
            / 1_000_000.0
    }
}

/// Looks up known API-equivalent pricing by normalized model name.
pub fn lookup_model_pricing(model_name: &str) -> Option<ModelPricing> {
    let m = model_name.trim().to_ascii_lowercase();
    if m.contains("neon") {
        Some(ModelPricing {
            usd_per_1m_in: 3.0,
            usd_per_1m_out: 15.0,
            usd_per_1m_cached_in: 0.15,
        })
    } else if m.contains("argon") {
        Some(ModelPricing {
            usd_per_1m_in: 2.0,
            usd_per_1m_out: 10.0,
            usd_per_1m_cached_in: 0.10,
        })
    } else if m.contains("hydrogen") {
        Some(ModelPricing {
            usd_per_1m_in: 1.5,
            usd_per_1m_out: 7.5,
            usd_per_1m_cached_in: 0.075,
        })
    } else {
        None
    }
}

/// Sums the API-equivalent USD cost across all priced entries in a usage slice.
/// Returns `(total_usd, all_entries_priced)`.
pub fn usage_api_cost_usd(usage: &[UsageEntry]) -> (f64, bool) {
    let mut total = 0.0;
    let mut all_priced = !usage.is_empty();
    for u in usage {
        if let Some(p) = lookup_model_pricing(&u.model) {
            total += p.cost_usd(u.tokens_in, u.tokens_out, u.tokens_cached_in);
        } else {
            all_priced = false;
        }
    }
    (total, all_priced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_model_pricing_variants() {
        let h = lookup_model_pricing("gemini 4 hydrogen (medium)").unwrap();
        assert_eq!(h.usd_per_1m_in, 1.5);
        assert_eq!(h.usd_per_1m_out, 7.5);
        assert_eq!(h.usd_per_1m_cached_in, 0.075);

        let n = lookup_model_pricing("orchestrator (neon)").unwrap();
        assert_eq!(n.usd_per_1m_in, 3.0);
        assert_eq!(n.usd_per_1m_out, 15.0);
        assert_eq!(n.usd_per_1m_cached_in, 0.15);

        let a = lookup_model_pricing("gemini 4 argon (medium)").unwrap();
        assert_eq!(a.usd_per_1m_in, 2.0);
        assert_eq!(a.usd_per_1m_out, 10.0);
        assert_eq!(a.usd_per_1m_cached_in, 0.10);

        assert!(lookup_model_pricing("unknown-model").is_none());
    }

    #[test]
    fn test_cost_usd_calculation() {
        let h = lookup_model_pricing("hydrogen").unwrap();
        // 1M in ($1.50) + 1M out ($7.50) + 10M cached ($0.75) = $9.75
        let cost = h.cost_usd(1_000_000, 1_000_000, 10_000_000);
        assert!((cost - 9.75).abs() < 1e-9);
    }
}
