use crate::cli::common::{
    default_state_file_path, flush_pending_intervals, load_shared_auth, load_state, save_state,
    shared_auth_path,
};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_SERVER: &str = "https://cut-rs.dominikstahl.dev";

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// cut-rs server base URL
    #[arg(long, env = "CUT_RS_SERVER", default_value = DEFAULT_SERVER)]
    pub server: String,

    /// Flush any pending intervals to the server now
    #[arg(long, default_value_t = false)]
    pub flush: bool,

    /// Output status as JSON
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct StatusReport {
    server: String,
    server_reachable: bool,
    server_latency_ms: Option<u128>,
    registered_client_id: Option<String>,
    auth_path: String,
    tools: Vec<ToolStatus>,
}

#[derive(Debug, Serialize)]
struct ToolStatus {
    name: &'static str,
    config_path: String,
    configured: bool,
    state_exists: bool,
    provider: Option<String>,
    tier: Option<String>,
    pending_intervals: usize,
    buckets: Vec<BucketStatus>,
}

#[derive(Debug, Serialize)]
struct BucketStatus {
    bucket_id: String,
    remaining_pct: f64,
    reset_in: Option<String>,
}

pub async fn run_status(args: StatusArgs) {
    let home = dirs::home_dir();
    let server = args.server.trim_end_matches('/').to_string();

    let (server_reachable, server_latency_ms) = check_server_health(&server).await;
    let auth = load_shared_auth(None);

    let tool_specs: &[(&str, &str, Option<PathBuf>)] = &[
        (
            "Claude Code",
            "claude-cli-state.json",
            home.as_ref().map(|h| h.join(".claude").join("settings.json")),
        ),
        (
            "Codex CLI",
            "codex-cli-state.json",
            home.as_ref().map(|h| h.join(".codex").join("config.toml")),
        ),
        (
            "Antigravity CLI",
            "agy-cli-state.json",
            home.as_ref()
                .map(|h| h.join(".gemini").join("antigravity-cli").join("settings.json")),
        ),
    ];

    let now = Utc::now();
    let mut tools = Vec::new();

    for (name, state_filename, cfg_path) in tool_specs {
        let configured = cfg_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|raw| raw.contains("cut-rs"));

        let state_path = default_state_file_path(state_filename);
        let mut state_opt = load_state(&state_path);

        if args.flush
            && let Some(st) = state_opt.as_mut()
            && !st.pending_intervals.is_empty()
        {
            flush_pending_intervals(st, &server, 4000).await;
            let _ = save_state(&state_path, st);
        }

        let (state_exists, provider, tier, pending_intervals, buckets) = match state_opt {
            Some(st) => {
                let mut b_list: Vec<BucketStatus> = st
                    .buckets
                    .iter()
                    .map(|(id, tracker)| BucketStatus {
                        bucket_id: id.clone(),
                        remaining_pct: (tracker.last_remaining_fraction * 100.0).clamp(0.0, 100.0),
                        reset_in: tracker
                            .last_reset_time
                            .as_deref()
                            .and_then(|rt| format_reset_in(rt, now)),
                    })
                    .collect();
                b_list.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));
                (
                    true,
                    st.provider,
                    st.tier,
                    st.pending_intervals.len(),
                    b_list,
                )
            }
            None => (false, None, None, 0, Vec::new()),
        };

        tools.push(ToolStatus {
            name,
            config_path: cfg_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            configured,
            state_exists,
            provider,
            tier,
            pending_intervals,
            buckets,
        });
    }

    let report = StatusReport {
        server,
        server_reachable,
        server_latency_ms,
        registered_client_id: auth.map(|a| a.client_id),
        auth_path: shared_auth_path().display().to_string(),
        tools,
    };

    if args.json {
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            println!("{json}");
        }
        return;
    }

    print_human_report(&report);
}

async fn check_server_health(server: &str) -> (bool, Option<u128>) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(3000))
        .build()
    else {
        return (false, None);
    };
    let start = Instant::now();
    let ok = client
        .get(format!("{server}/health"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());
    let ms = start.elapsed().as_millis();
    (ok, ok.then_some(ms))
}

fn format_reset_in(rfc3339: &str, now: DateTime<Utc>) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(rfc3339).ok()?.with_timezone(&Utc);
    let secs = (dt - now).num_seconds();
    if secs <= 0 {
        return Some("resetting".to_string());
    }
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        Some(format!("{days}d {hours}h"))
    } else if hours > 0 {
        Some(format!("{hours}h {mins}m"))
    } else {
        Some(format!("{mins}m"))
    }
}

fn print_human_report(report: &StatusReport) {
    println!("cut-rs status");
    println!("-------------");
    match (report.server_reachable, report.server_latency_ms) {
        (true, Some(ms)) => println!("Server:   {} (reachable, {ms}ms)", report.server),
        _ => println!("Server:   {} (UNREACHABLE)", report.server),
    }
    match &report.registered_client_id {
        Some(id) => println!("Auth:     registered ({id})"),
        None => println!("Auth:     not registered yet (run `cut-rs init` or trigger a session)"),
    }
    println!();

    for t in &report.tools {
        let cfg_badge = if t.configured { "configured" } else { "not configured" };
        let tier_str = t.tier.as_deref().unwrap_or("-");
        println!(
            "{:<16} [{}] tier={} pending={}",
            t.name, cfg_badge, tier_str, t.pending_intervals
        );
        if t.buckets.is_empty() {
            if t.configured {
                println!("  no quota samples recorded yet");
            }
        } else {
            for b in &t.buckets {
                let reset = b
                    .reset_in
                    .as_deref()
                    .map(|r| format!(" (resets in {r})"))
                    .unwrap_or_default();
                println!("  {:<12} {:>5.1}% remaining{reset}", b.bucket_id, b.remaining_pct);
            }
        }
    }
}
