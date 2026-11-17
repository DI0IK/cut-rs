use crate::cli::common::{CliToolState, default_state_file_path, load_state};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

const KNOWN_STATE_FILES: &[&str] = &[
    "agy-cli-state.json",
    "claude-cli-state.json",
    "codex-cli-state.json",
];

#[derive(Debug, Args)]
pub struct WaybarArgs {
    /// Path to a specific tool state file (by default aggregates all active tools)
    #[arg(long, env = "CUT_RS_STATE_FILE")]
    pub state_file: Option<PathBuf>,

    /// Filter to a specific provider (e.g. antigravity, claude, codex)
    #[arg(long)]
    pub provider: Option<String>,

    /// Optional cut-rs server URL to enrich tooltip with estimated model token costs
    #[arg(long, env = "CUT_RS_SERVER")]
    pub server: Option<String>,

    /// HTTP timeout in milliseconds when querying --server
    #[arg(long, default_value_t = 500)]
    pub timeout_ms: u64,

    /// Display used % instead of remaining %
    #[arg(long, default_value_t = false)]
    pub show_used: bool,
}

#[derive(Debug, Serialize)]
pub struct WaybarOutput {
    pub text: String,
    pub tooltip: String,
    pub class: &'static str,
    pub percentage: u8,
}

#[derive(Debug, Deserialize)]
pub struct EstimatesView {
    windows: Vec<WindowView>,
}

#[derive(Debug, Deserialize)]
pub struct WindowView {
    window_type: String,
    models: Vec<ModelView>,
}

#[derive(Debug, Deserialize)]
pub struct ModelView {
    model: String,
    pct_per_1m_in: f64,
    pct_per_1m_out: f64,
    pct_per_1m_cached_in: f64,
}

pub async fn run_waybar(args: WaybarArgs) {
    let mut states = load_matching_states(&args);

    if let Some(filter) = args.provider.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
        states.retain(|s| s.provider.as_deref() == Some(filter.as_str()));
    }

    let mut estimates_by_provider = Vec::new();
    if let Some(server) = &args.server {
        for s in &states {
            if !s.buckets.is_empty() {
                let provider = s.provider.as_deref().unwrap_or("antigravity");
                let tier = s.tier.as_deref().unwrap_or("unknown");
                if let Some(est) = fetch_estimates(server, provider, tier, args.timeout_ms).await {
                    estimates_by_provider.push((provider.to_string(), est));
                }
            }
        }
    }

    let output = format_waybar_multi(
        &states,
        &estimates_by_provider,
        Utc::now(),
        args.show_used,
    );
    if let Ok(json) = serde_json::to_string(&output) {
        println!("{json}");
    }
}

fn load_matching_states(args: &WaybarArgs) -> Vec<CliToolState> {
    if let Some(path) = &args.state_file {
        return load_state(path).into_iter().collect();
    }
    KNOWN_STATE_FILES
        .iter()
        .filter_map(|name| load_state(&default_state_file_path(name)))
        .filter(|s| !s.buckets.is_empty())
        .collect()
}

async fn fetch_estimates(
    server: &str,
    provider: &str,
    tier: &str,
    timeout_ms: u64,
) -> Option<EstimatesView> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
        .ok()?;
    let url = format!(
        "{}/v1/estimates/{}/{}",
        server.trim_end_matches('/'),
        provider,
        tier
    );
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<EstimatesView>().await.ok()
}

