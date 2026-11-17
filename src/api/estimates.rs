use crate::api::AppState;
use crate::error::AppError;
use crate::models::{
    ChangeEvent, ModelStatus, ModelWeightEstimate, SnapshotPayload, TokenMix, UsageEntry,
    normalize_key,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub struct EstimatesQuery {
    pub window_type: Option<String>,
    #[serde(default)]
    pub include_stale: bool,
}

#[derive(Debug, Serialize)]
pub struct WindowEstimateView {
    pub window_type: String,
    pub computed_at: i64,
    pub regime_since: i64,
    pub sample_count_window: usize,
    pub effective_contributors: f64,
    pub sigma_hat: f64,
    pub stale: bool,
    pub token_mix: TokenMix,
    pub models: Vec<ModelWeightEstimate>,
    pub recent_change_events: Vec<ChangeEvent>,
}

#[derive(Debug, Serialize)]
pub struct PartitionSummary {
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub sample_count_window: usize,
    pub effective_contributors: f64,
    pub token_mix: TokenMix,
    pub stale: bool,
    pub computed_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PartitionsResponse {
    pub partitions: Vec<PartitionSummary>,
}

pub async fn list_partitions(State(state): State<AppState>) -> Json<PartitionsResponse> {
    let mut partitions: Vec<PartitionSummary> = state
        .cache
        .iter()
        .map(|entry| {
            let s = entry.value();
            PartitionSummary {
                provider: s.provider.clone(),
                tier: s.tier.clone(),
                window_type: s.window_type.clone(),
                sample_count_window: s.sample_count_window,
                effective_contributors: s.effective_contributors,
                token_mix: s.token_mix,
                stale: s.stale,
                computed_at: s.computed_at,
            }
        })
        .collect();

    partitions.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.tier.cmp(&b.tier))
            .then(a.window_type.cmp(&b.window_type))
    });

    Json(PartitionsResponse { partitions })
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub window_type: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryModelPoint {
    pub model: String,
    pub pct_per_1m_in: f64,
    pub pct_per_1m_out: f64,
    pub pct_per_1m_cached_in: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistorySnapshotPoint {
    pub tier: String,
    pub window_type: String,
    pub computed_at: i64,
    pub regime_since: i64,
    pub sample_count_window: usize,
    pub effective_contributors: f64,
    pub token_mix: TokenMix,
    pub models: Vec<HistoryModelPoint>,
    pub recent_change_events: Vec<ChangeEvent>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub provider: String,
    pub tier: String,
    pub points: Vec<HistorySnapshotPoint>,
}

pub async fn get_estimates_history(
    State(state): State<AppState>,
    Path((provider, tier)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, AppError> {
    let provider_norm = normalize_key(&provider);
    let tier_norm = normalize_key(&tier);
    let wt_norm = query
        .window_type
        .as_deref()
        .map(normalize_key)
        .unwrap_or_else(|| "all".to_string());
    let limit = query.limit.unwrap_or(180).clamp(1, 500);

    let cache_key = (provider_norm.clone(), tier_norm.clone(), wt_norm.clone());
    if let Some(cached) = state.history_cache.get(&cache_key) {
        let mut points = cached.value().clone();
        if points.len() > limit {
            points.truncate(limit);
        }
        return Ok(Json(HistoryResponse {
            provider: provider_norm,
            tier: tier_norm,
            points,
        }));
    }

    let tier_filter = if tier_norm == "all" || tier_norm == "*" {
        None
    } else {
        Some(tier_norm.as_str())
    };
    let wt_filter = if wt_norm == "all" || wt_norm == "*" {
        None
    } else {
        Some(wt_norm.as_str())
    };

    let matching_keys: Vec<_> = state
        .cache
        .iter()
        .filter(|e| {
            let k = e.key();
            k.provider == provider_norm
                && tier_filter.is_none_or(|t| k.tier == t)
                && wt_filter.is_none_or(|w| k.window_type == w)
        })
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();

    let mut points = Vec::new();
    for (key, snap) in matching_keys {
        let mut intervals = state
            .repo
            .fetch_window_intervals(&key, snap.regime_since, crate::math::changepoint::W_LONG)
            .await?;
        if intervals.is_empty() {
            continue;
        }
        // Chronological order
        intervals.sort_by_key(|iv| (iv.ended_at, iv.id));
        let client_ids: Vec<String> = intervals
            .iter()
            .map(|iv| iv.client_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let clients = state.repo.get_clients_by_ids(&client_ids).await?;

        let n = intervals.len();
        let max_checkpoints = 40usize;
        let indices: Vec<usize> = if n <= max_checkpoints {
            (1..=n).collect()
        } else {
            (0..max_checkpoints)
                .map(|i| 1 + (i * (n - 1)) / (max_checkpoints - 1))
                .collect()
        };

        let last_idx = indices.len().saturating_sub(1);
        for (ci, &end_count) in indices.iter().enumerate() {
            let slice = &intervals[..end_count];
            let ts = slice.last().map(|iv| iv.ended_at).unwrap_or(snap.computed_at);
            let Some(fit) = crate::math::irls::fit_irls_nnls(
                slice,
                &clients,
                ts,
                state.config.model_stale_days,
            ) else {
                continue;
            };
            if fit.models.is_empty() {
                continue;
            }
            points.push(HistorySnapshotPoint {
                tier: key.tier.clone(),
                window_type: key.window_type.clone(),
                computed_at: ts,
                regime_since: snap.regime_since,
                sample_count_window: slice.len(),
                effective_contributors: fit.effective_contributors,
                token_mix: fit.token_mix,
                models: fit
                    .models
                    .into_iter()
                    .filter(|m| m.status == ModelStatus::Active)
                    .map(|m| HistoryModelPoint {
                        model: m.model,
                        pct_per_1m_in: m.pct_per_1m_in,
                        pct_per_1m_out: m.pct_per_1m_out,
                        pct_per_1m_cached_in: m.pct_per_1m_cached_in,
                    })
                    .collect(),
                recent_change_events: if ci == last_idx {
                    snap.recent_change_events.clone()
                } else {
                    Vec::new()
                },
            });
        }
    }

    // Fallback to persisted snapshots if no intervals were found
    if points.is_empty() {
        let snaps = state
            .repo
            .load_snapshot_history(&provider_norm, tier_filter, wt_filter, limit)
            .await?;
        points = snaps
            .into_iter()
            .map(|s| HistorySnapshotPoint {
                tier: s.tier,
                window_type: s.window_type,
                computed_at: s.computed_at,
                regime_since: s.regime_since,
                sample_count_window: s.sample_count_window,
                effective_contributors: s.effective_contributors,
                token_mix: s.token_mix,
                models: s
                    .models
                    .into_iter()
                    .filter(|m| m.status == ModelStatus::Active)
                    .map(|m| HistoryModelPoint {
                        model: m.model,
                        pct_per_1m_in: m.pct_per_1m_in,
                        pct_per_1m_out: m.pct_per_1m_out,
                        pct_per_1m_cached_in: m.pct_per_1m_cached_in,
                    })
                    .collect(),
                recent_change_events: s.recent_change_events,
            })
            .collect();
    } else {
        points.sort_by_key(|p| p.computed_at);
    }

    state.history_cache.insert(cache_key, points.clone());
    if points.len() > limit {
        points.truncate(limit);
    }

    Ok(Json(HistoryResponse {
        provider: provider_norm,
        tier: tier_norm,
        points,
    }))
}

#[derive(Debug, Serialize)]
pub struct EstimatesResponse {
    pub provider: String,
    pub tier: String,
    pub windows: Vec<WindowEstimateView>,
}

pub async fn get_estimates(
    State(state): State<AppState>,
    Path((provider, tier)): Path<(String, String)>,
    Query(query): Query<EstimatesQuery>,
) -> Result<impl IntoResponse, AppError> {
    let provider = normalize_key(&provider);
    let tier = normalize_key(&tier);
    let filter_window = query.window_type.as_deref().map(normalize_key);

    let mut windows = Vec::new();
    for entry in state.cache.iter() {
        let key = entry.key();
        if key.provider != provider || key.tier != tier {
            continue;
        }
        if let Some(ref wt) = filter_window
            && &key.window_type != wt
        {
            continue;
        }
        let snap = entry.value();
        if snap.stale && !query.include_stale {
            continue;
        }
        let active_models: Vec<ModelWeightEstimate> = snap
            .models
            .iter()
            .filter(|m| m.status == ModelStatus::Active || query.include_stale)
            .cloned()
            .collect();

        windows.push(WindowEstimateView {
            window_type: snap.window_type.clone(),
            computed_at: snap.computed_at,
            regime_since: snap.regime_since,
            sample_count_window: snap.sample_count_window,
            effective_contributors: snap.effective_contributors,
            sigma_hat: snap.sigma_hat,
            stale: snap.stale,
            token_mix: snap.token_mix,
            models: active_models,
            recent_change_events: snap.recent_change_events.clone(),
        });
    }

    windows.sort_by(|a, b| a.window_type.cmp(&b.window_type));

    if windows.is_empty() {
        return Err(AppError::NotFound(format!(
            "no estimates available for {provider}/{tier}"
        )));
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=10"),
    );

    Ok((
        headers,
        Json(EstimatesResponse {
            provider,
            tier,
            windows,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PredictRequest {
    pub window_type: Option<String>,
    pub usage: Vec<UsageEntry>,
}

#[derive(Debug, Serialize)]
pub struct PredictResponse {
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub expected_delta_pct: f64,
    pub ci_95_lower: f64,
    pub ci_95_upper: f64,
    pub sigma_hat: f64,
}

pub async fn predict_quota(
    State(state): State<AppState>,
    Path((provider, tier)): Path<(String, String)>,
    Json(req): Json<PredictRequest>,
) -> Result<Json<PredictResponse>, AppError> {
    if req.usage.is_empty() {
        return Err(AppError::BadRequest("usage must not be empty".to_string()));
    }

    let provider = normalize_key(&provider);
    let tier = normalize_key(&tier);
    let target_wt = req.window_type.as_deref().map(normalize_key);

    let mut candidates: Vec<SnapshotPayload> = state
        .cache
        .iter()
        .filter(|e| {
            let k = e.key();
            k.provider == provider
                && k.tier == tier
                && target_wt.as_ref().is_none_or(|wt| &k.window_type == wt)
        })
        .map(|e| e.value().clone())
        .collect();

    candidates.sort_by(|a, b| a.stale.cmp(&b.stale).then(a.window_type.cmp(&b.window_type)));
    let snap = candidates.into_iter().next().ok_or_else(|| {
        AppError::NotFound(format!("no model snapshot available for {provider}/{tier}"))
    })?;

    let p = snap.feature_names.len();
    let feat_idx: HashMap<&str, usize> = snap
        .feature_names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();

    let mut x = vec![0.0_f64; p];
    let mut w = vec![0.0_f64; p];

    for m in &snap.models {
        if let Some(&idx) = feat_idx.get(format!("{}:in", m.model).as_str()) {
            w[idx] = m.pct_per_1m_in;
        }
        if let Some(&idx) = feat_idx.get(format!("{}:out", m.model).as_str()) {
            w[idx] = m.pct_per_1m_out;
        }
        if let Some(&idx) = feat_idx.get(format!("{}:cached_in", m.model).as_str()) {
            w[idx] = m.pct_per_1m_cached_in;
        }
    }

    for u in &req.usage {
        let model = normalize_key(&u.model);
        if let Some(&idx) = feat_idx.get(format!("{model}:in").as_str()) {
            x[idx] += u.tokens_in as f64 / 1_000_000.0;
        }
        if let Some(&idx) = feat_idx.get(format!("{model}:out").as_str()) {
            x[idx] += u.tokens_out as f64 / 1_000_000.0;
        }
        if let Some(&idx) = feat_idx.get(format!("{model}:cached_in").as_str()) {
            x[idx] += u.tokens_cached_in as f64 / 1_000_000.0;
        }
    }

    let mu: f64 = x.iter().zip(w.iter()).map(|(xi, wi)| xi * wi).sum();

    // Compute x^T * Sigma_w * x
    let mut x_cov_x = 0.0_f64;
    if snap.covariance_matrix.len() == p * p {
        for r in 0..p {
            if x[r] == 0.0 {
                continue;
            }
            for c in 0..p {
                x_cov_x += x[r] * snap.covariance_matrix[r * p + c] * x[c];
            }
        }
    }
    let pred_se = (snap.sigma_hat.powi(2) + x_cov_x.max(0.0)).sqrt();

    Ok(Json(PredictResponse {
        provider,
        tier,
        window_type: snap.window_type,
        expected_delta_pct: mu,
        ci_95_lower: (mu - 1.96 * pred_se).max(0.0),
        ci_95_upper: mu + 1.96 * pred_se,
        sigma_hat: snap.sigma_hat,
    }))
}
