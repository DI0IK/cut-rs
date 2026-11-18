use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub state_file_path: PathBuf,
    pub bucket_stale_days: i64,
    pub model_stale_days: i64,
}

pub fn default_state_dir() -> PathBuf {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cut-rs")
}

pub fn default_db_path() -> PathBuf {
    default_state_dir().join("cut-rs.db")
}

pub fn default_database_url() -> String {
    let path = default_db_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let path_str = path.to_string_lossy().replace('\\', "/");
    format!("sqlite://{path_str}?mode=rwc")
}

impl Config {
    pub fn from_env() -> Self {
        let host = env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| default_database_url());
        let state_file_path = env::var("CUT_RS_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_state_dir().join("agy-cli-state.json"));

        let bucket_stale_days = env::var("BUCKET_STALE_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(14);

        let model_stale_days = env::var("MODEL_STALE_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(14);

        Self {
            host,
            port,
            database_url,
            state_file_path,
            bucket_stale_days,
            model_stale_days,
        }
    }

    pub fn test_default(database_url: String, state_file_path: PathBuf) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 0,
            database_url,
            state_file_path,
            bucket_stale_days: 14,
            model_stale_days: 14,
        }
    }
}
