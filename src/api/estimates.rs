use crate::api::AppState;
use crate::error::AppError;
use crate::models::{
    ChangeEvent, ModelStatus, ModelWeightEstimate, SnapshotPayload, UsageEntry, normalize_key,
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
    pub stale: bool,
    pub models: Vec<ModelWeightEstimate>,
    pub recent_change_events: Vec<ChangeEvent>,
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
            stale: snap.stale,
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
