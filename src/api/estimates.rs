use crate::api::AppState;
use crate::cli::common::load_state;
use crate::error::AppError;
use crate::estimator::{
    DEFAULT_WINDOW_LIMIT, compute_partition_estimate, compute_partition_history, predict_usage,
};
use crate::models::{
    ModelStatus, ModelWeightEstimate, PartitionKey, QuotaChangeEvent, QuotaSnapshotRow, TokenMix,
    UsageEntry, normalize_key,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::IntoResponse;
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct CurrentBucketStatus {
    pub bucket_id: String,
    pub remaining_pct: f64,
    pub reset_time: Option<String>,
    pub started_at: i64,
}

#[derive(Debug, Serialize)]
pub struct QuotaStatusResponse {
    pub provider: String,
    pub tier: Option<String>,
    pub buckets: Vec<CurrentBucketStatus>,
    pub history: Vec<QuotaSnapshotRow>,
    pub changes: Vec<QuotaChangeEvent>,
}

#[derive(Debug, Deserialize)]
pub struct QuotaQuery {
    pub bucket_id: Option<String>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub include_anomalies: bool,
}

pub async fn get_quota_status(
    State(state): State<AppState>,
    Query(query): Query<QuotaQuery>,
) -> Result<Json<QuotaStatusResponse>, AppError> {
    let cli_state = load_state(&state.config.state_file_path);
    let tier = cli_state.as_ref().and_then(|s| s.tier.clone());
    let mut buckets: Vec<CurrentBucketStatus> = cli_state
        .map(|s| {
            s.buckets
                .into_iter()
                .map(|(bucket_id, b)| CurrentBucketStatus {
                    bucket_id,
                    remaining_pct: (b.last_remaining_fraction * 100.0).clamp(0.0, 100.0),
                    reset_time: b.effective_reset_time().map(str::to_string),
                    started_at: b.started_at,
                })
                .collect()
        })
        .unwrap_or_default();
    buckets.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));

    let limit = query.limit.unwrap_or(500).clamp(1, 2000);
    let history = state
        .repo
        .fetch_quota_history(query.bucket_id.as_deref(), limit)
        .await?;
    let changes = state
        .repo
        .fetch_quota_changes(query.bucket_id.as_deref(), query.include_anomalies, limit)
        .await?;

    Ok(Json(QuotaStatusResponse {
        provider: "antigravity".to_string(),
        tier,
        buckets,
        history,
        changes,
    }))
}

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
    pub sample_count_window: usize,
    pub stale: bool,
    pub pct_per_usd: f64,
    pub bucket_api_equivalent_usd: f64,
    pub pct_per_usd_p25: f64,
    pub pct_per_usd_p75: f64,
    pub total_api_usd: f64,
    pub total_delta_pct: f64,
    pub token_mix: TokenMix,
    pub models: Vec<ModelWeightEstimate>,
}

#[derive(Debug, Serialize)]
pub struct PartitionSummary {
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub sample_count_window: usize,
    pub bucket_api_equivalent_usd: f64,
    pub pct_per_usd: f64,
    pub token_mix: TokenMix,
    pub stale: bool,
    pub computed_at: i64,
}

#[derive(Debug, Serialize)]
pub struct PartitionsResponse {
    pub partitions: Vec<PartitionSummary>,
}

