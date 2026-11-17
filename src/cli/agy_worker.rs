use crate::cli::common::{
    CliToolState, CommonClientArgs, ModelDelta, QuotaSample, TickInput, acquire_state_lock,
    apply_token_and_quota_tick, default_state_file_path, flush_pending_intervals, load_state,
    save_state,
};
use chrono::DateTime;
use clap::Args;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Args)]
pub struct AgyWorkerArgs {
    #[command(flatten)]
    pub common: CommonClientArgs,

    /// Poll interval in milliseconds when running as a persistent daemon
    #[arg(long, default_value_t = 5000)]
    pub poll_ms: u64,

    /// Run a single pass and exit instead of looping
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Exit after this many seconds with no queued snapshots or pending intervals (0 = run forever)
    #[arg(long, default_value_t = 0)]
    pub idle_exit_secs: u64,
}

pub fn default_antigravity_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("antigravity-cli"))
}

pub fn is_worker_running(state_path: &Path) -> bool {
    let lock_path = state_path.with_extension("worker.lock");
    let Ok(file) = OpenOptions::new().read(true).write(true).open(lock_path) else {
        return false;
    };
    file.try_lock().is_err()
}

fn acquire_worker_lock(state_path: &Path) -> Option<File> {
    if let Some(parent) = state_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lock_path = state_path.with_extension("worker.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    if file.try_lock().is_err() {
        return None;
    }
    Some(file)
}

pub async fn run_agy_worker(args: AgyWorkerArgs) {
    let state_path = args
        .common
        .state_file
        .clone()
        .unwrap_or_else(|| default_state_file_path("agy-cli-state.json"));

    let Some(_worker_lock) = acquire_worker_lock(&state_path) else {
        if args.common.verbose {
            eprintln!("agy-worker: another instance is already running");
        }
        return;
    };

    let Some(agy_root) = default_antigravity_root() else {
        if args.common.verbose {
            eprintln!("agy-worker: could not resolve home directory");
        }
        return;
    };

    let poll = Duration::from_millis(args.poll_ms.max(500));
    let mut last_active = std::time::Instant::now();
    loop {
        match process_agy_worker_pass(
            &state_path,
            &agy_root,
            &args.common.server,
            args.common.timeout_ms.max(5000),
            args.common.verbose,
        )
        .await
        {
            Ok(did_work) => {
                if did_work {
                    last_active = std::time::Instant::now();
                }
            }
            Err(err) => {
                if args.common.verbose {
                    eprintln!("agy-worker pass error: {err:#}");
                }
            }
        }

        if args.once {
            break;
        }
        if args.idle_exit_secs > 0
            && last_active.elapsed() >= Duration::from_secs(args.idle_exit_secs)
        {
            break;
        }
        tokio::time::sleep(poll).await;
    }
}

