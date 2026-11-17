use crate::cli::common::{
    CliToolState, CommonClientArgs, QuotaSample, TickInput, acquire_state_lock,
    apply_token_and_quota_tick, default_state_file_path, epoch_to_rfc3339, flush_pending_intervals,
    load_state, save_state,
};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Debug, Args)]
pub struct CodexCliArgs {
    #[command(flatten)]
    pub common: CommonClientArgs,

    /// Path to Codex CLI sessions directory (default: ~/.codex/sessions)
    #[arg(long, env = "CODEX_SESSIONS_DIR")]
    pub sessions_dir: Option<PathBuf>,

    /// Fallback subscription tier when rollout logs omit plan_type
    #[arg(long, env = "CUT_RS_TIER", default_value = "default")]
    pub tier: String,

    /// Continuously watch ~/.codex/sessions for new rollout events
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Poll interval in seconds when --watch is enabled
    #[arg(long, default_value_t = 5)]
    pub poll_secs: u64,
}

#[derive(Debug, Deserialize)]
struct RolloutLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    line_type: String,
    payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CodexRateLimits {
    primary: Option<CodexLimitBucket>,
    secondary: Option<CodexLimitBucket>,
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexLimitBucket {
    used_percent: Option<f64>,
    resets_at: Option<i64>,
}

pub async fn run_codex_cli(args: CodexCliArgs) {
    loop {
        if let Err(err) = run_codex_once(&args).await
            && args.common.verbose
        {
            eprintln!("cut-rs codex-cli: {err:#}");
        }
        if !args.watch {
            break;
        }
        tokio::time::sleep(Duration::from_secs(args.poll_secs.max(1))).await;
    }
}

async fn run_codex_once(args: &CodexCliArgs) -> anyhow::Result<()> {
    let sessions_dir = args
        .sessions_dir
        .clone()
        .unwrap_or_else(default_codex_sessions_dir);
    if !sessions_dir.exists() {
        return Ok(());
    }

    let state_path = args
        .common
        .state_file
        .clone()
        .unwrap_or_else(|| default_state_file_path("codex-cli-state.json"));
    let _lock = acquire_state_lock(&state_path);
    let mut state = load_state(&state_path).unwrap_or_default();

    let emitted = scan_codex_sessions(&sessions_dir, &mut state, &args.tier);

    if !state.pending_intervals.is_empty() {
        flush_pending_intervals(&mut state, &args.common.server, args.common.timeout_ms).await;
    }

    save_state(&state_path, &state)?;

    if args.common.print_status {
        println!(
            "cut-rs codex: emitted={emitted} pending={}",
            state.pending_intervals.len()
        );
    }
    Ok(())
}

pub fn scan_codex_sessions(
    sessions_dir: &Path,
    state: &mut CliToolState,
    fallback_tier: &str,
) -> usize {
    let mut files = Vec::new();
    collect_recent_jsonl_files(sessions_dir, 0, &mut files);
    files.sort_by_key(|(mtime, p)| (*mtime, p.clone()));

    let mut total_emitted = 0usize;
    let mut active_session = state
        .session_id
        .clone()
        .unwrap_or_else(|| "codex-default".to_string());
    let mut active_model: Option<String> = None;

    for (_, path) in files {
        let key = path.to_string_lossy().to_string();
        let offset = state.file_offsets.get(&key).copied().unwrap_or(0);

        let Ok(mut file) = File::open(&path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if meta.len() <= offset {
            continue;
        }
        if file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }

        let mut reader = BufReader::new(file);
        let mut new_offset = offset;
        let mut line_buf = String::new();

        while let Ok(bytes) = reader.read_line(&mut line_buf) {
            if bytes == 0 {
                break;
            }
            if !line_buf.ends_with('\n') {
                // Incomplete trailing line; wait until next scan
                break;
            }
            new_offset += bytes as u64;
            let trimmed = line_buf.trim();
            if !trimmed.is_empty()
                && let Ok(line) = serde_json::from_str::<RolloutLine>(trimmed)
            {
                total_emitted += process_rollout_line(
                    &line,
                    state,
                    &mut active_session,
                    &mut active_model,
                    fallback_tier,
                );
            }
            line_buf.clear();
        }

        state.file_offsets.insert(key, new_offset);
    }

    // Prune deleted files from file_offsets
    state.file_offsets.retain(|p, _| Path::new(p).exists());

    total_emitted
}

fn process_rollout_line(
    line: &RolloutLine,
    state: &mut CliToolState,
    active_session: &mut String,
    active_model: &mut Option<String>,
    fallback_tier: &str,
) -> usize {
    match line.line_type.as_str() {
        "session_meta" => {
            if let Some(id) = line.payload.get("id").and_then(|v| v.as_str()) {
                *active_session = id.to_string();
            }
            if let Some(m) = line.payload.get("model").and_then(|v| v.as_str()) {
                *active_model = Some(m.to_string());
            }
            0
        }
        "turn_context" => {
            if let Some(m) = line.payload.get("model").and_then(|v| v.as_str()) {
                *active_model = Some(m.to_string());
            }
            0
        }
        "event_msg" => {
            let is_token_count = line
                .payload
                .get("type")
                .and_then(|v| v.as_str())
                == Some("token_count");
            if !is_token_count {
                return 0;
            }

            let info = line.payload.get("info");
            let total_usage: Option<TokenUsage> = info
                .and_then(|i| i.get("total_token_usage"))
                .and_then(|u| serde_json::from_value(u.clone()).ok());
            let last_usage: Option<TokenUsage> = info
                .and_then(|i| i.get("last_token_usage"))
                .and_then(|u| serde_json::from_value(u.clone()).ok());

            let Some(totals) = total_usage else {
                return 0;
            };

            let rate_limits: Option<CodexRateLimits> = line
                .payload
                .get("rate_limits")
                .filter(|v| !v.is_null())
                .and_then(|v| serde_json::from_value(v.clone()).ok());

            let Some(rl) = rate_limits else {
                return 0;
            };

            let tier = rl
                .plan_type
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(fallback_tier)
                .to_string();

            let mut quotas = Vec::new();
            if let Some(p) = rl.primary
                && let Some(used) = p.used_percent
                && used.is_finite()
            {
                quotas.push(QuotaSample {
                    bucket_id: "5h".to_string(),
                    remaining_fraction: ((100.0 - used) / 100.0).clamp(0.0, 1.0),
                    reset_time: p.resets_at.and_then(epoch_to_rfc3339),
                });
            }
            if let Some(s) = rl.secondary
                && let Some(used) = s.used_percent
                && used.is_finite()
            {
                quotas.push(QuotaSample {
                    bucket_id: "weekly".to_string(),
                    remaining_fraction: ((100.0 - used) / 100.0).clamp(0.0, 1.0),
                    reset_time: s.resets_at.and_then(epoch_to_rfc3339),
                });
            }
            if quotas.is_empty() {
                return 0;
            }

            let ts = line
                .timestamp
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.timestamp())
                .unwrap_or_else(|| Utc::now().timestamp());

            let tick = TickInput {
                session_id: active_session.clone(),
                provider: "codex".to_string(),
                tier,
                model: active_model.clone(),
                total_input_tokens: totals.input_tokens,
                total_output_tokens: totals.output_tokens,
                current_cached_tokens: last_usage
                    .map(|u| u.cached_input_tokens)
                    .unwrap_or(0),
                total_cached_tokens: Some(totals.cached_input_tokens),
                precomputed_delta: None,
                multi_deltas: None,
                skip_emit: false,
                quota_precision: None,
                quotas,
                timestamp: ts,
            };

            apply_token_and_quota_tick(state, &tick)
        }
        _ => 0,
    }
}

