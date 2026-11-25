pub mod sqlite;

use crate::error::AppError;
use crate::models::{
    IntervalRecord, ModelUsageSummary, PartitionKey, QuotaChangeEvent, QuotaSnapshotRow,
    UsageEntry,
};
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct NewInterval {
    pub idempotency_key: String,
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
    /// Inserts an interval if `idempotency_key` is new. Returns `true` if inserted, `false` if duplicate.
    async fn insert_interval_if_new(&self, interval: &NewInterval) -> Result<bool, AppError>;

    async fn list_active_partitions(&self) -> Result<Vec<PartitionKey>, AppError>;

    async fn fetch_window_intervals(
        &self,
        key: &PartitionKey,
        since_ts: i64,
        limit: usize,
    ) -> Result<Vec<IntervalRecord>, AppError>;

    async fn count_intervals(&self) -> Result<i64, AppError>;

    async fn insert_quota_snapshot(
        &self,
        tier: &str,
        bucket_id: &str,
        remaining_pct: f64,
        reset_time: Option<&str>,
        recorded_at: i64,
    ) -> Result<(), AppError>;

    async fn fetch_quota_history(
        &self,
        bucket_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<QuotaSnapshotRow>, AppError>;

    async fn fetch_quota_changes(
        &self,
        bucket_id: Option<&str>,
        include_anomalies: bool,
        limit: usize,
    ) -> Result<Vec<QuotaChangeEvent>, AppError>;

    async fn fetch_usage_by_model(
        &self,
        tier: Option<&str>,
        window_type: Option<&str>,
        since_ts: i64,
    ) -> Result<Vec<ModelUsageSummary>, AppError>;
}
