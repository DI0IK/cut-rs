use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Probation,
    Trusted,
}

impl TrustTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustTier::Probation => "probation",
            TrustTier::Trusted => "trusted",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "trusted" => TrustTier::Trusted,
            _ => TrustTier::Probation,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRecord {
    pub client_id: String,
    pub created_at: i64,
    pub subnet_hash: String,
    pub trust_score: f64,
    pub trust_tier: TrustTier,
    pub accepted_count: i64,
    pub ema_abs_z: f64,
    pub last_seen_at: i64,
}

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
    pub client_id: String,
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub delta_pct: f64,
    pub quota_precision: f64,
    pub usage: Vec<UsageEntry>,
    pub residual_pct: Option<f64>,
    pub weight_assigned: Option<f64>,
    pub ingested_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identifiability {
    High,
    Moderate,
    LowCollinear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    Active,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceInterval {
    pub lower: f64,
    pub upper: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelWeightEstimate {
    pub model: String,
    pub pct_per_1m_in: f64,
    pub pct_per_1m_in_ci: ConfidenceInterval,
    pub pct_per_1m_out: f64,
    pub pct_per_1m_out_ci: ConfidenceInterval,
    pub pct_per_1m_cached_in: f64,
    pub pct_per_1m_cached_in_ci: ConfidenceInterval,
    pub identifiability: Identifiability,
    pub sample_count: usize,
    pub last_seen_at: i64,
    pub status: ModelStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeEventType {
    RegimeShift,
    ModelAdded,
    ModelRetired,
    BucketAdded,
    BucketRetired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub timestamp: i64,
    pub event_type: ChangeEventType,
    pub description: String,
    pub cusum_value: Option<f64>,
    pub corroborating_clients: Option<usize>,
    pub corroborating_subnets: Option<usize>,
    pub p_value: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenMix {
    pub in_share: f64,
    pub out_share: f64,
    pub cached_share: f64,
}

impl Default for TokenMix {
    fn default() -> Self {
        Self {
            in_share: 0.05,
            out_share: 0.05,
            cached_share: 0.90,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub provider: String,
    pub tier: String,
    pub window_type: String,
    pub computed_at: i64,
    pub regime_since: i64,
    pub sample_count_window: usize,
    pub effective_contributors: f64,
    pub sigma_hat: f64,
    pub stale: bool,
    #[serde(default)]
    pub token_mix: TokenMix,
    pub models: Vec<ModelWeightEstimate>,
    pub feature_names: Vec<String>,
    pub covariance_matrix: Vec<f64>,
    pub recent_change_events: Vec<ChangeEvent>,
    pub cusum_pos: f64,
    pub cusum_neg: f64,
}
