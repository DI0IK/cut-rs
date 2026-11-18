use crate::cli::common::{default_state_file_path, load_state};
use crate::cli::webserver::get_running_webserver_url;
use crate::config::{default_database_url, default_db_path};
use crate::db::TelemetryRepository;
use crate::db::sqlite::SqliteRepository;
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;

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
}

#[derive(Debug, Serialize)]
struct BucketStatus {
    bucket_id: String,
    remaining_pct: f64,
    reset_in: Option<String>,
    reset_time: Option<String>,
}

pub async fn run_status(args: StatusArgs) {
    let home = dirs::home_dir();
    let cfg_path = home.map(|h| h.join(".gemini").join("antigravity-cli").join("settings.json"));
    let hook_configured = cfg_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_some_and(|raw| raw.contains("cut-rs"));

    let db_path = default_db_path();
    let recorded_intervals = if db_path.exists() {
        match SqliteRepository::connect_and_migrate(&default_database_url()).await {
            Ok(repo) => repo.count_intervals().await.unwrap_or(0),
            Err(_) => 0,
        }
    } else {
        0
    };

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
            buckets.push(BucketStatus {
                bucket_id: bucket_id.clone(),
                remaining_pct: (b.last_remaining_fraction * 100.0).clamp(0.0, 100.0),
                reset_in,
                reset_time,
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
            println!("  • {}: {:.1}% left ({reset})", b.bucket_id, b.remaining_pct);
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
