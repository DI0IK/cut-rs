use crate::config::default_state_dir;
use crate::models::{IntervalSubmission, UsageEntry, normalize_key};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Args)]
pub struct CommonClientArgs {
    /// Path to local state file for token & turn-diff tracking
    #[arg(long, env = "CUT_RS_STATE_FILE")]
    pub state_file: Option<PathBuf>,

    /// Print a compact status summary to stdout (off by default for silent logging)
    #[arg(long, default_value_t = false)]
    pub print_status: bool,

    /// Log errors to stderr instead of failing silently
    #[arg(long, default_value_t = false)]
    pub verbose: bool,

    /// Optional preexisting command to forward stdin to (runs concurrently, preserving its stdout)
    #[arg(long)]
    pub chain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketTracker {
    pub last_remaining_fraction: f64,
    pub last_reset_time: Option<String>,
    /// Locked expiry timestamp captured when the bucket first drops below 100%.
    /// Cleared when the bucket resets (remaining_fraction increases or returns to 100%).
    #[serde(default)]
    pub locked_reset_time: Option<String>,
    pub started_at: i64,
    #[serde(default)]
    pub pending_usage: HashMap<String, UsageEntry>,
}

impl BucketTracker {
    pub fn effective_reset_time(&self) -> Option<&str> {
        self.locked_reset_time
            .as_deref()
            .or(self.last_reset_time.as_deref())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CliToolState {
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub buckets: HashMap<String, BucketTracker>,
    #[serde(default)]
    pub pending_intervals: Vec<IntervalSubmission>,
    /// Incremental byte offsets for log-tailing
    #[serde(default)]
    pub file_offsets: HashMap<String, u64>,
    /// Learned mapping of bucket_id -> models observed in clean single-model drops
    #[serde(default)]
    pub solo_bucket_models: HashMap<String, std::collections::HashSet<String>>,
}

#[derive(Debug, Clone)]
pub struct QuotaSample {
    pub bucket_id: String,
    pub remaining_fraction: f64,
    pub reset_time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelDelta {
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached_in: u64,
    pub request_count: u64,
}

#[derive(Debug, Clone)]
pub struct TickInput {
    pub session_id: String,
    pub provider: String,
    pub tier: String,
    pub multi_deltas: Vec<ModelDelta>,
    pub skip_emit: bool,
    pub quota_precision: Option<f64>,
    pub quotas: Vec<QuotaSample>,
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub struct QuotaChangeRecord {
    pub tier: String,
    pub bucket_id: String,
    pub remaining_pct: f64,
    pub reset_time: Option<String>,
    pub recorded_at: i64,
}

pub fn state_dir() -> PathBuf {
    default_state_dir()
}

pub fn default_state_file_path(filename: &str) -> PathBuf {
    state_dir().join(filename)
}

pub fn default_webserver_pid_path() -> PathBuf {
    state_dir().join("webserver.pid")
}

pub fn default_webserver_log_path() -> PathBuf {
    state_dir().join("webserver.log")
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
        std::fs::write(path, &data)?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(())
}

pub fn save_state(path: &Path, state: &CliToolState) -> anyhow::Result<()> {
    atomic_write_json(path, state)
}

/// Acquires an exclusive advisory OS lock on `<state_path>.lock` so concurrent CLI
/// invocations never race on `file_offsets`, `pending_usage`, or `buckets`.
pub fn acquire_state_lock(state_path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = state_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut os = state_path.as_os_str().to_os_string();
    os.push(".lock");
    let lock_path = PathBuf::from(os);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    let _ = file.lock();
    Some(file)
}

pub async fn run_chained_command(cmd: &str, stdin_payload: &str, verbose: bool) {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };

    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    match command.spawn() {
        Ok(mut child) => {
            if let Some(mut child_stdin) = child.stdin.take() {
                let _ = child_stdin.write_all(stdin_payload.as_bytes()).await;
                let _ = child_stdin.shutdown().await;
            }
            let _ = child.wait().await;
        }
        Err(err) => {
            if verbose {
                eprintln!("cut-rs: failed to run chained statusLine command: {err}");
            }
        }
    }
}

/// Diffs `tick` against `state`, accumulates per-model token deltas, queues quota-drop intervals,
/// and returns any quota state changes to record in SQLite.
pub fn apply_token_and_quota_tick(
    state: &mut CliToolState,
    tick: &TickInput,
) -> (usize, Vec<QuotaChangeRecord>) {
    let provider = normalize_key(&tick.provider);
    let tier = normalize_key(&tick.tier);
    state.provider = Some(provider.clone());
    state.tier = Some(tier.clone());
    if state.session_id.as_deref() != Some(tick.session_id.as_str()) {
        state.session_id = Some(tick.session_id.clone());
    }

    let should_accumulate_in_bucket =
        |bucket_id: &str,
         model_name: &str,
         solo_map: &HashMap<String, std::collections::HashSet<String>>| {
            let model_has_solo_bucket = solo_map.values().any(|ms| ms.contains(model_name));
            if !model_has_solo_bucket {
                return true;
            }
            match solo_map.get(bucket_id) {
                Some(known_models) if !known_models.is_empty() => known_models.contains(model_name),
                _ => true,
            }
        };

    if tick.skip_emit {
        for tracker in state.buckets.values_mut() {
            tracker.pending_usage.clear();
            tracker.started_at = tick.timestamp;
        }
    } else {
        for d in &tick.multi_deltas {
            let model_name = normalize_key(&d.model);
            if !model_name.is_empty()
                && (d.tokens_in > 0 || d.tokens_out > 0 || d.tokens_cached_in > 0)
            {
                for (bucket_id, tracker) in state.buckets.iter_mut() {
                    if !should_accumulate_in_bucket(
                        bucket_id,
                        &model_name,
                        &state.solo_bucket_models,
                    ) {
                        continue;
                    }
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
                    entry.tokens_in = entry.tokens_in.saturating_add(d.tokens_in);
                    entry.tokens_out = entry.tokens_out.saturating_add(d.tokens_out);
                    entry.tokens_cached_in =
                        entry.tokens_cached_in.saturating_add(d.tokens_cached_in);
                    entry.request_count = entry
                        .request_count
                        .saturating_add(d.request_count.max(1).min(u32::MAX as u64) as u32);
                }
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
    let mut quota_changes = Vec::new();

    for q in &tick.quotas {
        if !q.remaining_fraction.is_finite() || !(0.0..=1.0).contains(&q.remaining_fraction) {
            continue;
        }
        let frac = q.remaining_fraction;
        let is_new_bucket = !state.buckets.contains_key(&q.bucket_id);
        let tracker = state
            .buckets
            .entry(q.bucket_id.clone())
            .or_insert_with(|| BucketTracker {
                last_remaining_fraction: frac,
                last_reset_time: q.reset_time.clone(),
                locked_reset_time: if frac < 1.0 - 1e-7 {
                    q.reset_time.clone()
                } else {
                    None
                },
                started_at: now,
                pending_usage: HashMap::new(),
            });

        if is_new_bucket {
            if frac < 1.0 - 1e-7 {
                quota_changes.push(QuotaChangeRecord {
                    tier: tier.clone(),
                    bucket_id: q.bucket_id.clone(),
                    remaining_pct: (frac * 100.0).clamp(0.0, 100.0),
                    reset_time: tracker.effective_reset_time().map(str::to_string),
                    recorded_at: now,
                });
            }
            continue;
        }

        let prev_effective_reset = tracker.effective_reset_time().map(str::to_string);
        let reset_jumped = match (prev_effective_reset.as_deref(), q.reset_time.as_deref()) {
            (Some(prev), Some(curr)) if curr != prev => {
                match (
                    DateTime::parse_from_rfc3339(prev),
                    DateTime::parse_from_rfc3339(curr),
                ) {
                    (Ok(p_dt), Ok(c_dt)) => c_dt.timestamp() > p_dt.timestamp() + 60,
                    _ => frac >= tracker.last_remaining_fraction,
                }
            }
            _ => false,
        };

        let is_genuine_reset = reset_jumped
            || (frac >= 1.0 - 1e-4 && tracker.last_remaining_fraction < 1.0 - 1e-3)
            || frac > tracker.last_remaining_fraction + 0.15;

        if is_genuine_reset {
            let frac_changed = (frac - tracker.last_remaining_fraction).abs() > 1e-7;
            tracker.last_remaining_fraction = frac;
            tracker.last_reset_time = q.reset_time.clone();
            tracker.locked_reset_time = if frac < 1.0 - 1e-7 {
                q.reset_time.clone()
            } else {
                None
            };
            tracker.started_at = now;
            tracker.pending_usage.clear();
            if frac_changed && frac < 1.0 - 1e-7 {
                quota_changes.push(QuotaChangeRecord {
                    tier: tier.clone(),
                    bucket_id: q.bucket_id.clone(),
                    remaining_pct: (frac * 100.0).clamp(0.0, 100.0),
                    reset_time: tracker.effective_reset_time().map(str::to_string),
                    recorded_at: now,
                });
            }
            continue;
        }

        // Ignore small upward bounces from stale/cached quota responses without raising
        // the cycle's low-water mark or clearing pending_usage.
        if frac > tracker.last_remaining_fraction + 1e-6 {
            continue;
        }

        // While at 100%, the reset timestamp moves forward with each refresh.
        // Once it drops below 100%, lock the expiry timestamp until the bucket resets.
        if frac >= 1.0 - 1e-7 {
            tracker.last_reset_time = q.reset_time.clone();
            tracker.locked_reset_time = None;
        } else if tracker.locked_reset_time.is_none() {
            tracker.locked_reset_time = q.reset_time.clone().or_else(|| tracker.last_reset_time.clone());
        }

        if frac < tracker.last_remaining_fraction - 1e-7 {
            let quota_before_pct = tracker.last_remaining_fraction * 100.0;
            let quota_after_pct = frac * 100.0;
            let duration = now - tracker.started_at;
            let usage: Vec<UsageEntry> = tracker.pending_usage.values().cloned().collect();
            let solo_model = if usage.len() == 1 {
                Some(usage[0].model.clone())
            } else {
                None
            };
            let total_tokens: u64 = usage
                .iter()
                .map(|u| u.tokens_in + u.tokens_out + u.tokens_cached_in)
                .sum();

            if !tick.skip_emit && total_tokens > 0 && duration > 0 && duration <= 1800 {
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
                    quota_precision: tick.quota_precision.unwrap_or(0.01),
                    usage,
                });
                emitted += 1;
            }

            tracker.last_remaining_fraction = frac;
            tracker.last_reset_time = q.reset_time.clone();
            if tracker.locked_reset_time.is_none() {
                tracker.locked_reset_time = q.reset_time.clone();
            }
            tracker.started_at = now;
            tracker.pending_usage.clear();
            if let Some(m) = solo_model {
                state
                    .solo_bucket_models
                    .entry(q.bucket_id.clone())
                    .or_default()
                    .insert(m);
            }

            quota_changes.push(QuotaChangeRecord {
                tier: tier.clone(),
                bucket_id: q.bucket_id.clone(),
                remaining_pct: quota_after_pct.clamp(0.0, 100.0),
                reset_time: tracker.effective_reset_time().map(str::to_string),
                recorded_at: now,
            });
        }
    }

    if state.pending_intervals.len() > 500 {
        let excess = state.pending_intervals.len() - 500;
        state.pending_intervals.drain(0..excess);
    }

    (emitted, quota_changes)
}

pub fn epoch_to_rfc3339(epoch_secs: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(epoch_secs, 0).map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quota_bounce_does_not_reemit_or_clear_pending_usage() {
        let mut state = CliToolState::default();
        let reset = Some("2026-11-21T00:02:05Z".to_string());

        let make_tick = |ts: i64, frac: f64, tok_in: u64, reset_time: Option<String>| TickInput {
            session_id: "s1".to_string(),
            provider: "antigravity".to_string(),
            tier: "pro".to_string(),
            multi_deltas: if tok_in > 0 {
                vec![ModelDelta {
                    model: "gemini 4".to_string(),
                    tokens_in: tok_in,
                    tokens_out: 100,
                    tokens_cached_in: 0,
                    request_count: 1,
                }]
            } else {
                vec![]
            },
            skip_emit: false,
            quota_precision: Some(0.0001),
            quotas: vec![QuotaSample {
                bucket_id: "gemini-5h".to_string(),
                remaining_fraction: frac,
                reset_time,
            }],
            timestamp: ts,
        };

        // Initial baseline at 0.3453
        let (e0, _) = apply_token_and_quota_tick(&mut state, &make_tick(100, 0.3453, 0, reset.clone()));
        assert_eq!(e0, 0);

        // Drop to 0.3412 -> emits 1 interval
        let (e1, _) = apply_token_and_quota_tick(&mut state, &make_tick(110, 0.3412, 10_000, reset.clone()));
        assert_eq!(e1, 1);

        // Bounce back up to 0.3453 with same reset_time -> ignored, pending_usage preserved
        let (e2, _) = apply_token_and_quota_tick(&mut state, &make_tick(120, 0.3453, 5_000, reset.clone()));
        assert_eq!(e2, 0);
        assert_eq!(
            state.buckets["gemini-5h"].pending_usage["gemini 4"].tokens_in,
            5_000
        );

        // Return to 0.3412 -> should NOT re-emit duplicate 0.3453 -> 0.3412 interval
        let (e3, _) = apply_token_and_quota_tick(&mut state, &make_tick(130, 0.3412, 5_000, reset.clone()));
        assert_eq!(e3, 0);
        assert_eq!(
            state.buckets["gemini-5h"].pending_usage["gemini 4"].tokens_in,
            10_000
        );

        // Genuine new drop to 0.3350 -> emits 1 interval with all accumulated 10_000 tokens
        let (e4, _) = apply_token_and_quota_tick(&mut state, &make_tick(140, 0.3350, 0, reset));
        assert_eq!(e4, 1);
        assert_eq!(state.pending_intervals.len(), 2);
        assert_eq!(state.pending_intervals[1].usage[0].tokens_in, 10_000);

        // Genuine reset to 1.0000 -> resets low-water mark
        let (e5, _) = apply_token_and_quota_tick(
            &mut state,
            &make_tick(150, 1.0, 0, Some("2026-11-21T05:02:05Z".to_string())),
        );
        assert_eq!(e5, 0);
        assert!((state.buckets["gemini-5h"].last_remaining_fraction - 1.0).abs() < 1e-6);
    }
}
