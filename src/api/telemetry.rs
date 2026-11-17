use crate::api::AppState;
use crate::api::auth::AuthenticatedClient;
use crate::db::NewInterval;
use crate::error::AppError;
use crate::models::{IntervalSubmission, normalize_key};
use axum::Json;
use axum::extract::State;
use chrono::Utc;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TelemetryBatchRequest {
    Wrapped {
        intervals: Vec<IntervalSubmission>,
    },
    Bare(Vec<IntervalSubmission>),
}

impl TelemetryBatchRequest {
    fn into_vec(self) -> Vec<IntervalSubmission> {
        match self {
            TelemetryBatchRequest::Wrapped { intervals } => intervals,
            TelemetryBatchRequest::Bare(intervals) => intervals,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Accepted,
    Duplicate,
    Rejected,
}

#[derive(Debug, Serialize)]
pub struct IntervalItemResult {
    pub idempotency_key: String,
    pub status: ItemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TelemetryBatchResponse {
    pub accepted: usize,
    pub duplicate: usize,
    pub rejected: usize,
    pub results: Vec<IntervalItemResult>,
}

pub async fn ingest_intervals(
    State(state): State<AppState>,
    AuthenticatedClient(client): AuthenticatedClient,
    Json(req): Json<TelemetryBatchRequest>,
) -> Result<Json<TelemetryBatchResponse>, AppError> {
    let items = req.into_vec();
    if items.len() > 200 {
        return Err(AppError::BadRequest(
            "batch size exceeds maximum of 200 intervals".to_string(),
        ));
    }

    let now = Utc::now().timestamp();
    let mut accepted = 0usize;
    let mut duplicate = 0usize;
    let mut rejected = 0usize;
    let mut results = Vec::with_capacity(items.len());

    for item in items {
        if let Err(reason) = validate_interval(&item) {
            rejected += 1;
            results.push(IntervalItemResult {
                idempotency_key: item.idempotency_key,
                status: ItemStatus::Rejected,
                reason: Some(reason),
            });
            continue;
        }

        let delta_pct = item.quota_before_pct - item.quota_after_pct;
        let new_iv = NewInterval {
            idempotency_key: item.idempotency_key.clone(),
            client_id: client.client_id.clone(),
            provider: normalize_key(&item.provider),
            tier: normalize_key(&item.tier),
            window_type: normalize_key(&item.window_type),
            started_at: item.started_at,
            ended_at: item.ended_at,
            delta_pct,
            quota_precision: item.quota_precision,
            usage: item.usage,
            ingested_at: now,
        };

        if state.repo.insert_interval_if_new(&new_iv).await? {
            accepted += 1;
            results.push(IntervalItemResult {
                idempotency_key: item.idempotency_key,
                status: ItemStatus::Accepted,
                reason: None,
            });
        } else {
            duplicate += 1;
            results.push(IntervalItemResult {
                idempotency_key: item.idempotency_key,
                status: ItemStatus::Duplicate,
                reason: None,
            });
        }
    }

    if accepted > 0 {
        state.worker_notify.notify_one();
    }

    Ok(Json(TelemetryBatchResponse {
        accepted,
        duplicate,
        rejected,
        results,
    }))
}

fn validate_interval(item: &IntervalSubmission) -> Result<(), String> {
    if item.idempotency_key.trim().is_empty() {
        return Err("missing idempotency_key".to_string());
    }
    if item.provider.trim().is_empty()
        || item.tier.trim().is_empty()
        || item.window_type.trim().is_empty()
    {
        return Err("provider, tier, and window_type must be non-empty".to_string());
    }
    if !item.quota_before_pct.is_finite()
        || !item.quota_after_pct.is_finite()
        || !item.quota_precision.is_finite()
    {
        return Err("non-finite quota values".to_string());
    }
    if item.quota_precision <= 0.0 || item.quota_precision > 10.0 {
        return Err("invalid quota_precision".to_string());
    }
    let delta_pct = item.quota_before_pct - item.quota_after_pct;
    if delta_pct <= 0.0 || delta_pct > 100.0 {
        return Err(format!("invalid delta_pct={delta_pct:.4} (must be in (0, 100])"));
    }
    let duration = item.ended_at - item.started_at;
    if duration <= 0 || duration > 1800 {
        return Err(format!(
            "invalid interval duration={duration}s (must be in (0, 1800])"
        ));
    }
    if item.usage.is_empty() {
        return Err("usage array must not be empty".to_string());
    }
    let total_tokens: u64 = item
        .usage
        .iter()
        .map(|u| {
            u.tokens_in
                .saturating_add(u.tokens_out)
                .saturating_add(u.tokens_cached_in)
        })
        .sum();
    if total_tokens == 0 {
        return Err("usage must contain > 0 tokens".to_string());
    }
    if item.usage.iter().any(|u| u.model.trim().is_empty()) {
        return Err("usage entry missing model name".to_string());
    }
    Ok(())
}