pub async fn process_agy_worker_pass(
    state_path: &Path,
    agy_root: &Path,
    server: &str,
    timeout_ms: u64,
    verbose: bool,
) -> anyhow::Result<bool> {
    let brain_dir = agy_root.join("brain");
    let conv_dir = agy_root.join("conversations");
    let summaries_db = agy_root.join("conversation_summaries.db");

    let (mut state, mut snapshots) = {
        let _lock = acquire_state_lock(state_path);
        let mut s = load_state(state_path).unwrap_or_default();
        if s.quota_snapshots.is_empty() {
            let has_pending = !s.pending_intervals.is_empty();
            drop(_lock);
            if has_pending {
                flush_pending_intervals(&mut s, server, timeout_ms).await;
                let _lock2 = acquire_state_lock(state_path);
                let mut latest = load_state(state_path).unwrap_or_default();
                latest.pending_intervals = s.pending_intervals;
                save_state(state_path, &latest)?;
                return Ok(true);
            }
            return Ok(false);
        }
        let drained = std::mem::take(&mut s.quota_snapshots);
        save_state(state_path, &s)?;
        (s, drained)
    };

    snapshots.sort_by_key(|snap| snap.timestamp);

    let all_transcripts = discover_all_brain_transcripts(&brain_dir);
    let mut model_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut parent_cache: Option<HashMap<String, String>> = None;

    for snap in snapshots {
        let min_ts_if_new = state
            .buckets
            .values()
            .map(|b| b.started_at)
            .min()
            .unwrap_or(snap.timestamp);

        let mut by_model: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
        let mut unattributed = false;

        for path in &all_transcripts {
            let Some(conv_id) = extract_conv_id_from_transcript(path) else {
                continue;
            };
            let Some((di, do_, dc, reqs)) =
                read_transcript_deltas_until(path, &mut state, snap.timestamp, min_ts_if_new)
            else {
                continue;
            };
            if di == 0 && do_ == 0 && dc == 0 {
                continue;
            }

            let model_opt = resolve_model_for_conv(
                &conv_id,
                &conv_dir,
                &summaries_db,
                &mut model_cache,
                &mut parent_cache,
            )
            .await;

            if let Some(model_name) = model_opt {
                let e = by_model.entry(model_name).or_insert((0, 0, 0, 0));
                e.0 = e.0.saturating_add(di);
                e.1 = e.1.saturating_add(do_);
                e.2 = e.2.saturating_add(dc);
                e.3 = e.3.saturating_add(reqs);
            } else {
                unattributed = true;
                if verbose {
                    eprintln!("agy-worker: could not resolve model for conv {conv_id}");
                }
            }
        }

        let multi_deltas = Some(
            by_model
                .into_iter()
                .map(
                    |(model, (tokens_in, tokens_out, tokens_cached_in, request_count))| {
                        ModelDelta {
                            model,
                            tokens_in,
                            tokens_out,
                            tokens_cached_in,
                            request_count,
                        }
                    },
                )
                .collect(),
        );

        let quotas = snap
            .quotas
            .into_iter()
            .map(|q| QuotaSample {
                bucket_id: q.bucket_id,
                remaining_fraction: q.remaining_fraction,
                reset_time: q.reset_time,
            })
            .collect();

        let tick = TickInput {
            session_id: "agy-worker".to_string(),
            provider: snap.provider,
            tier: snap.tier,
            model: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            current_cached_tokens: 0,
            total_cached_tokens: None,
            precomputed_delta: None,
            multi_deltas,
            skip_emit: unattributed,
            quota_precision: Some(0.0001),
            quotas,
            timestamp: snap.timestamp,
        };

        apply_token_and_quota_tick(&mut state, &tick);
    }

    // Flush pending intervals outside the state lock
    flush_pending_intervals(&mut state, server, timeout_ms).await;

    // Merge updated worker fields back under lock (preserving any newly queued quota_snapshots)
    let _lock = acquire_state_lock(state_path);
    let mut latest = load_state(state_path).unwrap_or_default();
    latest.provider = state.provider;
    latest.tier = state.tier;
    latest.buckets = state.buckets;
    latest.file_offsets = state.file_offsets;
    latest.pending_intervals = state.pending_intervals;
    save_state(state_path, &latest)?;

    Ok(true)
}

pub fn discover_all_brain_transcripts(brain_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(brain_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry
            .path()
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        if p.is_file() {
            out.push(p);
        }
    }
    out.sort();
    out
}

pub fn extract_conv_id_from_transcript(transcript_path: &Path) -> Option<String> {
    transcript_path
        .parent()? // logs
        .parent()? // .system_generated
        .parent()? // <conv_id>
        .file_name()?
        .to_str()
        .map(|s| s.to_string())
}

