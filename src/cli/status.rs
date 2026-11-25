use crate::cli::common::{default_state_file_path, load_state};
use crate::cli::webserver::get_running_webserver_url;
use crate::config::{Config, default_database_url, default_db_path};
use crate::db::TelemetryRepository;
use crate::db::sqlite::SqliteRepository;
use crate::estimator::{DEFAULT_WINDOW_LIMIT, compute_partition_estimate};
use crate::models::{QuotaChangeEvent, QuotaChangeType};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Output status as JSON
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    hook_config_path: Option<String>,
    hook_configured: bool,
    db_path: String,
    recorded_intervals: i64,
    webserver_running: bool,
    webserver_pid: Option<u32>,
    webserver_url: Option<String>,
    tier: Option<String>,
    buckets: Vec<BucketStatus>,
    recent_resets: Vec<QuotaChangeEvent>,
}

#[derive(Debug, Serialize)]
struct BucketStatus {
    bucket_id: String,
    remaining_pct: f64,
    reset_in: Option<String>,
    reset_time: Option<String>,
    bucket_api_equivalent_usd: Option<f64>,
    pct_per_usd: Option<f64>,
}

pub async fn run_status(args: StatusArgs) {
    let home = dirs::home_dir();
    let cfg_path = home.map(|h| h.join(".gemini").join("antigravity-cli").join("settings.json"));
    let hook_configured = cfg_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|raw| raw.contains("cut-rs"));

    let db_path = default_db_path();
    let mut recorded_intervals = 0;
    let mut bucket_equivs: HashMap<String, (f64, f64)> = HashMap::new();
    let mut recent_resets = Vec::new();

    if db_path.exists()
        && let Ok(repo) = SqliteRepository::connect_and_migrate(&default_database_url()).await
    {
        recorded_intervals = repo.count_intervals().await.unwrap_or(0);
        let cfg = Config::from_env();
        let now_ts = Utc::now().timestamp();
        if let Ok(keys) = repo.list_active_partitions().await {
            for key in keys {
                if let Ok(mut ivs) = repo.fetch_window_intervals(&key, 0, DEFAULT_WINDOW_LIMIT).await {
                    ivs.sort_by_key(|iv| (iv.ended_at, iv.id));
                    if let Some(snap) = compute_partition_estimate(&key, &ivs, now_ts, &cfg)
                        && snap.bucket_api_equivalent_usd > 0.0
                    {
                        bucket_equivs.insert(
                            key.window_type.clone(),
                            (snap.bucket_api_equivalent_usd, snap.pct_per_usd),
                        );
                    }
                }
            }
        }
        if let Ok(changes) = repo.fetch_quota_changes(None, false, 500).await {
            recent_resets = changes
                .into_iter()
                .filter(|c| c.event_type == QuotaChangeType::Reset)
                .rev()
                .take(10)
                .collect();
        }
    }

    let (webserver_running, webserver_pid, webserver_url) = match get_running_webserver_url() {
        Some((pid, url)) => (true, Some(pid), Some(url)),
        None => (false, None, None),
    };

    let state_path = default_state_file_path("agy-cli-state.json");
    let state_opt = load_state(&state_path);
    let now = Utc::now();

    let tier = state_opt.as_ref().and_then(|s| s.tier.clone());
    let mut buckets = Vec::new();
    if let Some(st) = state_opt {
        for (bucket_id, b) in &st.buckets {
            let reset_time = b.effective_reset_time().map(str::to_string);
            let reset_in = reset_time.as_deref().map(|r| format_countdown(r, now));
            let equiv = bucket_equivs.get(&bucket_id.to_ascii_lowercase()).copied();
            buckets.push(BucketStatus {
                bucket_id: bucket_id.clone(),
                remaining_pct: (b.last_remaining_fraction * 100.0).clamp(0.0, 100.0),
                reset_in,
                reset_time,
                bucket_api_equivalent_usd: equiv.map(|(usd, _)| usd),
                pct_per_usd: equiv.map(|(_, ppu)| ppu),
            });
        }
        buckets.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));
    }

    let report = StatusReport {
        hook_config_path: cfg_path.map(|p| p.display().to_string()),
        hook_configured,
        db_path: db_path.display().to_string(),
        recorded_intervals,
        webserver_running,
        webserver_pid,
        webserver_url,
        tier,
        buckets,
        recent_resets,
    };

    if args.json {
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            println!("{json}");
        }
        return;
    }

    println!(
        "Antigravity hook: {} ({})",
        if report.hook_configured {
            "configured"
        } else {
            "not configured"
        },
        report.hook_config_path.as_deref().unwrap_or("unknown")
    );
    println!(
        "Local SQLite DB:  {} ({} intervals recorded)",
        report.db_path, report.recorded_intervals
    );
    match (&report.webserver_pid, &report.webserver_url) {
        (Some(pid), Some(url)) => {
            println!("Webserver:        running (pid={pid}, {url})");
        }
        _ => {
            println!("Webserver:        stopped (run `cut-rs webserver start --open`)");
        }
    }

    if !report.buckets.is_empty() {
        println!();
        let tier_label = report.tier.as_deref().unwrap_or("unknown");
        println!("Quota Buckets ({tier_label}):");
        for b in &report.buckets {
            let reset = b.reset_in.as_deref().unwrap_or("reset unknown");
            let equiv = b
                .bucket_api_equivalent_usd
                .map(|u| format!(", 100% ≈ ${u:.2} API"))
                .unwrap_or_default();
            println!(
                "  • {}: {:.1}% left ({reset}{equiv})",
                b.bucket_id, b.remaining_pct
            );
        }
    }
}

fn format_countdown(rfc3339: &str, now: DateTime<Utc>) -> String {
    let Ok(dt) = DateTime::parse_from_rfc3339(rfc3339) else {
        return rfc3339.to_string();
    };
    let secs = (dt.with_timezone(&Utc) - now).num_seconds();
    if secs <= 0 {
        return "resetting now".to_string();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {mins}m")
    } else {
        format!("resets in {mins}m")
    }
}
