use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartitionKey {
    pub provider: String,
    pub tier: String,
    pub window_type: String,
}

impl PartitionKey {
    pub fn new(provider: &str, tier: &str, window_type: &str) -> Self {
        Self {
            provider: normalize_key(provider),
            tier: normalize_key(tier),
            window_type: normalize_key(window_type),
        }
    }
}

pub fn normalize_key(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEntry {
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    #[serde(default)]
    pub tokens_cached_in: u64,
    #[serde(default = "default_request_count")]
    pub request_count: u32,
}

fn default_request_count() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalSubmission {
    pub idempotency_key: String,
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub quota_before_pct: f64,
    pub quota_after_pct: f64,
    #[serde(default = "default_quota_precision")]
    pub quota_precision: f64,
    pub usage: Vec<UsageEntry>,
}

fn default_quota_precision() -> f64 {
    0.01
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalRecord {
    pub id: i64,
    pub idempotency_key: String,
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub delta_pct: f64,
    pub quota_precision: f64,
    pub usage: Vec<UsageEntry>,
    pub ingested_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaSnapshotRow {
    pub id: i64,
    pub tier: String,
    pub bucket_id: String,
    pub remaining_pct: f64,
    pub reset_time: Option<String>,
    pub recorded_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaChangeType {
    Init,
    Reset,
    Decrease,
    AnomalyIncrease,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaChangeEvent {
    pub id: i64,
    pub tier: String,
    pub bucket_id: String,
    pub recorded_at: i64,
    pub prev_pct: Option<f64>,
    pub curr_pct: f64,
    pub delta_pct: Option<f64>,
    pub reset_time: Option<String>,
    pub event_type: QuotaChangeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelUsageSummary {
    pub tier: String,
    pub window_type: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached_in: u64,
    pub request_count: u64,
    pub interval_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    Active,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelWeightEstimate {
    pub model: String,
    pub priced: bool,
    pub usd_per_1m_in: f64,
    pub usd_per_1m_out: f64,
    pub usd_per_1m_cached_in: f64,
    pub pct_per_1m_in: f64,
    pub pct_per_1m_out: f64,
    pub pct_per_1m_cached_in: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_tokens_cached_in: u64,
    pub total_requests: u64,
    pub total_api_usd: f64,
    pub sample_count: usize,
    pub last_seen_at: i64,
    pub status: ModelStatus,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenMix {
    pub in_share: f64,
    pub out_share: f64,
    pub cached_share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub provider: String,
    pub tier: String,
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
