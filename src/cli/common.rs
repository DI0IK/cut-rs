use crate::auth::leading_zero_bits;
use crate::models::{IntervalSubmission, UsageEntry, normalize_key};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Args)]
pub struct CommonClientArgs {
    /// cut-rs server base URL
    #[arg(long, env = "CUT_RS_SERVER", default_value = "https://cut-rs.dominikstahl.dev")]
    pub server: String,

    /// Path to local state file for token & turn-diff tracking
    #[arg(long, env = "CUT_RS_STATE_FILE")]
    pub state_file: Option<PathBuf>,

    /// HTTP timeout in milliseconds (kept low so statusLine never hits its 5s limit)
    #[arg(long, default_value_t = 1500)]
    pub timeout_ms: u64,

    /// Print a compact status summary to stdout (off by default for silent logging)
    #[arg(long, default_value_t = false)]
    pub print_status: bool,

    /// Log errors to stderr instead of failing silently
    #[arg(long, default_value_t = false)]
    pub verbose: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedAuth {
    pub client_id: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketTracker {
    pub last_remaining_fraction: f64,
    pub last_reset_time: Option<String>,
    pub started_at: i64,
    #[serde(default)]
    pub pending_usage: HashMap<String, UsageEntry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CliToolState {
    /// Legacy per-file auth (migrated to shared auth.json on save/load)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SavedAuth>,
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    pub last_total_input_tokens: u64,
    pub last_total_output_tokens: u64,
    #[serde(default)]
    pub buckets: HashMap<String, BucketTracker>,
    #[serde(default)]
    pub pending_intervals: Vec<IntervalSubmission>,
    /// Incremental byte offsets for log-tailing adapters (e.g. codex-cli)
    #[serde(default)]
    pub file_offsets: HashMap<String, u64>,
}

#[derive(Debug, Clone)]
pub struct QuotaSample {
    pub bucket_id: String,
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TickInput {
    pub session_id: String,
    pub provider: String,
    pub tier: String,
    pub model: Option<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    /// Cached input tokens for the current turn (only attributed when input/output advance)
    pub current_cached_tokens: u64,
    /// Cumulative cached input tokens if the tool reports a monotonic total instead of per-turn
    pub total_cached_tokens: Option<u64>,
    pub quotas: Vec<QuotaSample>,
    pub timestamp: i64,
}

pub fn state_dir() -> PathBuf {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("cut-rs")
}

pub fn default_state_file_path(filename: &str) -> PathBuf {
    state_dir().join(filename)
}

pub fn shared_auth_path() -> PathBuf {
    state_dir().join("auth.json")
}

pub fn load_state(path: &Path) -> Option<CliToolState> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, &data)?;
    if std::fs::rename(&tmp, path).is_err() {
        // Windows antivirus/indexers can briefly lock the target; fallback to direct write
        std::fs::write(path, &data)?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

pub fn save_state(path: &Path, state: &CliToolState) -> anyhow::Result<()> {
    atomic_write_json(path, state)
}

pub fn load_shared_auth(fallback_state: Option<&CliToolState>) -> Option<SavedAuth> {
    if let Ok(data) = std::fs::read_to_string(shared_auth_path())
        && let Ok(auth) = serde_json::from_str::<SavedAuth>(&data)
    {
        return Some(auth);
    }
    if let Some(auth) = fallback_state.and_then(|s| s.auth.clone()) {
        let _ = save_shared_auth(&auth);
        return Some(auth);
    }
    // Also check legacy agy-cli-state.json if running a different adapter first
    if let Some(agy) = load_state(&default_state_file_path("agy-cli-state.json"))
        && let Some(auth) = agy.auth
    {
        let _ = save_shared_auth(&auth);
        return Some(auth);
    }
    None
}

pub fn save_shared_auth(auth: &SavedAuth) -> anyhow::Result<()> {
    atomic_write_json(&shared_auth_path(), auth)
}

pub fn clear_shared_auth() {
    let _ = std::fs::remove_file(shared_auth_path());
}

/// Shared runner for any `stdin`-based `statusLine` tool adapter.
pub async fn run_stdin_adapter<F>(
    args: &CommonClientArgs,
    default_state_filename: &str,
    parse_tick: F,
) where
    F: FnOnce(&str, i64) -> anyhow::Result<Option<TickInput>>,
{
    if let Err(err) = run_stdin_adapter_inner(args, default_state_filename, parse_tick).await
        && args.verbose
    {
        eprintln!("cut-rs: {err:#}");
    }
}

async fn run_stdin_adapter_inner<F>(
    args: &CommonClientArgs,
    default_state_filename: &str,
    parse_tick: F,
) -> anyhow::Result<()>
where
    F: FnOnce(&str, i64) -> anyhow::Result<Option<TickInput>>,
{
    let mut stdin_buf = String::new();
    std::io::stdin().read_to_string(&mut stdin_buf)?;
    if stdin_buf.trim().is_empty() {
        return Ok(());
    }

    let now = Utc::now().timestamp();
    let Some(tick) = parse_tick(&stdin_buf, now)? else {
        return Ok(());
    };

    let state_path = args
        .state_file
        .clone()
        .unwrap_or_else(|| default_state_file_path(default_state_filename));
    let mut state = load_state(&state_path).unwrap_or_default();

    let model_label = tick.model.clone().unwrap_or_else(|| "no-model".to_string());
    let emitted = apply_token_and_quota_tick(&mut state, &tick);

    if !state.pending_intervals.is_empty() {
        flush_pending_intervals(&mut state, &args.server, args.timeout_ms).await;
    }

    save_state(&state_path, &state)?;

    if args.print_status {
        println!(
            "cut-rs: {model_label} | emitted={emitted} pending={}",
            state.pending_intervals.len()
        );
    }

    Ok(())
}

/// Diffs `tick` against `state`, accumulates per-model token deltas, and queues quota-drop intervals.
pub fn apply_token_and_quota_tick(state: &mut CliToolState, tick: &TickInput) -> usize {
    let provider = normalize_key(&tick.provider);
    let tier = normalize_key(&tick.tier);
    state.provider = Some(provider.clone());
    state.tier = Some(tier.clone());

    let session_changed = state.session_id.as_deref() != Some(tick.session_id.as_str());
    let counters_reset = tick.total_input_tokens < state.last_total_input_tokens
        || tick.total_output_tokens < state.last_total_output_tokens;

    let (delta_in, delta_out, delta_cached) = if session_changed || counters_reset {
        state.session_id = Some(tick.session_id.clone());
        state.last_total_input_tokens = tick.total_input_tokens;
        state.last_total_output_tokens = tick.total_output_tokens;
        (0, 0, 0)
    } else {
        let di = tick.total_input_tokens - state.last_total_input_tokens;
        let do_ = tick.total_output_tokens - state.last_total_output_tokens;
        state.last_total_input_tokens = tick.total_input_tokens;
        state.last_total_output_tokens = tick.total_output_tokens;
        let dc = if di > 0 || do_ > 0 {
            tick.current_cached_tokens
        } else {
            0
        };
        (di, do_, dc)
    };

    if let Some(model_raw) = &tick.model {
        let model_name = normalize_key(model_raw);
        if !model_name.is_empty() && (delta_in > 0 || delta_out > 0 || delta_cached > 0) {
            for tracker in state.buckets.values_mut() {
                let entry = tracker
                    .pending_usage
                    .entry(model_name.clone())
                    .or_insert_with(|| UsageEntry {
                        model: model_name.clone(),
                        tokens_in: 0,
                        tokens_out: 0,
                        tokens_cached_in: 0,
                        request_count: 0,
                    });
                entry.tokens_in = entry.tokens_in.saturating_add(delta_in);
                entry.tokens_out = entry.tokens_out.saturating_add(delta_out);
                entry.tokens_cached_in = entry.tokens_cached_in.saturating_add(delta_cached);
                entry.request_count = entry.request_count.saturating_add(1);
            }
        }
    }

    let active_ids: std::collections::HashSet<&str> =
        tick.quotas.iter().map(|q| q.bucket_id.as_str()).collect();
    state
        .buckets
        .retain(|bucket_id, _| active_ids.contains(bucket_id.as_str()));

    let now = tick.timestamp;
    let mut emitted = 0usize;
    for q in &tick.quotas {
        if !q.remaining_fraction.is_finite() || !(0.0..=1.0).contains(&q.remaining_fraction) {
            continue;
        }
        let frac = q.remaining_fraction;
        let tracker = state
            .buckets
            .entry(q.bucket_id.clone())
            .or_insert_with(|| BucketTracker {
                last_remaining_fraction: frac,
                last_reset_time: q.reset_time.clone(),
                started_at: now,
                pending_usage: HashMap::new(),
            });

        let reset_jumped = match (&tracker.last_reset_time, &q.reset_time) {
            (Some(prev), Some(curr)) => curr != prev && frac >= tracker.last_remaining_fraction,
            _ => false,
        };

        if frac > tracker.last_remaining_fraction + 1e-6 || reset_jumped {
            tracker.last_remaining_fraction = frac;
            tracker.last_reset_time = q.reset_time.clone();
            tracker.started_at = now;
            tracker.pending_usage.clear();
            continue;
        }

        if frac < tracker.last_remaining_fraction - 1e-7 {
            let quota_before_pct = tracker.last_remaining_fraction * 100.0;
            let quota_after_pct = frac * 100.0;
            let duration = now - tracker.started_at;
            let usage: Vec<UsageEntry> = tracker.pending_usage.values().cloned().collect();
            let total_tokens: u64 = usage
                .iter()
                .map(|u| u.tokens_in + u.tokens_out + u.tokens_cached_in)
                .sum();

            if total_tokens > 0 && duration > 0 && duration <= 1800 {
                let mut hasher = Sha256::new();
                hasher.update(format!(
                    "{provider}:{}:{}:{}:{now}:{quota_after_pct:.6}",
                    tick.session_id, q.bucket_id, tracker.started_at
                ));
                let idempotency_key = hex::encode(hasher.finalize());

                state.pending_intervals.push(IntervalSubmission {
                    idempotency_key,
                    provider: provider.clone(),
                    tier: tier.clone(),
                    window_type: normalize_key(&q.bucket_id),
                    started_at: tracker.started_at,
                    ended_at: now,
                    quota_before_pct,
                    quota_after_pct,
                    quota_precision: 0.01,
                    usage,
                });
                emitted += 1;
            }

            tracker.last_remaining_fraction = frac;
            tracker.last_reset_time = q.reset_time.clone();
            tracker.started_at = now;
            tracker.pending_usage.clear();
        }
    }

    if state.pending_intervals.len() > 500 {
        let excess = state.pending_intervals.len() - 500;
        state.pending_intervals.drain(0..excess);
    }

    emitted
}

pub async fn flush_pending_intervals(state: &mut CliToolState, server: &str, timeout_ms: u64) {
    if state.pending_intervals.is_empty() {
        return;
    }
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    else {
        return;
    };

    let mut auth = load_shared_auth(Some(state));
    if auth.is_none()
        && let Ok(new_auth) = register_client(&client, server).await
    {
        let _ = save_shared_auth(&new_auth);
        state.auth = Some(new_auth.clone());
        auth = Some(new_auth);
    }

    if let Some(auth) = auth {
        let batch: Vec<IntervalSubmission> =
            state.pending_intervals.iter().take(50).cloned().collect();
        let url = format!("{}/v1/telemetry/intervals", server.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .bearer_auth(&auth.token)
            .json(&serde_json::json!({ "intervals": batch }))
            .send()
            .await;

        if let Ok(r) = resp {
            if r.status().is_success() {
                let drained = batch.len().min(state.pending_intervals.len());
                state.pending_intervals.drain(0..drained);
            } else if r.status() == reqwest::StatusCode::UNAUTHORIZED {
                clear_shared_auth();
                state.auth = None;
            }
        }
    }
}

async fn register_client(client: &reqwest::Client, server: &str) -> anyhow::Result<SavedAuth> {
    #[derive(Deserialize)]
    struct ChallengeResp {
        challenge: String,
        difficulty_bits: u8,
    }
    #[derive(Deserialize)]
    struct RegisterResp {
        client_id: String,
        token: String,
    }

    let base = server.trim_end_matches('/');
    let ch: ChallengeResp = client
        .get(format!("{base}/v1/auth/challenge"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let nonce = solve_pow(&ch.challenge, ch.difficulty_bits);
    let reg: RegisterResp = client
        .post(format!("{base}/v1/auth/register"))
        .json(&serde_json::json!({ "challenge": ch.challenge, "nonce": nonce }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(SavedAuth {
        client_id: reg.client_id,
        token: reg.token,
    })
}

fn solve_pow(challenge: &str, difficulty_bits: u8) -> String {
    let mut nonce = 0u64;
    loop {
        let candidate = nonce.to_string();
        let mut hasher = Sha256::new();
        hasher.update(challenge.as_bytes());
        hasher.update(b":");
        hasher.update(candidate.as_bytes());
        let digest = hasher.finalize();
        if leading_zero_bits(&digest) >= difficulty_bits as u32 {
            return candidate;
        }
        nonce += 1;
    }
}

pub fn epoch_to_rfc3339(epoch_secs: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(epoch_secs, 0).map(|dt| dt.to_rfc3339())
}
