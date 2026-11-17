use crate::config::Config;
use crate::db::TelemetryRepository;
use crate::math::changepoint::{W_LONG, evaluate_changepoint};
use crate::math::irls::fit_irls_nnls;
use crate::models::{
    ChangeEvent, ChangeEventType, ModelStatus, PartitionKey, SnapshotPayload, TrustTier,
};
use chrono::Utc;
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub type SnapshotCache = Arc<DashMap<PartitionKey, SnapshotPayload>>;

pub fn spawn_worker(
    repo: Arc<dyn TelemetryRepository>,
    cache: SnapshotCache,
    config: Arc<Config>,
    notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(config.worker_interval_secs));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = notify.notified() => {}
            }
            if let Err(err) = process_all_partitions(repo.as_ref(), &cache, &config).await {
                tracing::error!(error = ?err, "background estimation worker pass failed");
            }
        }
    });
}

pub async fn process_all_partitions(
    repo: &dyn TelemetryRepository,
    cache: &SnapshotCache,
    config: &Config,
) -> Result<(), crate::error::AppError> {
    let partitions = repo.list_active_partitions().await?;
    let now = Utc::now().timestamp();

    for key in partitions {
        if let Err(err) = process_partition(repo, cache, config, &key, now).await {
            tracing::warn!(
                provider = %key.provider,
                tier = %key.tier,
                window_type = %key.window_type,
                error = ?err,
                "failed processing partition"
            );
        }
    }
    Ok(())
}

