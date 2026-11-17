use crate::db::{NewInterval, TelemetryRepository};
use crate::error::AppError;
use crate::models::{ClientRecord, IntervalRecord, PartitionKey, SnapshotPayload, TrustTier, UsageEntry};
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
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
            .foreign_keys(true)
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
    async fn insert_client(&self, client: &ClientRecord) -> Result<(), AppError> {
        sqlx::query(
            r#"
            INSERT INTO clients (
                client_id, created_at, subnet_hash, trust_score,
                trust_tier, accepted_count, ema_abs_z, last_seen_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
        )
        .bind(&client.client_id)
        .bind(client.created_at)
        .bind(&client.subnet_hash)
        .bind(client.trust_score)
        .bind(client.trust_tier.as_str())
        .bind(client.accepted_count)
        .bind(client.ema_abs_z)
        .bind(client.last_seen_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get_client(&self, client_id: &str) -> Result<Option<ClientRecord>, AppError> {
        let row = sqlx::query(
            r#"
            SELECT client_id, created_at, subnet_hash, trust_score,
                   trust_tier, accepted_count, ema_abs_z, last_seen_at
            FROM clients
            WHERE client_id = ?1
            "#,
        )
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| ClientRecord {
            client_id: r.get("client_id"),
            created_at: r.get("created_at"),
            subnet_hash: r.get("subnet_hash"),
            trust_score: r.get("trust_score"),
            trust_tier: TrustTier::from_db(r.get("trust_tier")),
            accepted_count: r.get("accepted_count"),
            ema_abs_z: r.get("ema_abs_z"),
            last_seen_at: r.get("last_seen_at"),
        }))
    }

    async fn get_clients_by_ids(
        &self,
        client_ids: &[String],
    ) -> Result<HashMap<String, ClientRecord>, AppError> {
        let mut map = HashMap::new();
        if client_ids.is_empty() {
            return Ok(map);
        }

        // Chunk to stay well within SQLite bind parameter limits (999)
        for chunk in client_ids.chunks(500) {
            let placeholders: Vec<String> =
                (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT client_id, created_at, subnet_hash, trust_score, \
                        trust_tier, accepted_count, ema_abs_z, last_seen_at \
                 FROM clients WHERE client_id IN ({})",
                placeholders.join(",")
            );
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for r in rows {
                let rec = ClientRecord {
                    client_id: r.get("client_id"),
                    created_at: r.get("created_at"),
                    subnet_hash: r.get("subnet_hash"),
                    trust_score: r.get("trust_score"),
                    trust_tier: TrustTier::from_db(r.get("trust_tier")),
                    accepted_count: r.get("accepted_count"),
                    ema_abs_z: r.get("ema_abs_z"),
                    last_seen_at: r.get("last_seen_at"),
                };
                map.insert(rec.client_id.clone(), rec);
            }
        }
        Ok(map)
    }

    async fn insert_interval_if_new(&self, interval: &NewInterval) -> Result<bool, AppError> {
        let usage_json = serde_json::to_string(&interval.usage)
            .map_err(|e| AppError::Internal(e.into()))?;

        let mut tx = self.pool.begin().await?;
        let res = sqlx::query(
            r#"
            INSERT OR IGNORE INTO intervals (
                idempotency_key, client_id, provider, tier, window_type,
                started_at, ended_at, delta_pct, quota_precision, usage_json, ingested_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
        )
        .bind(&interval.idempotency_key)
        .bind(&interval.client_id)
        .bind(&interval.provider)
        .bind(&interval.tier)
        .bind(&interval.window_type)
        .bind(interval.started_at)
        .bind(interval.ended_at)
        .bind(interval.delta_pct)
        .bind(interval.quota_precision)
        .bind(&usage_json)
        .bind(interval.ingested_at)
        .execute(&mut *tx)
        .await?;

        let inserted = res.rows_affected() > 0;
        if inserted {
            sqlx::query(
                r#"
                UPDATE clients
                SET accepted_count = accepted_count + 1,
                    last_seen_at = MAX(last_seen_at, ?2)
                WHERE client_id = ?1
                "#,
            )
            .bind(&interval.client_id)
            .bind(interval.ingested_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    async fn list_active_partitions(&self) -> Result<Vec<PartitionKey>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT provider, tier, window_type
            FROM intervals
            ORDER BY provider, tier, window_type
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| PartitionKey {
                provider: r.get("provider"),
                tier: r.get("tier"),
                window_type: r.get("window_type"),
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
            SELECT id, idempotency_key, client_id, provider, tier, window_type,
                   started_at, ended_at, delta_pct, quota_precision, usage_json,
                   residual_pct, weight_assigned, ingested_at
            FROM intervals
            WHERE provider = ?1 AND tier = ?2 AND window_type = ?3
              AND ingested_at >= ?4
            ORDER BY ingested_at DESC, id DESC
            LIMIT ?5
            "#,
        )
        .bind(&key.provider)
        .bind(&key.tier)
        .bind(&key.window_type)
        .bind(since_ts)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut records = Vec::with_capacity(rows.len());
        for r in rows {
            let usage_str: String = r.get("usage_json");
            let usage: Vec<UsageEntry> = serde_json::from_str(&usage_str)
                .map_err(|e| AppError::Internal(e.into()))?;
            records.push(IntervalRecord {
                id: r.get("id"),
                idempotency_key: r.get("idempotency_key"),
                client_id: r.get("client_id"),
                provider: r.get("provider"),
                tier: r.get("tier"),
                window_type: r.get("window_type"),
                started_at: r.get("started_at"),
                ended_at: r.get("ended_at"),
                delta_pct: r.get("delta_pct"),
                quota_precision: r.get("quota_precision"),
                usage,
                residual_pct: r.get("residual_pct"),
                weight_assigned: r.get("weight_assigned"),
                ingested_at: r.get("ingested_at"),
            });
        }
        // Return in chronological order (oldest -> newest) for CUSUM & time ordering
        records.reverse();
        Ok(records)
    }

    async fn update_interval_fits(&self, updates: &[(i64, f64, f64)]) -> Result<(), AppError> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for &(id, residual_pct, weight_assigned) in updates {
            sqlx::query(
                r#"
                UPDATE intervals
                SET residual_pct = ?2, weight_assigned = ?3
                WHERE id = ?1
                "#,
            )
            .bind(id)
            .bind(residual_pct)
            .bind(weight_assigned)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn update_client_trust(
        &self,
        client_id: &str,
        trust_score: f64,
        trust_tier: TrustTier,
        ema_abs_z: f64,
    ) -> Result<(), AppError> {
        sqlx::query(
            r#"
            UPDATE clients
            SET trust_score = ?2, trust_tier = ?3, ema_abs_z = ?4
            WHERE client_id = ?1
            "#,
        )
        .bind(client_id)
        .bind(trust_score)
        .bind(trust_tier.as_str())
        .bind(ema_abs_z)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn save_snapshot(&self, snapshot: &SnapshotPayload) -> Result<(), AppError> {
        let weights_json =
            serde_json::to_string(snapshot).map_err(|e| AppError::Internal(e.into()))?;
        sqlx::query(
            r#"
            INSERT INTO model_snapshots (
                provider, tier, window_type, computed_at, regime_since,
                sample_count, effective_contributors, weights_json, cusum_pos, cusum_neg
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
        )
        .bind(&snapshot.provider)
        .bind(&snapshot.tier)
        .bind(&snapshot.window_type)
        .bind(snapshot.computed_at)
        .bind(snapshot.regime_since)
        .bind(snapshot.sample_count_window as i64)
        .bind(snapshot.effective_contributors)
        .bind(&weights_json)
        .bind(snapshot.cusum_pos)
        .bind(snapshot.cusum_neg)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn load_latest_snapshots(&self) -> Result<Vec<SnapshotPayload>, AppError> {
        let rows = sqlx::query(
            r#"
            SELECT ms.weights_json
            FROM model_snapshots ms
            INNER JOIN (
                SELECT provider, tier, window_type, MAX(id) AS max_id
                FROM model_snapshots
                GROUP BY provider, tier, window_type
            ) latest
            ON ms.id = latest.max_id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let json_str: String = r.get("weights_json");
            let snap: SnapshotPayload = serde_json::from_str(&json_str)
                .map_err(|e| AppError::Internal(e.into()))?;
            out.push(snap);
        }
        Ok(out)
    }
}
