pub mod api;
pub mod cli;
pub mod config;
pub mod db;
pub mod error;
pub mod estimator;
pub mod math;
pub mod models;

use crate::api::AppState;
use crate::config::Config;
use crate::db::sqlite::SqliteRepository;
use std::sync::Arc;

pub async fn init_app_state(config: Config) -> Result<AppState, error::AppError> {
    let repo = Arc::new(SqliteRepository::connect_and_migrate(&config.database_url).await?);

    Ok(AppState {
        repo,
        config: Arc::new(config),
    })
}