pub async fn process_partition(
    repo: &dyn TelemetryRepository,
    cache: &SnapshotCache,
    config: &Config,
    key: &PartitionKey,
    now: i64,
) -> Result<(), crate::error::AppError> {
    let prev_snap = cache.get(key).map(|r| r.value().clone());
    let mut regime_since = prev_snap.as_ref().map(|s| s.regime_since).unwrap_or(0);
    let prev_cusum_pos = prev_snap.as_ref().map(|s| s.cusum_pos).unwrap_or(0.0);
    let prev_cusum_neg = prev_snap.as_ref().map(|s| s.cusum_neg).unwrap_or(0.0);
    let mut events = prev_snap
        .as_ref()
        .map(|s| s.recent_change_events.clone())
        .unwrap_or_default();

    let intervals = repo
        .fetch_window_intervals(key, regime_since, W_LONG)
        .await?;
    if intervals.is_empty() {
        // Check if existing snapshot became stale
        if let Some(mut snap) = prev_snap {
            let stale_cutoff = now - config.bucket_stale_days * 86_400;
            if snap.computed_at < stale_cutoff && !snap.stale {
                snap.stale = true;
                snap.recent_change_events.push(ChangeEvent {
                    timestamp: now,
                    event_type: ChangeEventType::BucketRetired,
                    description: format!("Quota bucket '{}' marked stale", key.window_type),
                    cusum_value: None,
                    corroborating_clients: None,
                    corroborating_subnets: None,
                    p_value: None,
                });
                if snap.recent_change_events.len() > 20 {
                    let excess = snap.recent_change_events.len() - 20;
                    snap.recent_change_events.drain(0..excess);
                }
                repo.save_snapshot(&snap).await?;
                cache.insert(key.clone(), snap);
            }
        }
        return Ok(());
    }

    let last_ingested = intervals.iter().map(|iv| iv.ingested_at).max().unwrap_or(now);
    let stale = now - last_ingested > config.bucket_stale_days * 86_400;
    let has_unfitted = intervals.iter().any(|iv| iv.residual_pct.is_none());
    let model_stale_cutoff = now - config.model_stale_days * 86_400;
    let has_newly_retired_model = prev_snap.as_ref().is_some_and(|prev| {
        prev.models
            .iter()
            .any(|m| m.status == ModelStatus::Active && m.last_seen_at < model_stale_cutoff)
    });

    if let Some(prev) = &prev_snap
        && !has_unfitted
        && prev.sample_count_window == intervals.len()
        && prev.stale == stale
        && !has_newly_retired_model
    {
        return Ok(());
    }

    let client_ids: Vec<String> = intervals
        .iter()
        .map(|iv| iv.client_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let clients = repo.get_clients_by_ids(&client_ids).await?;

    let Some(fit) = fit_irls_nnls(&intervals, &clients, now, config.model_stale_days) else {
        return Ok(());
    };

    let cp = evaluate_changepoint(
        &intervals,
        &fit.residuals,
        fit.sigma_hat,
        &clients,
        prev_cusum_pos,
        prev_cusum_neg,
        now,
    );

    if prev_snap.is_none() {
        events.push(ChangeEvent {
            timestamp: now,
            event_type: ChangeEventType::BucketAdded,
            description: format!("Discovered quota bucket '{}'", key.window_type),
            cusum_value: None,
            corroborating_clients: None,
            corroborating_subnets: None,
            p_value: None,
        });
    }

    // Track model additions & retirements vs previous snapshot
    if let Some(prev) = &prev_snap {
        let prev_active: HashSet<&str> = prev
            .models
            .iter()
            .filter(|m| m.status == ModelStatus::Active)
            .map(|m| m.model.as_str())
            .collect();
        let curr_active: HashSet<&str> = fit
            .models
            .iter()
            .filter(|m| m.status == ModelStatus::Active)
            .map(|m| m.model.as_str())
            .collect();

        for m in &curr_active {
            if !prev_active.contains(m) {
                events.push(ChangeEvent {
                    timestamp: now,
                    event_type: ChangeEventType::ModelAdded,
                    description: format!("Model '{m}' added to bucket '{}'", key.window_type),
                    cusum_value: None,
                    corroborating_clients: None,
                    corroborating_subnets: None,
                    p_value: None,
                });
            }
        }
        for m in &prev_active {
            if !curr_active.contains(m) {
                events.push(ChangeEvent {
                    timestamp: now,
                    event_type: ChangeEventType::ModelRetired,
                    description: format!(
                        "Model '{m}' retired/removed from bucket '{}'",
                        key.window_type
                    ),
                    cusum_value: None,
                    corroborating_clients: None,
                    corroborating_subnets: None,
                    p_value: None,
                });
            }
        }
    }

    if cp.regime_changed {
        regime_since = now;
        if let Some(ev) = cp.event {
            events.push(ev);
        }
    } else {
        // Persist fitted residuals & weights
        let updates: Vec<(i64, f64, f64)> = intervals
            .iter()
            .enumerate()
            .map(|(i, iv)| (iv.id, fit.residuals[i], fit.assigned_weights[i]))
            .collect();
        repo.update_interval_fits(&updates).await?;

        // Update client EMA |z| and trust scores (only on newly-fitted intervals where residual_pct was None,
        // or most recent interval per client to avoid double-counting across worker ticks)
        if fit.sigma_hat > 1e-9 {
            let mut latest_z_by_client: HashMap<&str, f64> = HashMap::new();
            for (i, iv) in intervals.iter().enumerate() {
                if iv.residual_pct.is_none() {
                    let abs_z = (fit.residuals[i] / fit.sigma_hat).abs().min(10.0);
                    latest_z_by_client.insert(iv.client_id.as_str(), abs_z);
                }
            }
            for (cid, abs_z) in latest_z_by_client {
                if let Some(client) = clients.get(cid) {
                    let alpha = 0.15;
                    let new_ema = if client.ema_abs_z == 0.0 {
                        abs_z
                    } else {
                        alpha * abs_z + (1.0 - alpha) * client.ema_abs_z
                    };

                    let age_secs = now - client.created_at;
                    let mut new_tier = client.trust_tier;
                    if new_tier == TrustTier::Probation
                        && client.accepted_count >= 20
                        && age_secs >= 86_400
                        && new_ema <= 2.5
                    {
                        new_tier = TrustTier::Trusted;
                    } else if new_tier == TrustTier::Trusted && new_ema > 3.5 {
                        new_tier = TrustTier::Probation;
                    }

                    let new_score = match new_tier {
                        TrustTier::Probation => 0.10,
                        TrustTier::Trusted => {
                            (1.0 / (1.0 + 0.25 * new_ema.powi(2))).clamp(0.01, 1.0)
                        }
                    };

                    repo.update_client_trust(cid, new_score, new_tier, new_ema)
                        .await?;
                }
            }
        }
    }

    if events.len() > 20 {
        let excess = events.len() - 20;
        events.drain(0..excess);
    }

    let last_ingested = intervals.iter().map(|iv| iv.ingested_at).max().unwrap_or(now);
    let stale = now - last_ingested > config.bucket_stale_days * 86_400;

    let snap = SnapshotPayload {
        provider: key.provider.clone(),
        tier: key.tier.clone(),
        window_type: key.window_type.clone(),
        computed_at: now,
        regime_since,
        sample_count_window: intervals.len(),
        effective_contributors: fit.effective_contributors,
        sigma_hat: fit.sigma_hat,
        stale,
        token_mix: fit.token_mix,
        models: fit.models,
        feature_names: fit.feature_names,
        covariance_matrix: fit.covariance_matrix,
        recent_change_events: events,
        cusum_pos: cp.cusum_pos,
        cusum_neg: cp.cusum_neg,
    };

    repo.save_snapshot(&snap).await?;
    cache.insert(key.clone(), snap);
    Ok(())
}
