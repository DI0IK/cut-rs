use crate::config::Config;
use crate::math::changepoint::{RECENT_TAIL, W_LONG, evaluate_changepoint};
use crate::math::irls::fit_irls_nnls;
use crate::models::{
    ChangeEvent, ChangeEventType, ConfidenceInterval, Identifiability, IntervalRecord, ModelStatus,
    ModelWeightEstimate, PartitionKey, SnapshotPayload,
};
use std::collections::{HashMap, HashSet};

pub fn compute_partition_estimate(
    key: &PartitionKey,
    intervals: &[IntervalRecord],
    now: i64,
    config: &Config,
) -> Option<SnapshotPayload> {
    compute_partition_history(key, intervals, now, config, 40).pop()
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
    let mut regime_start_idx = 0usize;
    let mut pre_regime_rate: Option<f64> = None;
    let mut prev_status: Option<HashMap<String, ModelStatus>> = None;
    let mut last_known_estimates: HashMap<String, ModelWeightEstimate> = HashMap::new();
    let mut events: Vec<ChangeEvent> = vec![ChangeEvent {
        timestamp: intervals[0].ingested_at,
        event_type: ChangeEventType::BucketAdded,
        description: format!("Discovered quota bucket '{}'", key.window_type),
        cusum_value: None,
        p_value: None,
    }];

    for end in sample_ends {
        let long_start = end.saturating_sub(W_LONG);

        // If a previous regime_changed was triggered by a transient burst and the most recent
        // RECENT_TAIL samples have reverted to the pre-regime rate, recover the full window.
        if regime_start_idx > long_start
            && end >= regime_start_idx + RECENT_TAIL * 2
            && let Some(base_rate) = pre_regime_rate
        {
            let tail_rates: Vec<f64> = intervals[end - RECENT_TAIL..end]
                .iter()
                .filter_map(|iv| {
                    let tot: u64 = iv
                        .usage
                        .iter()
                        .map(|u| u.tokens_in + u.tokens_out + u.tokens_cached_in)
                        .sum();
                    if tot > 0 {
                        Some(iv.delta_pct / (tot as f64 / 1_000_000.0))
                    } else {
                        None
                    }
                })
                .collect();
            if tail_rates.len() >= RECENT_TAIL / 2 {
                let mut sorted = tail_rates;
                sorted.sort_by(|a, b| a.total_cmp(b));
                let med_tail = sorted[sorted.len() / 2];
                if (med_tail - base_rate).abs() <= base_rate.abs() * 0.5 + 0.5 {
                    regime_start_idx = long_start;
                    pre_regime_rate = None;
                }
            }
        }

        let win_start = regime_start_idx.max(long_start);
        let window = &intervals[win_start..end];
        if window.is_empty() {
            continue;
        }

        let point_ts = if end == n {
            now
        } else {
            window.last().map(|iv| iv.ingested_at).unwrap_or(now)
        };

        let Some(fit) = fit_irls_nnls(window, point_ts, config.model_stale_days) else {
            continue;
        };

        let cp = evaluate_changepoint(
            window,
            &fit.residuals,
            fit.sigma_hat,
            0.0,
            0.0,
            point_ts,
        );

        let stale_cutoff = point_ts - config.model_stale_days * 86_400;

        // Track all models seen across the W_LONG window so a regime reset does not
        // falsely retire or hide active models that haven't been used in the new tail yet.
        let mut long_last_seen: HashMap<String, i64> = HashMap::new();
        for iv in &intervals[long_start..end] {
            for u in &iv.usage {
                let m = u.model.trim().to_ascii_lowercase();
                if m.is_empty() {
                    continue;
                }
                let entry = long_last_seen.entry(m).or_insert(0);
                if iv.ended_at > *entry {
                    *entry = iv.ended_at;
                }
            }
        }

        let mut models = fit.models;
        for m in &models {
            last_known_estimates.insert(m.model.clone(), m.clone());
        }
        let fitted_names: HashSet<String> = models.iter().map(|m| m.model.clone()).collect();
        for (m_name, last_seen_at) in &long_last_seen {
            if fitted_names.contains(m_name) {
                continue;
            }
            let status = if *last_seen_at < stale_cutoff {
                ModelStatus::Retired
            } else {
                ModelStatus::Active
            };
            if let Some(prev) = last_known_estimates.get(m_name) {
                let mut carried = prev.clone();
                carried.last_seen_at = *last_seen_at;
                carried.status = status;
                carried.identifiability = Identifiability::InsufficientSamples;
                models.push(carried);
            } else {
                models.push(ModelWeightEstimate {
                    model: m_name.clone(),
                    pct_per_1m_in: 0.0,
                    pct_per_1m_in_ci: ConfidenceInterval {
                        lower: 0.0,
                        upper: 0.0,
                    },
                    pct_per_1m_out: 0.0,
                    pct_per_1m_out_ci: ConfidenceInterval {
                        lower: 0.0,
                        upper: 0.0,
                    },
                    pct_per_1m_cached_in: 0.0,
                    pct_per_1m_cached_in_ci: ConfidenceInterval {
                        lower: 0.0,
                        upper: 0.0,
                    },
                    identifiability: Identifiability::InsufficientSamples,
                    sample_count: 0,
                    last_seen_at: *last_seen_at,
                    status,
                    token_mix: None,
                });
            }
        }
        models.sort_by(|a, b| a.model.cmp(&b.model));

        let curr_status: HashMap<String, ModelStatus> = models
            .iter()
            .map(|m| (m.model.clone(), m.status))
            .collect();

        if let Some(prev) = &prev_status {
            for (m, &status) in &curr_status {
                match prev.get(m) {
                    None if status == ModelStatus::Active => {
                        events.push(ChangeEvent {
                            timestamp: point_ts,
                            event_type: ChangeEventType::ModelAdded,
                            description: format!("Model '{m}' added to bucket '{}'", key.window_type),
                            cusum_value: None,
                            p_value: None,
                        });
                    }
                    Some(ModelStatus::Retired) if status == ModelStatus::Active => {
                        events.push(ChangeEvent {
                            timestamp: point_ts,
                            event_type: ChangeEventType::ModelAdded,
                            description: format!("Model '{m}' added to bucket '{}'", key.window_type),
                            cusum_value: None,
                            p_value: None,
                        });
                    }
                    Some(ModelStatus::Active) if status == ModelStatus::Retired => {
                        events.push(ChangeEvent {
                            timestamp: point_ts,
                            event_type: ChangeEventType::ModelRetired,
                            description: format!(
                                "Model '{m}' retired from bucket '{}'",
                                key.window_type
                            ),
                            cusum_value: None,
                            p_value: None,
                        });
                    }
                    _ => {}
                }
            }
        }
        prev_status = Some(curr_status);

        if let Some(ev) = cp.event {
            events.push(ev);
        }
        if cp.regime_changed {
            let mut base_rates: Vec<f64> = window[..window.len().saturating_sub(RECENT_TAIL)]
                .iter()
                .filter_map(|iv| {
                    let tot: u64 = iv
                        .usage
                        .iter()
                        .map(|u| u.tokens_in + u.tokens_out + u.tokens_cached_in)
                        .sum();
                    if tot > 0 {
                        Some(iv.delta_pct / (tot as f64 / 1_000_000.0))
                    } else {
                        None
                    }
                })
                .collect();
            if !base_rates.is_empty() {
                base_rates.sort_by(|a, b| a.total_cmp(b));
                pre_regime_rate = Some(base_rates[base_rates.len() / 2]);
            }
            regime_start_idx = end.saturating_sub(RECENT_TAIL);
        }

        if events.len() > 20 {
            let excess = events.len() - 20;
            events.drain(0..excess);
        }

        let last_ingested = window.last().map(|iv| iv.ingested_at).unwrap_or(point_ts);
        let stale = point_ts - last_ingested > config.bucket_stale_days * 86_400;
        let regime_since = window.first().map(|iv| iv.ingested_at).unwrap_or(0);

        out.push(SnapshotPayload {
            provider: key.provider.clone(),
            tier: key.tier.clone(),
            window_type: key.window_type.clone(),
            computed_at: point_ts,
            regime_since,
            sample_count_window: window.len(),
            sigma_hat: fit.sigma_hat,
            stale,
            token_mix: fit.token_mix,
            models,
            recent_change_events: events.clone(),
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UsageEntry;

    #[test]
    fn test_regime_change_does_not_retire_active_models() {
        let now = 1_795_210_000;
        let key = PartitionKey::new("antigravity", "pro", "gemini-5h");
        let config = Config::test_default("sqlite::memory:".to_string(), "/tmp/s.json".into());

        let mut intervals = Vec::new();
        for i in 0..30 {
            intervals.push(IntervalRecord {
                id: i + 1,
                idempotency_key: format!("a-{i}"),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 3600 + i * 30,
                ended_at: now - 3590 + i * 30,
                delta_pct: 1.0,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "model-a".to_string(),
                    tokens_in: 1_000_000,
                    tokens_out: 0,
                    tokens_cached_in: 0,
                    request_count: 1,
                }],
                ingested_at: now - 3590 + i * 30,
            });
        }
        for i in 0..10 {
            intervals.push(IntervalRecord {
                id: 31 + i,
                idempotency_key: format!("b-{i}"),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 600 + i * 30,
                ended_at: now - 590 + i * 30,
                delta_pct: 5.0,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "model-b".to_string(),
                    tokens_in: 1_000_000,
                    tokens_out: 0,
                    tokens_cached_in: 0,
                    request_count: 1,
                }],
                ingested_at: now - 590 + i * 30,
            });
        }

        let snap = compute_partition_estimate(&key, &intervals, now, &config)
            .expect("estimate should succeed");
        let model_a = snap
            .models
            .iter()
            .find(|m| m.model == "model-a")
            .expect("model-a should remain present after regime change");
        assert_eq!(model_a.status, ModelStatus::Active);
        assert!(
            !snap
                .recent_change_events
                .iter()
                .any(|e| e.event_type == ChangeEventType::ModelRetired),
            "non-stale model-a should not emit ModelRetired"
        );
    }

    #[test]
    fn test_transient_spike_burst_recovers_baseline_window() {
        let now = 1_795_210_000;
        let key = PartitionKey::new("antigravity", "pro", "gemini-5h");
        let config = Config::test_default("sqlite::memory:".to_string(), "/tmp/s.json".into());

        let mut intervals = Vec::new();
        // 30 baseline intervals at 1.0 pct / 1M in
        for i in 0..30 {
            intervals.push(IntervalRecord {
                id: i + 1,
                idempotency_key: format!("base-{i}"),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 5000 + i * 30,
                ended_at: now - 4990 + i * 30,
                delta_pct: 1.0,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "model-a".to_string(),
                    tokens_in: 1_000_000,
                    tokens_out: 0,
                    tokens_cached_in: 0,
                    request_count: 1,
                }],
                ingested_at: now - 4990 + i * 30,
            });
        }
        // 6 transient spike intervals at 10.0 pct / 1M in
        for i in 0..6 {
            intervals.push(IntervalRecord {
                id: 31 + i,
                idempotency_key: format!("burst-{i}"),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 2000 + i * 30,
                ended_at: now - 1990 + i * 30,
                delta_pct: 10.0,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "model-b".to_string(),
                    tokens_in: 1_000_000,
                    tokens_out: 0,
                    tokens_cached_in: 0,
                    request_count: 1,
                }],
                ingested_at: now - 1990 + i * 30,
            });
        }
        // 15 post-burst intervals back at 1.0 pct / 1M in
        for i in 0..15 {
            intervals.push(IntervalRecord {
                id: 37 + i,
                idempotency_key: format!("post-{i}"),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 1000 + i * 30,
                ended_at: now - 990 + i * 30,
                delta_pct: 1.0,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "model-a".to_string(),
                    tokens_in: 1_000_000,
                    tokens_out: 0,
                    tokens_cached_in: 0,
                    request_count: 1,
                }],
                ingested_at: now - 990 + i * 30,
            });
        }

        let snap = compute_partition_estimate(&key, &intervals, now, &config)
            .expect("estimate should succeed");
        assert!(
            snap.sample_count_window >= 45,
            "should recover full baseline window after transient burst, got {}",
            snap.sample_count_window
        );
        let model_a = snap.models.iter().find(|m| m.model == "model-a").unwrap();
        assert!(
            (model_a.pct_per_1m_in - 1.0).abs() < 0.2,
            "model-a rate should recover to ~1.0, got {}",
            model_a.pct_per_1m_in
        );
    }
}
