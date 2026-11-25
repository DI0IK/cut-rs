use crate::config::Config;
use crate::models::{
    IntervalRecord, ModelStatus, ModelWeightEstimate, PartitionKey, SnapshotPayload, TokenMix,
    UsageEntry, normalize_key,
};
use crate::pricing::{lookup_model_pricing, usage_api_cost_usd};
use std::collections::BTreeMap;

pub const DEFAULT_WINDOW_LIMIT: usize = 1500;
const HISTORY_ROLLING_WINDOW: usize = 50;

#[derive(Debug, Clone, Default)]
struct ModelAgg {
    tokens_in: u64,
    tokens_out: u64,
    tokens_cached_in: u64,
    requests: u64,
    api_usd: f64,
    sample_count: usize,
    last_seen_at: i64,
    unpriced_delta_pct: f64,
    unpriced_blended_tokens: f64,
}

#[derive(Debug, Clone)]
pub struct PredictionResult {
    pub expected_delta_pct: f64,
    pub expected_api_usd: f64,
    pub ci_95_lower: f64,
    pub ci_95_upper: f64,
    pub pct_per_usd: f64,
}

pub fn compute_partition_estimate(
    key: &PartitionKey,
    intervals: &[IntervalRecord],
    now: i64,
    config: &Config,
) -> Option<SnapshotPayload> {
    if intervals.is_empty() {
        return None;
    }
    let cal_start = intervals.len().saturating_sub(HISTORY_ROLLING_WINDOW);
    Some(build_snapshot(key, intervals, &intervals[cal_start..], now, config))
}

pub fn compute_partition_history(
    key: &PartitionKey,
    intervals: &[IntervalRecord],
    now: i64,
    config: &Config,
    max_points: usize,
) -> Vec<SnapshotPayload> {
    if intervals.is_empty() || max_points == 0 {
        return Vec::new();
    }

    let n = intervals.len();
    let step = (n / max_points).max(1);
    let mut sample_ends: Vec<usize> = (1..=n).step_by(step).collect();
    if sample_ends.last().copied() != Some(n) {
        sample_ends.push(n);
    }

    let mut out = Vec::with_capacity(sample_ends.len());
    for end in sample_ends {
        let start = end.saturating_sub(HISTORY_ROLLING_WINDOW);
        let window = &intervals[start..end];
        let point_ts = if end == n {
            now
        } else {
            window.last().map(|iv| iv.ingested_at).unwrap_or(now)
        };
        out.push(build_snapshot(key, window, window, point_ts, config));
    }

    out
}

pub fn predict_usage(
    snap: &SnapshotPayload,
    usage: &[UsageEntry],
) -> PredictionResult {
    let (priced_usd, _) = usage_api_cost_usd(usage);
    let mut expected_delta = priced_usd * snap.pct_per_usd;

    // Fallback for any unpriced models in the prediction request
    for u in usage {
        if lookup_model_pricing(&u.model).is_none() {
            let m_norm = normalize_key(&u.model);
            if let Some(m) = snap.models.iter().find(|m| m.model == m_norm) {
                expected_delta += (u.tokens_in as f64 * m.pct_per_1m_in
                    + u.tokens_out as f64 * m.pct_per_1m_out
                    + u.tokens_cached_in as f64 * m.pct_per_1m_cached_in)
                    / 1_000_000.0;
            }
        }
    }

    let lower = if snap.pct_per_usd > 0.0 && snap.pct_per_usd_p25 > 0.0 {
        expected_delta * (snap.pct_per_usd_p25 / snap.pct_per_usd)
    } else {
        expected_delta * 0.85
    };
    let upper = if snap.pct_per_usd > 0.0 && snap.pct_per_usd_p75 > 0.0 {
        expected_delta * (snap.pct_per_usd_p75 / snap.pct_per_usd)
    } else {
        expected_delta * 1.15
    };

    PredictionResult {
        expected_delta_pct: expected_delta.max(0.0),
        expected_api_usd: priced_usd.max(0.0),
        ci_95_lower: lower.min(expected_delta).max(0.0),
        ci_95_upper: upper.max(expected_delta),
        pct_per_usd: snap.pct_per_usd,
    }
}