pub async fn list_partitions(
    State(state): State<AppState>,
) -> Result<Json<PartitionsResponse>, AppError> {
    let keys = state.repo.list_active_partitions().await?;
    let now = Utc::now().timestamp();
    let mut partitions = Vec::new();

    for key in keys {
        let mut intervals = state
            .repo
            .fetch_window_intervals(&key, 0, DEFAULT_WINDOW_LIMIT)
            .await?;
        intervals.sort_by_key(|iv| (iv.ended_at, iv.id));
        if let Some(snap) = compute_partition_estimate(&key, &intervals, now, &state.config) {
            partitions.push(PartitionSummary {
                provider: snap.provider,
                tier: snap.tier,
                window_type: snap.window_type,
                sample_count_window: snap.sample_count_window,
                bucket_api_equivalent_usd: snap.bucket_api_equivalent_usd,
                pct_per_usd: snap.pct_per_usd,
                token_mix: snap.token_mix,
                stale: snap.stale,
                computed_at: snap.computed_at,
            });
        }
    }

    partitions.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.tier.cmp(&b.tier))
            .then(a.window_type.cmp(&b.window_type))
    });

    Ok(Json(PartitionsResponse { partitions }))
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub window_type: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistorySnapshotPoint {
    pub tier: String,
    pub window_type: String,
    pub computed_at: i64,
    pub sample_count_window: usize,
    pub pct_per_usd: f64,
    pub bucket_api_equivalent_usd: f64,
    pub token_mix: TokenMix,
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

    let all_keys = state.repo.list_active_partitions().await?;
    let matching_keys: Vec<PartitionKey> = all_keys
        .into_iter()
        .filter(|k| {
            k.provider == provider_norm
                && tier_filter.is_none_or(|t| k.tier == t)
                && wt_filter.is_none_or(|w| k.window_type == w)
        })
        .collect();

    let now = Utc::now().timestamp();
    let mut points = Vec::new();
    for key in matching_keys {
        let mut intervals = state
            .repo
            .fetch_window_intervals(&key, 0, DEFAULT_WINDOW_LIMIT)
            .await?;
        if intervals.is_empty() {
            continue;
        }
        intervals.sort_by_key(|iv| (iv.ended_at, iv.id));
        let snaps = compute_partition_history(&key, &intervals, now, &state.config, 40);
        for s in snaps {
            points.push(HistorySnapshotPoint {
                tier: s.tier,
                window_type: s.window_type,
                computed_at: s.computed_at,
                sample_count_window: s.sample_count_window,
                pct_per_usd: s.pct_per_usd,
                bucket_api_equivalent_usd: s.bucket_api_equivalent_usd,
                token_mix: s.token_mix,
            });
        }
    }

    points.sort_by_key(|p| p.computed_at);
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

    let all_keys = state.repo.list_active_partitions().await?;
    let now = Utc::now().timestamp();
    let mut windows = Vec::new();

    for key in all_keys {
        if key.provider != provider || key.tier != tier {
            continue;
        }
        if let Some(ref wt) = filter_window
            && &key.window_type != wt
        {
            continue;
        }
        let mut intervals = state
            .repo
            .fetch_window_intervals(&key, 0, DEFAULT_WINDOW_LIMIT)
            .await?;
        intervals.sort_by_key(|iv| (iv.ended_at, iv.id));
        let Some(snap) = compute_partition_estimate(&key, &intervals, now, &state.config) else {
            continue;
        };
        if snap.stale && !query.include_stale {
            continue;
        }
        let active_models: Vec<ModelWeightEstimate> = snap
            .models
            .into_iter()
            .filter(|m| m.status == ModelStatus::Active || query.include_stale)
            .collect();

        windows.push(WindowEstimateView {
            window_type: snap.window_type,
            computed_at: snap.computed_at,
            sample_count_window: snap.sample_count_window,
            stale: snap.stale,
            pct_per_usd: snap.pct_per_usd,
            bucket_api_equivalent_usd: snap.bucket_api_equivalent_usd,
            pct_per_usd_p25: snap.pct_per_usd_p25,
            pct_per_usd_p75: snap.pct_per_usd_p75,
            total_api_usd: snap.total_api_usd,
            total_delta_pct: snap.total_delta_pct,
            token_mix: snap.token_mix,
            models: active_models,
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
        HeaderValue::from_static("no-cache"),
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
    pub expected_api_usd: f64,
    pub ci_95_lower: f64,
    pub ci_95_upper: f64,
    pub pct_per_usd: f64,
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

    let all_keys = state.repo.list_active_partitions().await?;
    let mut keys: Vec<PartitionKey> = all_keys
        .into_iter()
        .filter(|k| {
            k.provider == provider
                && k.tier == tier
                && target_wt.as_ref().is_none_or(|wt| &k.window_type == wt)
        })
        .collect();
    keys.sort_by(|a, b| a.window_type.cmp(&b.window_type));

    let key = keys.into_iter().next().ok_or_else(|| {
        AppError::NotFound(format!("no intervals available for {provider}/{tier}"))
    })?;

    let mut intervals = state
        .repo
        .fetch_window_intervals(&key, 0, DEFAULT_WINDOW_LIMIT)
        .await?;
    intervals.sort_by_key(|iv| (iv.ended_at, iv.id));
    let now = Utc::now().timestamp();
    let snap = compute_partition_estimate(&key, &intervals, now, &state.config).ok_or_else(|| {
        AppError::NotFound(format!("insufficient data to predict for {provider}/{tier}"))
    })?;

    let pred = predict_usage(&snap, &req.usage);

    Ok(Json(PredictResponse {
        provider,
        tier,
        window_type: key.window_type,
        expected_delta_pct: pred.expected_delta_pct,
        expected_api_usd: pred.expected_api_usd,
        ci_95_lower: pred.ci_95_lower,
        ci_95_upper: pred.ci_95_upper,
        pct_per_usd: pred.pct_per_usd,
    }))
}
