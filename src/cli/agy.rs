use crate::cli::common::{
    CliToolState, CommonClientArgs, QuotaSnapshotRecord, StoredQuotaSample, acquire_state_lock,
    default_state_file_path, load_state, run_chained_command, save_state,
};
use chrono::Utc;
use clap::Args;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;

#[derive(Debug, Args)]
pub struct AgyCliArgs {
    #[command(flatten)]
    pub common: CommonClientArgs,
}

#[derive(Debug, Deserialize)]
pub struct StatusLinePayload {
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub product: Option<String>,
    pub plan_tier: Option<String>,
    pub quota: HashMap<String, QuotaBucketInfo>,
}

#[derive(Debug, Deserialize)]
pub struct QuotaBucketInfo {
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

pub async fn run_agy_cli(args: AgyCliArgs) {
    let mut stdin_buf = String::new();
    if std::io::stdin().read_to_string(&mut stdin_buf).is_err() {
        return;
    }

    let record_fut = async {
        if let Err(err) = record_quota_snapshot_from_raw(&args.common, &stdin_buf)
            && args.common.verbose
        {
            eprintln!("cut-rs agy-cli: {err:#}");
        }
    };

    if let Some(chain_cmd) = args.common.chain.as_deref().filter(|s| !s.trim().is_empty()) {
        let chain_fut = run_chained_command(chain_cmd, &stdin_buf, args.common.verbose);
        tokio::join!(record_fut, chain_fut);
    } else {
        record_fut.await;
    }
}

fn record_quota_snapshot_from_raw(args: &CommonClientArgs, raw: &str) -> anyhow::Result<()> {
    let payload: StatusLinePayload = serde_json::from_str(raw)?;
    let now = Utc::now().timestamp();
    let state_path = args
        .state_file
        .clone()
        .unwrap_or_else(|| default_state_file_path("agy-cli-state.json"));

    let _lock = acquire_state_lock(&state_path);
    let mut state = load_state(&state_path).unwrap_or_default();
    let recorded = apply_quota_snapshot(&mut state, payload, now);
    if recorded {
        save_state(&state_path, &state)?;
    }
    drop(_lock);

    if recorded {
        ensure_agy_worker_running(args, &state_path);
    }

    if args.print_status {
        println!(
            "cut-rs agy-cli: queued_snapshots={} pending_intervals={}",
            state.quota_snapshots.len(),
            state.pending_intervals.len()
        );
    }
    Ok(())
}

pub fn apply_quota_snapshot(
    state: &mut CliToolState,
    payload: StatusLinePayload,
    now: i64,
) -> bool {
    if let Some(sid) = payload.session_id.or(payload.conversation_id) {
        state.session_id = Some(sid);
    }

    let mut quotas: Vec<StoredQuotaSample> = payload
        .quota
        .into_iter()
        .filter_map(|(bucket_id, q)| {
            if q.disabled == Some(true) {
                return None;
            }
            let remaining_fraction = q.remaining_fraction?;
            if !remaining_fraction.is_finite() || !(0.0..=1.0).contains(&remaining_fraction) {
                return None;
            }
            Some(StoredQuotaSample {
                bucket_id,
                remaining_fraction,
                reset_time: q.reset_time,
            })
        })
        .collect();

    if quotas.is_empty() {
        return false;
    }
    quotas.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));

    let provider = payload.product.unwrap_or_else(|| "antigravity".to_string());
    let tier = payload.plan_tier.unwrap_or_else(|| "unknown".to_string());

    // Skip exact duplicate consecutive snapshots within 2s
    if let Some(last) = state.quota_snapshots.last()
        && now - last.timestamp <= 2
        && last.provider == provider
        && last.tier == tier
        && last.quotas.len() == quotas.len()
        && last.quotas.iter().zip(&quotas).all(|(a, b)| {
            a.bucket_id == b.bucket_id
                && (a.remaining_fraction - b.remaining_fraction).abs() < 1e-9
                && a.reset_time == b.reset_time
        })
    {
        return false;
    }

    state.quota_snapshots.push(QuotaSnapshotRecord {
        timestamp: now,
        provider,
        tier,
        quotas,
    });

    if state.quota_snapshots.len() > 2000 {
        let excess = state.quota_snapshots.len() - 2000;
        state.quota_snapshots.drain(0..excess);
    }

    true
}

#[cfg(not(test))]
fn ensure_agy_worker_running(args: &CommonClientArgs, state_path: &std::path::Path) {
    if crate::cli::agy_worker::is_worker_running(state_path) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("agy-worker")
        .arg("--server")
        .arg(&args.server)
        .arg("--state-file")
        .arg(state_path)
        .arg("--idle-exit-secs")
        .arg("60")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let _ = cmd.spawn();
}

#[cfg(test)]
fn ensure_agy_worker_running(_args: &CommonClientArgs, _state_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agy_cli_records_quota_snapshot_without_reading_transcripts() {
        let raw = r#"{
            "session_id": "s1",
            "product": "antigravity",
            "plan_tier": "Google AI Pro",
            "model": {"id": "Gemini 4 Hydrogen (Medium)"},
            "quota": {
                "gemini-5h": {"remaining_fraction": 0.995, "reset_time": "2026-11-17T23:00:00Z"},
                "disabled-b": {"remaining_fraction": 1.0, "disabled": true}
            }
        }"#;
        let payload: StatusLinePayload = serde_json::from_str(raw).unwrap();
        let mut state = CliToolState::default();

        assert!(apply_quota_snapshot(&mut state, payload, 1_700_000_000));
        assert_eq!(state.quota_snapshots.len(), 1);
        assert_eq!(state.quota_snapshots[0].quotas.len(), 1);
        assert_eq!(state.quota_snapshots[0].quotas[0].bucket_id, "gemini-5h");

        // Duplicate within 2s is ignored
        let payload2: StatusLinePayload = serde_json::from_str(raw).unwrap();
        assert!(!apply_quota_snapshot(&mut state, payload2, 1_700_000_001));
        assert_eq!(state.quota_snapshots.len(), 1);
    }
}
