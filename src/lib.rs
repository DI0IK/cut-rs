pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod math;
pub mod models;
pub mod worker;

use crate::api::AppState;
use crate::auth::RegistrationRateTracker;
use crate::config::Config;
use crate::db::TelemetryRepository;
use crate::db::sqlite::SqliteRepository;
use crate::models::PartitionKey;
use crate::worker::SnapshotCache;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::Notify;

pub async fn init_app_state(config: Config) -> Result<AppState, error::AppError> {
    let repo = Arc::new(SqliteRepository::connect_and_migrate(&config.database_url).await?);
    let cache: SnapshotCache = Arc::new(DashMap::new());

    let latest = repo.load_latest_snapshots().await?;
    for snap in latest {
        let key = PartitionKey::new(&snap.provider, &snap.tier, &snap.window_type);
        cache.insert(key, snap);
    }

    Ok(AppState {
        repo,
        cache,
        history_cache: Arc::new(DashMap::new()),
        config: Arc::new(config),
        rate_tracker: Arc::new(RegistrationRateTracker::new()),
        worker_notify: Arc::new(Notify::new()),
        http_rate_limiter: Arc::new(DashMap::new()),
    })
}
