use rand::RngCore;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub hmac_secret: Vec<u8>,
    pub trust_proxy_headers: bool,
    pub max_body_bytes: usize,
    pub pow_base_difficulty: u8,
    pub pow_max_difficulty: u8,
    pub pow_ttl_secs: i64,
    pub worker_interval_secs: u64,
    pub bucket_stale_days: i64,
    pub model_stale_days: i64,
    pub rate_limit_rpm: u32,
}

impl Config {
    pub fn from_env() -> Self {
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let database_url = env::var("DATABASE_URL")
            .unwrap_or_else(|_| "sqlite://cut_rs.db?mode=rwc".to_string());

        let hmac_secret = match env::var("HMAC_SECRET") {
            Ok(secret) if !secret.trim().is_empty() => secret.into_bytes(),
            _ => {
                tracing::warn!(
                    "HMAC_SECRET not set; generating ephemeral random key (tokens will not survive restarts)"
                );
                let mut key = vec![0u8; 32];
                rand::rng().fill_bytes(&mut key);
                key
            }
        };

        let trust_proxy_headers = env::var("TRUST_PROXY_HEADERS")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes"))
            .unwrap_or(false);

        let max_body_bytes = env::var("MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_048_576);

        let pow_base_difficulty = env::var("POW_BASE_DIFFICULTY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);

        let pow_max_difficulty = env::var("POW_MAX_DIFFICULTY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(26);

        let pow_ttl_secs = env::var("POW_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

        let worker_interval_secs = env::var("WORKER_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);

        let bucket_stale_days = env::var("BUCKET_STALE_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(14);

        let model_stale_days = env::var("MODEL_STALE_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(14);

        let rate_limit_rpm = env::var("RATE_LIMIT_RPM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

        Self {
            host,
            port,
            database_url,
            hmac_secret,
            trust_proxy_headers,
            max_body_bytes,
            pow_base_difficulty,
            pow_max_difficulty,
            pow_ttl_secs,
            worker_interval_secs,
            bucket_stale_days,
            model_stale_days,
            rate_limit_rpm,
        }
    }

    pub fn test_default(database_url: String) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 0,
            database_url,
            hmac_secret: b"test-secret-key-for-cut-rs-unit-and-integration-tests".to_vec(),
            trust_proxy_headers: false,
            max_body_bytes: 1_048_576,
            pow_base_difficulty: 8,
            pow_max_difficulty: 16,
            pow_ttl_secs: 120,
            worker_interval_secs: 60,
            bucket_stale_days: 14,
            model_stale_days: 14,
            rate_limit_rpm: 1000,
        }
    }
}
