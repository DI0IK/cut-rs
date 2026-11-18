use crate::config::Config;
use crate::math::changepoint::{W_LONG, evaluate_changepoint};
use crate::math::irls::fit_irls_nnls;
use crate::models::{
    ChangeEvent, ChangeEventType, IntervalRecord, ModelStatus, PartitionKey, SnapshotPayload,
};
use std::collections::HashSet;

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
    let mut cusum_pos = 0.0;
    let mut cusum_neg = 0.0;
    let mut prev_active: Option<HashSet<String>> = None;
    let mut events: Vec<ChangeEvent> = vec![ChangeEvent {
        timestamp: intervals[0].ingested_at,
        event_type: ChangeEventType::BucketAdded,
        description: format!("Discovered quota bucket '{}'", key.window_type),
        cusum_value: None,
        p_value: None,
    }];

    for end in sample_ends {
        let win_start = regime_start_idx.max(end.saturating_sub(W_LONG));
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
            cusum_pos,
            cusum_neg,
            point_ts,
        );
        cusum_pos = cp.cusum_pos;
        cusum_neg = cp.cusum_neg;

        let curr_active: HashSet<String> = fit
            .models
            .iter()
            .filter(|m| m.status == ModelStatus::Active)
            .map(|m| m.model.clone())
            .collect();

        if let Some(prev) = &prev_active {
            for m in curr_active.difference(prev) {
                events.push(ChangeEvent {
                    timestamp: point_ts,
                    event_type: ChangeEventType::ModelAdded,
                    description: format!("Model '{m}' added to bucket '{}'", key.window_type),
                    cusum_value: None,
                    p_value: None,
                });
            }
            for m in prev.difference(&curr_active) {
                events.push(ChangeEvent {
                    timestamp: point_ts,
                    event_type: ChangeEventType::ModelRetired,
                    description: format!("Model '{m}' retired from bucket '{}'", key.window_type),
                    cusum_value: None,
                    p_value: None,
                });
            }
        }
        prev_active = Some(curr_active);

        if let Some(ev) = cp.event {
            events.push(ev);
        }
        if cp.regime_changed {
            regime_start_idx = end.saturating_sub(1);
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
            models: fit.models,
            recent_change_events: events.clone(),
        });
    }

    out
}
