use crate::db::{NewInterval, TelemetryRepository};
use crate::error::AppError;
use crate::models::{IntervalRecord, PartitionKey, QuotaSnapshotRow, UsageEntry};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::str::FromStr;
use std::time::Duration;

#[derive(Clone)]
pub struct SqliteRepository {
    pool: SqlitePool,
}

impl SqliteRepository {
    pub async fn connect_and_migrate(database_url: &str) -> Result<Self, AppError> {
        let options = SqliteConnectOptions::from_str(database_url)
            .map_err(|e| AppError::Internal(e.into()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl TelemetryRepository for SqliteRepository {
    async fn insert_interval_if_new(&self, interval: &NewInterval) -> Result<bool, AppError> {
        let usage_json = serde_json::to_string(&interval.usage)
            .map_err(|e| AppError::Internal(e.into()))?;

        let res = sqlx::query(
            r#"
            INSERT OR IGNORE INTO intervals (
                idempotency_key, tier, window_type,
                started_at, ended_at, delta_pct, quota_precision, usage_json, ingested_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
        )
        .bind(&interval.idempotency_key)
        .bind(&interval.tier)
        .bind(&interval.window_type)
        .bind(interval.started_at)
        .bind(interval.ended_at)
        .bind(interval.delta_pct)
        .bind(interval.quota_precision)
        .bind(&usage_json)
        .bind(interval.ingested_at)
        .execute(&self.pool)
        .await?;

        Ok(res.rows_affected() > 0)
    }

    async fn list_active_partitions(&self) -> Result<Vec<PartitionKey>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT tier, window_type
            FROM intervals
            ORDER BY tier, window_type
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| {
                PartitionKey::new(
                    "antigravity",
                    r.get::<String, _>("tier").as_str(),
                    r.get::<String, _>("window_type").as_str(),
                )
            })
            .collect())
    }

    async fn fetch_window_intervals(
        &self,
        key: &PartitionKey,
        since_ts: i64,
        limit: usize,
    ) -> Result<Vec<IntervalRecord>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT id, idempotency_key, tier, window_type,
                   started_at, ended_at, delta_pct, quota_precision, usage_json, ingested_at
            FROM intervals
            WHERE tier = ?1 AND window_type = ?2 AND ingested_at >= ?3
            ORDER BY ingested_at DESC, id DESC
            LIMIT ?4
            "#,
        )
        .bind(&key.tier)
        .bind(&key.window_type)
        .bind(since_ts)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut records = Vec::with_capacity(rows.len());
        for r in rows {
            let usage_json: String = r.get("usage_json");
            let usage: Vec<UsageEntry> = serde_json::from_str(&usage_json)
                .map_err(|e| AppError::Internal(e.into()))?;
            records.push(IntervalRecord {
                id: r.get("id"),
                idempotency_key: r.get("idempotency_key"),
                provider: "antigravity".to_string(),
                tier: r.get("tier"),
                window_type: r.get("window_type"),
                started_at: r.get("started_at"),
                ended_at: r.get("ended_at"),
                delta_pct: r.get("delta_pct"),
                quota_precision: r.get("quota_precision"),
                usage,
                ingested_at: r.get("ingested_at"),
            });
        }
        records.reverse();
        Ok(records)
    }

    async fn count_intervals(&self) -> Result<i64, AppError> {
        let row = sqlx::query("SELECT COUNT(*) AS cnt FROM intervals")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("cnt"))
    }

    async fn insert_quota_snapshot(
        &self,
        tier: &str,
        bucket_id: &str,
        remaining_pct: f64,
        reset_time: Option<&str>,
        recorded_at: i64,
    ) -> Result<(), AppError> {
        sqlx::query(
            r#"
            INSERT INTO quota_snapshots (tier, bucket_id, remaining_pct, reset_time, recorded_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
        )
        .bind(tier)
        .bind(bucket_id)
        .bind(remaining_pct)
        .bind(reset_time)
        .bind(recorded_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn fetch_quota_history(
        &self,
        bucket_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<QuotaSnapshotRow>, AppError> {
        let rows = if let Some(bid) = bucket_id {
            sqlx::query(
                r#"
                SELECT id, tier, bucket_id, remaining_pct, reset_time, recorded_at
                FROM quota_snapshots
                WHERE bucket_id = ?1
                ORDER BY recorded_at DESC, id DESC
                LIMIT ?2
                "#,
            )
            .bind(bid)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                SELECT id, tier, bucket_id, remaining_pct, reset_time, recorded_at
                FROM quota_snapshots
                ORDER BY recorded_at DESC, id DESC
                LIMIT ?1
                "#,
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?
        };

        let mut out: Vec<QuotaSnapshotRow> = rows
            .into_iter()
            .map(|r| QuotaSnapshotRow {
                id: r.get("id"),
                tier: r.get("tier"),
                bucket_id: r.get("bucket_id"),
                remaining_pct: r.get("remaining_pct"),
                reset_time: r.get("reset_time"),
                recorded_at: r.get("recorded_at"),
            })
            .collect();
        out.reverse();
        Ok(out)
    }
}