fn build_snapshot(
    key: &PartitionKey,
    window: &[IntervalRecord],
    calibration_window: &[IntervalRecord],
    point_ts: i64,
    config: &Config,
) -> SnapshotPayload {
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;
    let mut total_cached: u64 = 0;
    let mut total_delta_pct: f64 = 0.0;
    let mut total_api_usd: f64 = 0.0;

    let mut ratios: Vec<f64> = Vec::new();
    for iv in calibration_window {
        let (iv_usd, all_priced) = usage_api_cost_usd(&iv.usage);
        if all_priced && iv_usd > 1e-9 && iv.delta_pct > 0.0 {
            ratios.push(iv.delta_pct / iv_usd);
        }
    }

    let mut model_aggs: BTreeMap<String, ModelAgg> = BTreeMap::new();

    for iv in window {
        total_delta_pct += iv.delta_pct.max(0.0);
        let (iv_usd, _) = usage_api_cost_usd(&iv.usage);
        total_api_usd += iv_usd;

        let mut seen_in_iv = std::collections::HashSet::new();
        for u in &iv.usage {
            let m = normalize_key(&u.model);
            if m.is_empty() {
                continue;
            }
            total_in += u.tokens_in;
            total_out += u.tokens_out;
            total_cached += u.tokens_cached_in;

            let agg = model_aggs.entry(m.clone()).or_default();
            agg.tokens_in += u.tokens_in;
            agg.tokens_out += u.tokens_out;
            agg.tokens_cached_in += u.tokens_cached_in;
            agg.requests += u.request_count as u64;
            if seen_in_iv.insert(m) {
                agg.sample_count += 1;
            }
            if iv.ended_at > agg.last_seen_at {
                agg.last_seen_at = iv.ended_at;
            }
            if let Some(p) = lookup_model_pricing(&u.model) {
                agg.api_usd += p.cost_usd(u.tokens_in, u.tokens_out, u.tokens_cached_in);
            } else if iv.usage.len() == 1 && iv.delta_pct > 0.0 {
                let blended = u.tokens_in as f64
                    + 5.0 * u.tokens_out as f64
                    + 0.05 * u.tokens_cached_in as f64;
                if blended > 0.0 {
                    agg.unpriced_delta_pct += iv.delta_pct;
                    agg.unpriced_blended_tokens += blended;
                }
            }
        }
    }

    ratios.sort_by(|a, b| a.total_cmp(b));
    let (pct_per_usd, pct_per_usd_p25, pct_per_usd_p75) = if ratios.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        let med = ratios[ratios.len() / 2];
        let p25 = ratios[(ratios.len() - 1) / 4];
        let p75 = ratios[((ratios.len() - 1) * 3) / 4];
        (med, p25, p75)
    };
    let bucket_api_equivalent_usd = if pct_per_usd > 1e-9 {
        100.0 / pct_per_usd
    } else {
        0.0
    };

    let grand_tokens = (total_in + total_out + total_cached) as f64;
    let token_mix = if grand_tokens > 0.0 {
        TokenMix {
            in_share: total_in as f64 / grand_tokens,
            out_share: total_out as f64 / grand_tokens,
            cached_share: total_cached as f64 / grand_tokens,
        }
    } else {
        TokenMix::default()
    };

    let stale_cutoff = point_ts - config.model_stale_days * 86_400;
    let models = model_aggs
        .into_iter()
        .map(|(model, agg)| {
            let status = if agg.last_seen_at < stale_cutoff {
                ModelStatus::Retired
            } else {
                ModelStatus::Active
            };
            if let Some(p) = lookup_model_pricing(&model) {
                ModelWeightEstimate {
                    model,
                    priced: true,
                    usd_per_1m_in: p.usd_per_1m_in,
                    usd_per_1m_out: p.usd_per_1m_out,
                    usd_per_1m_cached_in: p.usd_per_1m_cached_in,
                    pct_per_1m_in: p.usd_per_1m_in * pct_per_usd,
                    pct_per_1m_out: p.usd_per_1m_out * pct_per_usd,
                    pct_per_1m_cached_in: p.usd_per_1m_cached_in * pct_per_usd,
                    total_tokens_in: agg.tokens_in,
                    total_tokens_out: agg.tokens_out,
                    total_tokens_cached_in: agg.tokens_cached_in,
                    total_requests: agg.requests,
                    total_api_usd: agg.api_usd,
                    sample_count: agg.sample_count,
                    last_seen_at: agg.last_seen_at,
                    status,
                }
            } else {
                let base_in = if agg.unpriced_blended_tokens > 0.0 {
                    agg.unpriced_delta_pct / (agg.unpriced_blended_tokens / 1_000_000.0)
                } else {
                    0.0
                };
                ModelWeightEstimate {
                    model,
                    priced: false,
                    usd_per_1m_in: 0.0,
                    usd_per_1m_out: 0.0,
                    usd_per_1m_cached_in: 0.0,
                    pct_per_1m_in: base_in,
                    pct_per_1m_out: base_in * 5.0,
                    pct_per_1m_cached_in: base_in * 0.05,
                    total_tokens_in: agg.tokens_in,
                    total_tokens_out: agg.tokens_out,
                    total_tokens_cached_in: agg.tokens_cached_in,
                    total_requests: agg.requests,
                    total_api_usd: 0.0,
                    sample_count: agg.sample_count,
                    last_seen_at: agg.last_seen_at,
                    status,
                }
            }
        })
        .collect();

    let last_ingested = window.last().map(|iv| iv.ingested_at).unwrap_or(point_ts);
    let stale = point_ts - last_ingested > config.bucket_stale_days * 86_400;

    SnapshotPayload {
        provider: key.provider.clone(),
        tier: key.tier.clone(),
        window_type: key.window_type.clone(),
        computed_at: point_ts,
        sample_count_window: window.len(),
        stale,
        pct_per_usd,
        bucket_api_equivalent_usd,
        pct_per_usd_p25,
        pct_per_usd_p75,
        total_api_usd,
        total_delta_pct,
        token_mix,
        models,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bucket_api_equivalent_and_prediction() {
        let now = 1_795_210_000;
        let key = PartitionKey::new("antigravity", "google ai pro", "gemini-5h");
        let config = Config::test_default("sqlite::memory:".to_string(), "/tmp/s.json".into());

        // Hydrogen: $1.5 / $7.5 / $0.075 per 1M.
        // 1M in ($1.50) + 0 out + 0 cached = $1.50 API cost.
        // If delta_pct = 15.0%, then pct_per_usd = 10.0%/USD and bucket_api_equivalent_usd = $10.00.
        let intervals = vec![IntervalRecord {
            id: 1,
            idempotency_key: "h-1".to_string(),
            provider: "antigravity".to_string(),
            tier: "google ai pro".to_string(),
            window_type: "gemini-5h".to_string(),
            started_at: now - 120,
            ended_at: now - 60,
            delta_pct: 15.0,
            quota_precision: 0.01,
            usage: vec![UsageEntry {
                model: "gemini 4 hydrogen (medium)".to_string(),
                tokens_in: 1_000_000,
                tokens_out: 0,
                tokens_cached_in: 0,
                request_count: 1,
            }],
            ingested_at: now - 60,
        }];

        let snap = compute_partition_estimate(&key, &intervals, now, &config).unwrap();
        assert!((snap.pct_per_usd - 10.0).abs() < 1e-6);
        assert!((snap.bucket_api_equivalent_usd - 10.0).abs() < 1e-6);

        let m = &snap.models[0];
        assert!(m.priced);
        assert!((m.pct_per_1m_in - 15.0).abs() < 1e-6);
        assert!((m.pct_per_1m_out - 75.0).abs() < 1e-6);
        assert!((m.pct_per_1m_cached_in - 0.75).abs() < 1e-6);

        let pred = predict_usage(
            &snap,
            &[UsageEntry {
                model: "gemini 4 neon (high)".to_string(),
                tokens_in: 1_000_000, // $3.00 -> 30.0%
                tokens_out: 0,
                tokens_cached_in: 0,
                request_count: 1,
            }],
        );
        assert!((pred.expected_api_usd - 3.0).abs() < 1e-6);
        assert!((pred.expected_delta_pct - 30.0).abs() < 1e-6);
    }
}
