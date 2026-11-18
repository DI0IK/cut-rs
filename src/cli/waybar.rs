use crate::cli::common::{CliToolState, default_state_file_path, load_state};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct WaybarArgs {
    /// Path to agy-cli state file
    #[arg(long, env = "CUT_RS_STATE_FILE")]
    pub state_file: Option<PathBuf>,

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

pub async fn run_waybar(args: WaybarArgs) {
    let path = args
        .state_file
        .unwrap_or_else(|| default_state_file_path("agy-cli-state.json"));
    let state = load_state(&path);
    let output = format_waybar(state.as_ref(), Utc::now(), args.show_used);
    if let Ok(json) = serde_json::to_string(&output) {
        println!("{json}");
    }
}

pub fn format_waybar(
    state: Option<&CliToolState>,
    now: DateTime<Utc>,
    show_used: bool,
) -> WaybarOutput {
    let Some(state) = state.filter(|s| !s.buckets.is_empty()) else {
        return WaybarOutput {
            text: "-- / --".to_string(),
            tooltip: "cut-rs: waiting for Antigravity quota data".to_string(),
            class: "unknown",
            percentage: 0,
        };
    };

    let fmt_pct = |rem: f64| -> String {
        let val = if show_used { 100.0 - rem } else { rem };
        format!("{:.0}%", val.round())
    };

    let mut entries: Vec<(&String, f64, Option<&str>)> = state
        .buckets
        .iter()
        .map(|(k, v)| {
            (
                k,
                (v.last_remaining_fraction * 100.0).clamp(0.0, 100.0),
                v.effective_reset_time(),
            )
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let min_rem = entries
        .iter()
        .map(|(_, rem, _)| *rem)
        .fold(f64::INFINITY, f64::min);

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

    let text = match (five_h, weekly) {
        (Some(h), Some(w)) => format!("{} / {}", fmt_pct(h), fmt_pct(w)),
        (Some(h), None) => fmt_pct(h),
        (None, Some(w)) => fmt_pct(w),
        (None, None) => entries
            .iter()
            .take(2)
            .map(|(k, rem, _)| format!("{k}: {}", fmt_pct(*rem)))
            .collect::<Vec<_>>()
            .join(" "),
    };

    let header = match &state.tier {
        Some(t) if !t.is_empty() && t != "unknown" => format!("Antigravity ({t})"),
        _ => "Antigravity Quota".to_string(),
    };
    let mut lines = vec![header];
    for (bucket, rem, reset) in &entries {
        let reset_str = reset
            .map(|r| format_reset_countdown(r, now))
            .unwrap_or_else(|| "reset unknown".to_string());
        lines.push(format!("• {bucket}: {rem:.1}% left ({reset_str})"));
    }

    let class = if min_rem < 20.0 {
        "critical"
    } else if min_rem < 50.0 {
        "warning"
    } else {
        "ok"
    };

    let bar_pct = if show_used { 100.0 - min_rem } else { min_rem };

    WaybarOutput {
        text,
        tooltip: lines.join("\n"),
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
    fn test_waybar_formats_compact_antigravity_tooltip() {
        let now = Utc.with_ymd_and_hms(2026, 11, 17, 10, 0, 0).unwrap();
        let mut buckets = HashMap::new();
        buckets.insert(
            "gemini_5h".to_string(),
            BucketTracker {
                last_remaining_fraction: 0.72,
                last_reset_time: Some("2026-11-17T12:30:00Z".to_string()),
                locked_reset_time: Some("2026-11-17T12:00:00Z".to_string()),
                started_at: 0,
                pending_usage: HashMap::new(),
            },
        );
        buckets.insert(
            "gemini_weekly".to_string(),
            BucketTracker {
                last_remaining_fraction: 0.90,
                last_reset_time: Some("2026-11-20T10:00:00Z".to_string()),
                locked_reset_time: None,
                started_at: 0,
                pending_usage: HashMap::new(),
            },
        );

        let state = CliToolState {
            provider: Some("antigravity".to_string()),
            tier: Some("pro".to_string()),
            buckets,
            ..Default::default()
        };

        let out = format_waybar(Some(&state), now, false);
        assert_eq!(out.text, "72% / 90%");
        assert_eq!(out.class, "ok");
        assert_eq!(out.percentage, 72);
        assert!(out.tooltip.contains("Antigravity (pro)"));
        assert!(out.tooltip.contains("gemini_5h: 72.0% left (resets in 2h 0m)"));
        assert!(out.tooltip.contains("gemini_weekly: 90.0% left (resets in 3d 0h)"));
    }
}