pub fn format_waybar_multi(
    states: &[CliToolState],
    estimates: &[(String, EstimatesView)],
    now: DateTime<Utc>,
    show_used: bool,
) -> WaybarOutput {
    let active: Vec<&CliToolState> = states.iter().filter(|s| !s.buckets.is_empty()).collect();
    if active.is_empty() {
        return WaybarOutput {
            text: "-- / --".to_string(),
            tooltip: "cut-rs: waiting for quota data".to_string(),
            class: "unknown",
            percentage: 0,
        };
    }

    let fmt_pct = |rem: f64, with_suffix: bool| -> String {
        let val = if show_used { 100.0 - rem } else { rem };
        if with_suffix {
            format!("{:.0}%", val.round())
        } else {
            format!("{:.0}", val.round())
        }
    };

    let mut global_min_rem = f64::INFINITY;
    let mut text_parts = Vec::new();
    let mut tooltip_sections = Vec::new();
    let multi = active.len() > 1;

    for state in &active {
        let mut entries: Vec<(&String, f64, Option<&str>)> = state
            .buckets
            .iter()
            .map(|(k, v)| {
                (
                    k,
                    (v.last_remaining_fraction * 100.0).clamp(0.0, 100.0),
                    v.last_reset_time.as_deref(),
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        for (_, rem, _) in &entries {
            if *rem < global_min_rem {
                global_min_rem = *rem;
            }
        }

        let five_h = entries
            .iter()
            .filter(|(k, _, _)| k.contains("5h"))
            .map(|(_, rem, _)| *rem)
            .reduce(f64::min);
        let weekly = entries
            .iter()
            .filter(|(k, _, _)| k.contains("weekly") || k.contains("week"))
            .map(|(_, rem, _)| *rem)
            .reduce(f64::min);

        let summary = match (five_h, weekly) {
            (Some(h), Some(w)) => {
                if multi {
                    format!("{}/{}%", fmt_pct(h, false), fmt_pct(w, false))
                } else {
                    format!("{} / {}", fmt_pct(h, true), fmt_pct(w, true))
                }
            }
            (Some(h), None) => fmt_pct(h, true),
            (None, Some(w)) => fmt_pct(w, true),
            (None, None) => entries
                .iter()
                .take(2)
                .map(|(k, rem, _)| format!("{k}: {}", fmt_pct(*rem, true)))
                .collect::<Vec<_>>()
                .join(" "),
        };

        let provider_raw = state.provider.as_deref().unwrap_or("unknown");
        if multi {
            let short = match provider_raw {
                "antigravity" => "agy",
                other => other,
            };
            text_parts.push(format!("{short} {summary}"));
        } else {
            text_parts.push(summary);
        }

        let header = match (&state.provider, &state.tier) {
            (Some(p), Some(t)) => format!("{p} ({t})"),
            _ => "Quota Buckets".to_string(),
        };
        let mut lines = vec![header];
        for (bucket, rem, reset) in &entries {
            let reset_str = reset
                .map(|r| format_reset_countdown(r, now))
                .unwrap_or_else(|| "reset unknown".to_string());
            lines.push(format!("• {bucket}: {rem:.1}% left ({reset_str})"));
        }

        if let Some((_, est)) = estimates.iter().find(|(p, _)| p == provider_raw) {
            for w in &est.windows {
                for m in &w.models {
                    lines.push(format!(
                        "  [{}] {}: {:.2}% in / {:.2}% out / {:.2}% cached (per 1M)",
                        w.window_type,
                        m.model,
                        m.pct_per_1m_in,
                        m.pct_per_1m_out,
                        m.pct_per_1m_cached_in
                    ));
                }
            }
        }

        tooltip_sections.push(lines.join("\n"));
    }

    let class = if global_min_rem < 20.0 {
        "critical"
    } else if global_min_rem < 50.0 {
        "warning"
    } else {
        "ok"
    };

    let bar_pct = if show_used {
        100.0 - global_min_rem
    } else {
        global_min_rem
    };

    WaybarOutput {
        text: text_parts.join(" | "),
        tooltip: tooltip_sections.join("\n\n"),
        class,
        percentage: bar_pct.round().clamp(0.0, 100.0) as u8,
    }
}

fn format_reset_countdown(reset_rfc3339: &str, now: DateTime<Utc>) -> String {
    let Ok(dt) = DateTime::parse_from_rfc3339(reset_rfc3339) else {
        return format!("resets {reset_rfc3339}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::common::BucketTracker;
    use chrono::TimeZone;
    use std::collections::HashMap;

    #[test]
    fn test_waybar_formats_single_and_multi_provider() {
        let now = Utc.with_ymd_and_hms(2026, 11, 17, 10, 0, 0).unwrap();
        let mut agy_buckets = HashMap::new();
        agy_buckets.insert(
            "gemini-5h".to_string(),
            BucketTracker {
                last_remaining_fraction: 0.84,
                last_reset_time: Some("2026-11-17T13:30:00Z".to_string()),
                started_at: 0,
                pending_usage: HashMap::new(),
            },
        );
        agy_buckets.insert(
            "gemini-weekly".to_string(),
            BucketTracker {
                last_remaining_fraction: 0.96,
                last_reset_time: Some("2026-11-20T10:00:00Z".to_string()),
                started_at: 0,
                pending_usage: HashMap::new(),
            },
        );

        let agy_state = CliToolState {
            provider: Some("antigravity".to_string()),
            tier: Some("google ai pro".to_string()),
            buckets: agy_buckets,
            ..Default::default()
        };

        let out = format_waybar_multi(std::slice::from_ref(&agy_state), &[], now, false);
        assert_eq!(out.text, "84% / 96%");
        assert_eq!(out.class, "ok");
        assert_eq!(out.percentage, 84);
        assert!(out.tooltip.contains("gemini-5h: 84.0% left (resets in 3h 30m)"));

        // Multi-provider: add claude at 15% remaining -> class becomes critical
        let mut claude_buckets = HashMap::new();
        claude_buckets.insert(
            "5h".to_string(),
            BucketTracker {
                last_remaining_fraction: 0.15,
                last_reset_time: Some("2026-11-17T11:00:00Z".to_string()),
                started_at: 0,
                pending_usage: HashMap::new(),
            },
        );
        claude_buckets.insert(
            "weekly".to_string(),
            BucketTracker {
                last_remaining_fraction: 0.70,
                last_reset_time: Some("2026-11-22T10:00:00Z".to_string()),
                started_at: 0,
                pending_usage: HashMap::new(),
            },
        );
        let claude_state = CliToolState {
            provider: Some("claude".to_string()),
            tier: Some("pro".to_string()),
            buckets: claude_buckets,
            ..Default::default()
        };

        let multi = format_waybar_multi(&[agy_state, claude_state], &[], now, false);
        assert_eq!(multi.text, "agy 84/96% | claude 15/70%");
        assert_eq!(multi.class, "critical");
        assert_eq!(multi.percentage, 15);
    }

    #[test]
    fn test_waybar_handles_empty_state() {
        let now = Utc.with_ymd_and_hms(2026, 11, 17, 10, 0, 0).unwrap();
        let out = format_waybar_multi(&[], &[], now, false);
        assert_eq!(out.text, "-- / --");
        assert_eq!(out.class, "unknown");
    }
}