/// Reads complete `PLANNER_RESPONSE` lines from `path` starting at `state.file_offsets[path]`
/// whose `created_at <= until_ts`. Stops advancing the offset at the first line with
/// `created_at > until_ts` so later turns are attributed to the next quota window.
/// If `path` has never been seen before, skips lines with `created_at <= min_ts_if_new`.
pub fn read_transcript_deltas_until(
    path: &Path,
    state: &mut CliToolState,
    until_ts: i64,
    min_ts_if_new: i64,
) -> Option<(u64, u64, u64, u64)> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let key = path.to_string_lossy().into_owned();

    let is_new = !state.file_offsets.contains_key(&key);
    let mut offset = state.file_offsets.get(&key).copied().unwrap_or(0);
    if offset > file_len {
        offset = 0;
    }
    if offset == file_len {
        state.file_offsets.insert(key, offset);
        return Some((0, 0, 0, 0));
    }

    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;

    let mut tok_in = 0u64;
    let mut tok_out = 0u64;
    let mut tok_cached = 0u64;
    let mut reqs = 0u64;
    let mut consumed = 0usize;

    for chunk in buf.split_inclusive(|&b| b == b'\n') {
        if !chunk.ends_with(b"\n") {
            break; // Incomplete trailing line
        }
        let trimmed = std::str::from_utf8(chunk).ok().map(str::trim).unwrap_or("");
        if trimmed.is_empty() {
            consumed += chunk.len();
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            consumed += chunk.len();
            continue;
        };

        if let Some(ts_str) = val.get("created_at").and_then(|v| v.as_str())
            && let Ok(dt) = DateTime::parse_from_rfc3339(ts_str)
        {
            let line_ts = dt.timestamp();
            if line_ts > until_ts {
                break; // Belongs to a future quota window
            }
            if is_new && line_ts <= min_ts_if_new {
                consumed += chunk.len();
                continue;
            }
        }

        if val.get("source").and_then(|v| v.as_str()) == Some("MODEL")
            && val.get("type").and_then(|v| v.as_str()) == Some("PLANNER_RESPONSE")
        {
            tok_in = tok_in.saturating_add(
                val.get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            );
            tok_out = tok_out.saturating_add(
                val.get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            );
            tok_cached = tok_cached.saturating_add(
                val.get("cache_read_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            );
            reqs = reqs.saturating_add(1);
        }

        consumed += chunk.len();
    }

    state.file_offsets.insert(key, offset + consumed as u64);
    Some((tok_in, tok_out, tok_cached, reqs))
}

async fn resolve_model_for_conv(
    conv_id: &str,
    conv_dir: &Path,
    summaries_db: &Path,
    model_cache: &mut HashMap<String, Option<String>>,
    parent_cache: &mut Option<HashMap<String, String>>,
) -> Option<String> {
    if let Some(cached) = model_cache.get(conv_id) {
        return cached.clone();
    }

    let db_path = conv_dir.join(format!("{conv_id}.db"));
    let mut resolved = query_model_from_conv_db(&db_path).await;

    if resolved.is_none() {
        if parent_cache.is_none() {
            *parent_cache = Some(load_conversation_parents(summaries_db).await);
        }
        if let Some(parents) = parent_cache.as_ref() {
            let mut cur = conv_id;
            for _ in 0..8 {
                let Some(parent_id) = parents.get(cur) else {
                    break;
                };
                if let Some(m) = model_cache.get(parent_id).and_then(|v| v.clone()) {
                    resolved = Some(m);
                    break;
                }
                let p_db = conv_dir.join(format!("{parent_id}.db"));
                if let Some(m) = query_model_from_conv_db(&p_db).await {
                    resolved = Some(m);
                    break;
                }
                cur = parent_id.as_str();
            }
        }
    }

    model_cache.insert(conv_id.to_string(), resolved.clone());
    resolved
}

async fn query_model_from_conv_db(db_path: &Path) -> Option<String> {
    if !db_path.is_file() {
        return None;
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .ok()?
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .ok()?;

    let rows = sqlx::query("SELECT data FROM gen_metadata ORDER BY idx DESC LIMIT 8")
        .fetch_all(&pool)
        .await
        .ok()?;
    pool.close().await;

    for row in rows {
        let blob: Vec<u8> = row.try_get("data").ok()?;
        if let Some(model) = extract_model_from_gen_metadata_blob(&blob) {
            return Some(model);
        }
    }
    None
}

async fn load_conversation_parents(summaries_db: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if !summaries_db.is_file() {
        return map;
    }
    let Ok(opts) = SqliteConnectOptions::from_str(&format!("sqlite://{}", summaries_db.display()))
    else {
        return map;
    };
    let Ok(pool) = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts.read_only(true))
        .await
    else {
        return map;
    };

    if let Ok(rows) =
        sqlx::query("SELECT conversation_id, parent_id FROM conversation_parents")
            .fetch_all(&pool)
            .await
    {
        for row in rows {
            if let (Ok(cid), Ok(pid)) = (
                row.try_get::<String, _>("conversation_id"),
                row.try_get::<String, _>("parent_id"),
            ) {
                map.insert(cid, pid);
            }
        }
    }
    pool.close().await;
    map
}

/// Decodes top-level protobuf wire fields from `gen_metadata.data` and extracts
/// field 21 (length-delimited UTF-8 model display string, e.g. `"Gemini 4 Hydrogen (Medium)"`).
pub fn extract_model_from_gen_metadata_blob(blob: &[u8]) -> Option<String> {
    let mut i = 0usize;
    while i < blob.len() {
        let (tag, next) = read_varint(blob, i)?;
        i = next;
        let field_num = tag >> 3;
        let wire_type = tag & 0x7;
        match wire_type {
            0 => {
                let (_, n) = read_varint(blob, i)?;
                i = n;
            }
            1 => {
                i = i.checked_add(8)?;
                if i > blob.len() {
                    return None;
                }
            }
            2 => {
                let (len, n) = read_varint(blob, i)?;
                i = n;
                let end = i.checked_add(len as usize)?;
                if end > blob.len() {
                    return None;
                }
                if field_num == 21
                    && let Ok(s) = std::str::from_utf8(&blob[i..end])
                {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
                i = end;
            }
            5 => {
                i = i.checked_add(4)?;
                if i > blob.len() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

fn read_varint(buf: &[u8], mut idx: usize) -> Option<(u64, usize)> {
    let mut val = 0u64;
    let mut shift = 0u32;
    while idx < buf.len() && shift < 64 {
        let b = buf[idx];
        idx += 1;
        val |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0 {
            return Some((val, idx));
        }
        shift += 7;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::common::{QuotaSnapshotRecord, StoredQuotaSample};
    use tempfile::tempdir;

    #[test]
    fn test_extract_model_from_gen_metadata_blob_field_21() {
        // Field 1 (varint = 42), then Field 21 (wire type 2 = 0xAA 0x01) "Gemini 4 Hydrogen (Medium)"
        let mut blob = vec![0x08, 42, 0xAA, 0x01];
        let model = b"Gemini 4 Hydrogen (Medium)";
        blob.push(model.len() as u8);
        blob.extend_from_slice(model);
        assert_eq!(
            extract_model_from_gen_metadata_blob(&blob).as_deref(),
            Some("Gemini 4 Hydrogen (Medium)")
        );
    }

    #[test]
    fn test_read_transcript_deltas_until_respects_timestamp_boundary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let content = concat!(
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:00:00Z","input_tokens":100,"output_tokens":10,"cache_read_tokens":1000}"#, "\n",
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:01:00Z","input_tokens":200,"output_tokens":20,"cache_read_tokens":2000}"#, "\n",
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:03:00Z","input_tokens":500,"output_tokens":50,"cache_read_tokens":5000}"#, "\n",
        );
        std::fs::write(&path, content).unwrap();

        let t0 = DateTime::parse_from_rfc3339("2026-11-17T19:00:30Z")
            .unwrap()
            .timestamp();
        let t1 = DateTime::parse_from_rfc3339("2026-11-17T19:02:00Z")
            .unwrap()
            .timestamp();
        let t2 = DateTime::parse_from_rfc3339("2026-11-17T19:04:00Z")
            .unwrap()
            .timestamp();

        let mut state = CliToolState::default();
        // First pass up to t1 (is_new=true, min_ts_if_new=t0): skips 19:00:00, counts 19:01:00, stops before 19:03:00
        let d1 = read_transcript_deltas_until(&path, &mut state, t1, t0).unwrap();
        assert_eq!(d1, (200, 20, 2000, 1));

        // Second pass up to t2: picks up 19:03:00
        let d2 = read_transcript_deltas_until(&path, &mut state, t2, t0).unwrap();
        assert_eq!(d2, (500, 50, 5000, 1));
    }

    #[tokio::test]
    async fn test_process_agy_worker_pass_reads_conv_db_and_emits_interval() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("agy-cli-state.json");
        let agy_root = dir.path().join("agy");
        let conv_id = "11111111-1111-1111-1111-111111111111";
        let logs_dir = agy_root
            .join("brain")
            .join(conv_id)
            .join(".system_generated")
            .join("logs");
        let conv_dir = agy_root.join("conversations");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::create_dir_all(&conv_dir).unwrap();

        // Create conversation DB with gen_metadata field 21
        let db_path = conv_dir.join(format!("{conv_id}.db"));
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        sqlx::query("CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)")
            .execute(&pool)
            .await
            .unwrap();
        let mut blob = vec![0xAA, 0x01];
        let model = b"Gemini 4 Hydrogen (Medium)";
        blob.push(model.len() as u8);
        blob.extend_from_slice(model);
        sqlx::query("INSERT INTO gen_metadata (idx, data) VALUES (0, ?)")
            .bind(&blob)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let t0 = DateTime::parse_from_rfc3339("2026-11-17T19:00:00Z")
            .unwrap()
            .timestamp();
        let t1 = DateTime::parse_from_rfc3339("2026-11-17T19:02:00Z")
            .unwrap()
            .timestamp();

        let transcript = logs_dir.join("transcript.jsonl");
        let lines = concat!(
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:01:00Z","input_tokens":1200,"output_tokens":340,"cache_read_tokens":50000}"#, "\n",
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:03:00Z","input_tokens":999,"output_tokens":99,"cache_read_tokens":9999}"#, "\n",
        );
        std::fs::write(&transcript, lines).unwrap();

        let initial = CliToolState {
            quota_snapshots: vec![
                QuotaSnapshotRecord {
                    timestamp: t0,
                    provider: "antigravity".to_string(),
                    tier: "google ai pro".to_string(),
                    quotas: vec![StoredQuotaSample {
                        bucket_id: "gemini-5h".to_string(),
                        remaining_fraction: 0.90,
                        reset_time: None,
                    }],
                },
                QuotaSnapshotRecord {
                    timestamp: t1,
                    provider: "antigravity".to_string(),
                    tier: "google ai pro".to_string(),
                    quotas: vec![StoredQuotaSample {
                        bucket_id: "gemini-5h".to_string(),
                        remaining_fraction: 0.895,
                        reset_time: None,
                    }],
                },
            ],
            ..Default::default()
        };
        save_state(&state_path, &initial).unwrap();

        // Use unreachable server so intervals stay in pending_intervals for assertion
        process_agy_worker_pass(&state_path, &agy_root, "http://127.0.0.1:9", 500, false)
            .await
            .unwrap();

        let after = load_state(&state_path).unwrap();
        assert!(after.quota_snapshots.is_empty());
        assert_eq!(after.pending_intervals.len(), 1);
        let iv = &after.pending_intervals[0];
        assert!((iv.quota_precision - 0.0001).abs() < 1e-9);
        assert_eq!(iv.usage.len(), 1);
        assert_eq!(iv.usage[0].model, "gemini 4 hydrogen (medium)");
        assert_eq!(iv.usage[0].tokens_in, 1200);
        assert_eq!(iv.usage[0].tokens_out, 340);
        assert_eq!(iv.usage[0].tokens_cached_in, 50000);
    }

    #[tokio::test]
    async fn test_agy_worker_handles_quota_resets_without_poisoned_intervals() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("agy-cli-state.json");
        let agy_root = dir.path().join("agy");
        let conv_id = "22222222-2222-2222-2222-222222222222";
        let logs_dir = agy_root
            .join("brain")
            .join(conv_id)
            .join(".system_generated")
            .join("logs");
        let conv_dir = agy_root.join("conversations");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::create_dir_all(&conv_dir).unwrap();

        let db_path = conv_dir.join(format!("{conv_id}.db"));
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().connect_with(opts).await.unwrap();
        sqlx::query("CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)")
            .execute(&pool)
            .await
            .unwrap();
        let mut blob = vec![0xAA, 0x01];
        let model = b"Gemini 4 Hydrogen (Medium)";
        blob.push(model.len() as u8);
        blob.extend_from_slice(model);
        sqlx::query("INSERT INTO gen_metadata (idx, data) VALUES (0, ?)")
            .bind(&blob)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let t0 = DateTime::parse_from_rfc3339("2026-11-17T19:00:00Z")
            .unwrap()
            .timestamp();
        let t1 = t0 + 60;
        let t2 = t0 + 120;
        let t3 = t0 + 180;

        let transcript = logs_dir.join("transcript.jsonl");
        let lines = concat!(
            // Pre-reset turn (should be discarded on reset)
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:00:30Z","input_tokens":5000,"output_tokens":500,"cache_read_tokens":50000}"#, "\n",
            // Post-reset turn (should be attributed to [t1, t2] drop, or cleared if reset happened at t1)
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:01:30Z","input_tokens":100,"output_tokens":10,"cache_read_tokens":1000}"#, "\n",
            // Post-second-reset turn (attributed to [t2, t3])
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:02:30Z","input_tokens":200,"output_tokens":20,"cache_read_tokens":2000}"#, "\n",
        );
        std::fs::write(&transcript, lines).unwrap();

        let initial = CliToolState {
            quota_snapshots: vec![
                // Baseline at 0.99
                QuotaSnapshotRecord {
                    timestamp: t0,
                    provider: "antigravity".to_string(),
                    tier: "google ai pro".to_string(),
                    quotas: vec![StoredQuotaSample {
                        bucket_id: "gemini-5h".to_string(),
                        remaining_fraction: 0.99,
                        reset_time: Some("2026-11-17T19:01:00Z".to_string()),
                    }],
                },
                // Window resets by 5h AND user immediately burns down to 0.98 (< 0.99!)
                QuotaSnapshotRecord {
                    timestamp: t1,
                    provider: "antigravity".to_string(),
                    tier: "google ai pro".to_string(),
                    quotas: vec![StoredQuotaSample {
                        bucket_id: "gemini-5h".to_string(),
                        remaining_fraction: 0.98,
                        reset_time: Some("2026-11-18T00:01:00Z".to_string()),
                    }],
                },
                // Normal fraction increase reset (0.98 -> 1.00)
                QuotaSnapshotRecord {
                    timestamp: t2,
                    provider: "antigravity".to_string(),
                    tier: "google ai pro".to_string(),
                    quotas: vec![StoredQuotaSample {
                        bucket_id: "gemini-5h".to_string(),
                        remaining_fraction: 1.00,
                        reset_time: Some("2026-11-18T05:01:00Z".to_string()),
                    }],
                },
                // Normal drop after reset (1.00 -> 0.995)
                QuotaSnapshotRecord {
                    timestamp: t3,
                    provider: "antigravity".to_string(),
                    tier: "google ai pro".to_string(),
                    quotas: vec![StoredQuotaSample {
                        bucket_id: "gemini-5h".to_string(),
                        remaining_fraction: 0.995,
                        reset_time: Some("2026-11-18T05:01:00Z".to_string()),
                    }],
                },
            ],
            ..Default::default()
        };
        save_state(&state_path, &initial).unwrap();

        process_agy_worker_pass(&state_path, &agy_root, "http://127.0.0.1:9", 500, false)
            .await
            .unwrap();

        let after = load_state(&state_path).unwrap();
        // Only the clean post-reset [t2, t3] interval should be emitted
        assert_eq!(after.pending_intervals.len(), 1);
        let iv = &after.pending_intervals[0];
        assert!((iv.quota_before_pct - 100.0).abs() < 1e-6);
        assert!((iv.quota_after_pct - 99.5).abs() < 1e-6);
        assert_eq!(iv.usage[0].tokens_in, 200);
        assert_eq!(iv.usage[0].tokens_out, 20);
        assert_eq!(iv.usage[0].tokens_cached_in, 2000);
    }
}
