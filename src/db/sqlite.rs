use crate::db::{NewInterval, TelemetryRepository};
use crate::error::AppError;
use crate::models::{
    IntervalRecord, ModelUsageSummary, PartitionKey, QuotaChangeEvent, QuotaChangeType,
    QuotaSnapshotRow, UsageEntry,
};
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

    async fn fetch_quota_changes(
        &self,
        bucket_id: Option<&str>,
        include_anomalies: bool,
        limit: usize,
    ) -> Result<Vec<QuotaChangeEvent>, AppError> {
        let rows = sqlx::query(
            r#"
            WITH ordered AS (
                SELECT
                    id,
                    tier,
                    bucket_id,
                    recorded_at,
                    remaining_pct,
                    reset_time,
                    LAG(remaining_pct) OVER (PARTITION BY tier, bucket_id ORDER BY recorded_at, id) AS prev_pct,
                    LAG(reset_time) OVER (PARTITION BY tier, bucket_id ORDER BY recorded_at, id) AS prev_reset
                FROM quota_snapshots
                WHERE (?1 IS NULL OR bucket_id = ?1)
            ),
            classified AS (
                SELECT
                    id,
                    tier,
                    bucket_id,
                    recorded_at,
                    prev_pct,
                    remaining_pct AS curr_pct,
                    CASE WHEN prev_pct IS NULL THEN NULL ELSE remaining_pct - prev_pct END AS delta_pct,
                    reset_time,
                    CASE
                        WHEN prev_pct IS NULL THEN 'init'
                        WHEN reset_time IS NOT prev_reset THEN 'reset'
                        WHEN remaining_pct < prev_pct - 1e-6 THEN 'decrease'
                        WHEN remaining_pct > prev_pct + 1e-6 THEN 'anomaly_increase'
                        ELSE 'unchanged'
                    END AS event_type
                FROM ordered
            )
            SELECT id, tier, bucket_id, recorded_at, prev_pct, curr_pct, delta_pct, reset_time, event_type
            FROM classified
            WHERE event_type != 'unchanged'
              AND (?2 = 1 OR event_type != 'anomaly_increase')
            ORDER BY recorded_at DESC, id DESC
            LIMIT ?3
            "#,
        )
        .bind(bucket_id)
        .bind(if include_anomalies { 1_i64 } else { 0_i64 })
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut out: Vec<QuotaChangeEvent> = rows
            .into_iter()
            .filter_map(|r| {
                let et_str: String = r.get("event_type");
                let event_type = match et_str.as_str() {
                    "init" => QuotaChangeType::Init,
                    "reset" => QuotaChangeType::Reset,
                    "decrease" => QuotaChangeType::Decrease,
                    "anomaly_increase" => QuotaChangeType::AnomalyIncrease,
                    _ => return None,
                };
                Some(QuotaChangeEvent {
                    id: r.get("id"),
                    tier: r.get("tier"),
                    bucket_id: r.get("bucket_id"),
                    recorded_at: r.get("recorded_at"),
                    prev_pct: r.get("prev_pct"),
                    curr_pct: r.get("curr_pct"),
                    delta_pct: r.get("delta_pct"),
                    reset_time: r.get("reset_time"),
                    event_type,
                })
            })
            .collect();
        out.reverse();
        Ok(out)
    }

    async fn fetch_usage_by_model(
        &self,
        tier: Option<&str>,
        window_type: Option<&str>,
        since_ts: i64,
    ) -> Result<Vec<ModelUsageSummary>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT
                i.tier,
                i.window_type,
                LOWER(TRIM(json_extract(u.value, '$.model'))) AS model,
                CAST(SUM(COALESCE(json_extract(u.value, '$.tokens_in'), 0)) AS INTEGER) AS tokens_in,
                CAST(SUM(COALESCE(json_extract(u.value, '$.tokens_out'), 0)) AS INTEGER) AS tokens_out,
                CAST(SUM(COALESCE(json_extract(u.value, '$.tokens_cached_in'), 0)) AS INTEGER) AS tokens_cached_in,
                CAST(SUM(COALESCE(json_extract(u.value, '$.request_count'), 1)) AS INTEGER) AS request_count,
                COUNT(DISTINCT i.id) AS interval_count
            FROM intervals i, json_each(i.usage_json) u
            WHERE i.ingested_at >= ?1
              AND (?2 IS NULL OR i.tier = ?2)
              AND (?3 IS NULL OR i.window_type = ?3)
            GROUP BY i.tier, i.window_type, model
            ORDER BY i.tier, i.window_type, model
            "#,
        )
        .bind(since_ts)
        .bind(tier)
        .bind(window_type)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| ModelUsageSummary {
                tier: r.get("tier"),
                window_type: r.get("window_type"),
                model: r.get("model"),
                tokens_in: r.get::<i64, _>("tokens_in").max(0) as u64,
                tokens_out: r.get::<i64, _>("tokens_out").max(0) as u64,
                tokens_cached_in: r.get::<i64, _>("tokens_cached_in").max(0) as u64,
                request_count: r.get::<i64, _>("request_count").max(0) as u64,
                interval_count: r.get::<i64, _>("interval_count").max(0) as usize,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_quota_changes_classification_and_usage_summary() {
        let repo = SqliteRepository::connect_and_migrate("sqlite::memory:")
            .await
            .unwrap();

        // Init -> Decrease -> AnomalyIncrease (same reset_time) -> Reset (new reset_time)
        repo.insert_quota_snapshot("pro", "gemini-5h", 95.0, Some("2026-11-25T15:00:00Z"), 100)
            .await
            .unwrap();
        repo.insert_quota_snapshot("pro", "gemini-5h", 90.0, Some("2026-11-25T15:00:00Z"), 200)
            .await
            .unwrap();
        repo.insert_quota_snapshot("pro", "gemini-5h", 92.0, Some("2026-11-25T15:00:00Z"), 202)
            .await
            .unwrap();
        repo.insert_quota_snapshot("pro", "gemini-5h", 99.8, Some("2026-11-25T20:00:00Z"), 300)
            .await
            .unwrap();

        let clean = repo
            .fetch_quota_changes(Some("gemini-5h"), false, 50)
            .await
            .unwrap();
        assert_eq!(clean.len(), 3);
        assert_eq!(clean[0].event_type, QuotaChangeType::Init);
        assert_eq!(clean[1].event_type, QuotaChangeType::Decrease);
        assert_eq!(clean[2].event_type, QuotaChangeType::Reset);

        let all = repo
            .fetch_quota_changes(Some("gemini-5h"), true, 50)
            .await
            .unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[2].event_type, QuotaChangeType::AnomalyIncrease);

        repo.insert_interval_if_new(&NewInterval {
            idempotency_key: "k1".to_string(),
            tier: "pro".to_string(),
            window_type: "gemini-5h".to_string(),
            started_at: 100,
            ended_at: 200,
            delta_pct: 5.0,
            quota_precision: 0.01,
            usage: vec![UsageEntry {
                model: "Gemini 4 Hydrogen (Medium)".to_string(),
                tokens_in: 100_000,
                tokens_out: 20_000,
                tokens_cached_in: 500_000,
                request_count: 3,
            }],
            ingested_at: 200,
        })
        .await
        .unwrap();

        let usage = repo
            .fetch_usage_by_model(Some("pro"), Some("gemini-5h"), 0)
            .await
            .unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].model, "gemini 4 hydrogen (medium)");
        assert_eq!(usage[0].tokens_in, 100_000);
        assert_eq!(usage[0].tokens_out, 20_000);
        assert_eq!(usage[0].tokens_cached_in, 500_000);
        assert_eq!(usage[0].request_count, 3);
        assert_eq!(usage[0].interval_count, 1);
    }
}
