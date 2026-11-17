pub mod sqlite;

use crate::error::AppError;
use crate::models::{ClientRecord, IntervalRecord, PartitionKey, SnapshotPayload, TrustTier, UsageEntry};
use async_trait::async_trait;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct NewInterval {
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
    pub ingested_at: i64,
}

#[async_trait]
pub trait TelemetryRepository: Send + Sync {
    async fn insert_client(&self, client: &ClientRecord) -> Result<(), AppError>;
    async fn get_client(&self, client_id: &str) -> Result<Option<ClientRecord>, AppError>;
    async fn get_clients_by_ids(
        &self,
        client_ids: &[String],
    ) -> Result<HashMap<String, ClientRecord>, AppError>;

    /// Inserts an interval if `idempotency_key` is new. Returns `true` if inserted, `false` if duplicate.
    /// Also increments `accepted_count` and updates `last_seen_at` on the client when inserted.
    async fn insert_interval_if_new(&self, interval: &NewInterval) -> Result<bool, AppError>;

    async fn list_active_partitions(&self) -> Result<Vec<PartitionKey>, AppError>;

    async fn fetch_window_intervals(
        &self,
        key: &PartitionKey,
        since_ts: i64,
        limit: usize,
    ) -> Result<Vec<IntervalRecord>, AppError>;

    async fn update_interval_fits(&self, updates: &[(i64, f64, f64)]) -> Result<(), AppError>;

    async fn update_client_trust(
        &self,
        client_id: &str,
        trust_score: f64,
        trust_tier: TrustTier,
        ema_abs_z: f64,
    ) -> Result<(), AppError>;

    async fn save_snapshot(&self, snapshot: &SnapshotPayload) -> Result<(), AppError>;

    async fn load_latest_snapshots(&self) -> Result<Vec<SnapshotPayload>, AppError>;

    async fn load_snapshot_history(
        &self,
        provider: &str,
        tier: Option<&str>,
        window_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SnapshotPayload>, AppError>;
}