fn collect_recent_jsonl_files(dir: &Path, depth: usize, out: &mut Vec<(SystemTime, PathBuf)>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_recent_jsonl_files(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && let Ok(meta) = entry.metadata()
        {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if mtime >= cutoff {
                out.push((mtime, path));
            }
        }
    }
}

fn default_codex_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_codex_cli_incremental_jsonl_scan_and_rate_limits() {
        let dir = TempDir::new().unwrap();
        let rollout = dir.path().join("rollout-test.jsonl");

        let lines = [
            r#"{"timestamp":"2026-11-17T10:00:00Z","type":"session_meta","payload":{"id":"codex-s1"}}"#,
            r#"{"timestamp":"2026-11-17T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            r#"{"timestamp":"2026-11-17T10:00:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"output_tokens":1000,"cached_input_tokens":2000},"last_token_usage":{"input_tokens":10000,"output_tokens":1000,"cached_input_tokens":2000}},"rate_limits":{"primary":{"used_percent":5.0,"resets_at":1794920000},"secondary":{"used_percent":1.0,"resets_at":1795400000},"plan_type":"plus"}}}"#,
            r#"{"timestamp":"2026-11-17T10:01:10Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50000,"output_tokens":5000,"cached_input_tokens":12000},"last_token_usage":{"input_tokens":40000,"output_tokens":4000,"cached_input_tokens":10000}},"rate_limits":{"primary":{"used_percent":7.0,"resets_at":1794920000},"secondary":{"used_percent":1.5,"resets_at":1795400000},"plan_type":"plus"}}}"#,
        ];
        std::fs::write(&rollout, format!("{}\n", lines.join("\n"))).unwrap();

        let mut state = CliToolState::default();
        let emitted = scan_codex_sessions(dir.path(), &mut state, "default");
        assert_eq!(emitted, 2);
        assert_eq!(state.pending_intervals.len(), 2);
        assert_eq!(state.pending_intervals[0].provider, "codex");
        assert_eq!(state.pending_intervals[0].tier, "plus");
        assert_eq!(state.pending_intervals[0].usage[0].tokens_in, 40_000);
        assert_eq!(state.pending_intervals[0].usage[0].tokens_cached_in, 10_000);

        // Second scan with no new lines should emit 0
        let emitted_again = scan_codex_sessions(dir.path(), &mut state, "default");
        assert_eq!(emitted_again, 0);
    }
}
