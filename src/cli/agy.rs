use crate::cli::common::{
    CommonClientArgs, ModelDelta, QuotaSample, TickInput, acquire_state_lock,
    apply_token_and_quota_tick, default_state_file_path, load_state, run_chained_command,
    save_state,
};
use crate::config::default_database_url;
use crate::db::sqlite::SqliteRepository;
use crate::db::{NewInterval, TelemetryRepository};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::Deserialize;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Args)]
pub struct AgyCliArgs {
    #[command(flatten)]
    pub common: CommonClientArgs,

    /// Optional SQLite database URL override
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: Option<String>,
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

pub fn default_antigravity_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("antigravity-cli"))
}

pub async fn run_agy_cli(args: AgyCliArgs) {
    let mut stdin_buf = String::new();
    if std::io::stdin().read_to_string(&mut stdin_buf).is_err() {
        return;
    }

    let record_fut = async {
        let state_path = args
            .common
            .state_file
            .clone()
            .unwrap_or_else(|| default_state_file_path("agy-cli-state.json"));
        let db_url = args
            .database_url
            .clone()
            .unwrap_or_else(default_database_url);

        if let Some(agy_root) = default_antigravity_root()
            && let Err(err) = process_agy_status_line(
                &state_path,
                &agy_root,
                &db_url,
                &stdin_buf,
                args.common.print_status,
                args.common.verbose,
            )
            .await
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

pub async fn process_agy_status_line(
    state_path: &Path,
    agy_root: &Path,
    database_url: &str,
    raw_stdin: &str,
    print_status: bool,
    verbose: bool,
) -> anyhow::Result<()> {
    let payload: StatusLinePayload = serde_json::from_str(raw_stdin)?;
    let now = Utc::now().timestamp();

    let mut quotas: Vec<QuotaSample> = payload
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
            Some(QuotaSample {
                bucket_id,
                remaining_fraction,
                reset_time: q.reset_time,
            })
        })
        .collect();

    if quotas.is_empty() {
        return Ok(());
    }
    quotas.sort_by(|a, b| a.bucket_id.cmp(&b.bucket_id));

    let _lock = acquire_state_lock(state_path);
    let mut state = load_state(state_path).unwrap_or_default();

    if let Some(sid) = payload.session_id.or(payload.conversation_id) {
        state.session_id = Some(sid);
    }

    let provider = payload.product.unwrap_or_else(|| "antigravity".to_string());
    let tier = payload
        .plan_tier
        .filter(|t| !t.trim().is_empty() && !t.eq_ignore_ascii_case("unknown"))
        .or_else(|| {
            state
                .tier
                .clone()
                .filter(|t| !t.trim().is_empty() && !t.eq_ignore_ascii_case("unknown"))
        })
        .unwrap_or_else(|| "unknown".to_string());

    let brain_dir = agy_root.join("brain");
    let conv_dir = agy_root.join("conversations");
    let summaries_db = agy_root.join("conversation_summaries.db");

    let min_ts_if_new = state
        .buckets
        .values()
        .map(|b| b.started_at)
        .min()
        .unwrap_or(now);

    let all_transcripts = discover_all_brain_transcripts(&brain_dir);
    let mut model_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut parent_cache: Option<HashMap<String, String>> = None;
    let mut by_model: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
    let mut unattributed = false;

    for path in &all_transcripts {
        let Some(conv_id) = extract_conv_id_from_transcript(path) else {
            continue;
        };
        let Some((di, do_, dc, reqs)) =
            read_transcript_deltas_until(path, &mut state, now, min_ts_if_new)
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
                eprintln!("agy-cli: could not resolve model for conv {conv_id}");
            }
        }
    }

    let multi_deltas: Vec<ModelDelta> = by_model
        .into_iter()
        .map(
            |(model, (tokens_in, tokens_out, tokens_cached_in, request_count))| ModelDelta {
                model,
                tokens_in,
                tokens_out,
                tokens_cached_in,
                request_count,
            },
        )
        .collect();

    let tick = TickInput {
        session_id: state
            .session_id
            .clone()
            .unwrap_or_else(|| "agy-cli".to_string()),
        provider,
        tier,
        multi_deltas,
        skip_emit: unattributed,
        quota_precision: Some(0.0001),
        quotas,
        timestamp: now,
    };

    let (emitted, quota_changes) = apply_token_and_quota_tick(&mut state, &tick);

    if !state.pending_intervals.is_empty() || !quota_changes.is_empty() {
        let repo = SqliteRepository::connect_and_migrate(database_url).await?;

        let mut flushed = 0usize;
        for iv in &state.pending_intervals {
            let delta_pct = (iv.quota_before_pct - iv.quota_after_pct).max(0.0);
            let new_iv = NewInterval {
                idempotency_key: iv.idempotency_key.clone(),
                tier: iv.tier.clone(),
                window_type: iv.window_type.clone(),
                started_at: iv.started_at,
                ended_at: iv.ended_at,
                delta_pct,
                quota_precision: iv.quota_precision,
                usage: iv.usage.clone(),
                ingested_at: now,
            };
            repo.insert_interval_if_new(&new_iv).await?;
            flushed += 1;
        }
        state.pending_intervals.drain(0..flushed);

        for qc in &quota_changes {
            repo.insert_quota_snapshot(
                &qc.tier,
                &qc.bucket_id,
                qc.remaining_pct,
                qc.reset_time.as_deref(),
                qc.recorded_at,
            )
            .await?;
        }
    }

    save_state(state_path, &state)?;

    if print_status {
        println!(
            "cut-rs agy-cli: recorded_intervals={emitted} quota_updates={}",
            quota_changes.len()
        );
    }
    Ok(())
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
        .parent()?
        .parent()?
        .parent()?
        .file_name()?
        .to_str()
        .map(|s| s.to_string())
}

pub fn read_transcript_deltas_until(
    path: &Path,
    state: &mut crate::cli::common::CliToolState,
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
            break;
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
                break;
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

    if let Ok(rows) = sqlx::query("SELECT conversation_id, parent_id FROM conversation_parents")
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

pub fn extract_model_from_gen_metadata_blob(blob: &[u8]) -> Option<String> {
    if let Some(m) = extract_field_21_utf8(blob) {
        return Some(m);
    }
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
                if field_num == 1
                    && let Some(m) = extract_field_21_utf8(&blob[i..end])
                {
                    return Some(m);
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

fn extract_field_21_utf8(blob: &[u8]) -> Option<String> {
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
    use crate::cli::common::CliToolState;
    use tempfile::tempdir;

    #[test]
    fn test_extract_model_from_gen_metadata_blob_field_21() {
        let mut blob = vec![0x08, 42, 0xAA, 0x01];
        let model = b"Gemini 4 Hydrogen (Medium)";
        blob.push(model.len() as u8);
        blob.extend_from_slice(model);
        assert_eq!(
            extract_model_from_gen_metadata_blob(&blob).as_deref(),
            Some("Gemini 4 Hydrogen (Medium)")
        );

        let mut nested = vec![0x0A, blob.len() as u8];
        nested.extend_from_slice(&blob);
        assert_eq!(
            extract_model_from_gen_metadata_blob(&nested).as_deref(),
            Some("Gemini 4 Hydrogen (Medium)")
        );
    }

    #[test]
    fn test_read_transcript_deltas_until_respects_timestamp_boundary() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let content = concat!(
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:00:00Z","input_tokens":100,"output_tokens":10,"cache_read_tokens":1000}"#,
            "\n",
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:01:00Z","input_tokens":200,"output_tokens":20,"cache_read_tokens":2000}"#,
            "\n",
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-17T19:03:00Z","input_tokens":500,"output_tokens":50,"cache_read_tokens":5000}"#,
            "\n",
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
        assert_eq!(
            read_transcript_deltas_until(&path, &mut state, t1, t0),
            Some((200, 20, 2000, 1))
        );
        assert_eq!(
            read_transcript_deltas_until(&path, &mut state, t2, t0),
            Some((500, 50, 5000, 1))
        );
    }

    #[test]
    fn test_locked_expiry_captured_when_dropping_below_100() {
        let mut state = CliToolState::default();
        let t0 = 1_700_000_000;

        // Tick 1: at 100%, reset_time = T+5h
        let tick1 = TickInput {
            session_id: "s1".to_string(),
            provider: "antigravity".to_string(),
            tier: "pro".to_string(),
            multi_deltas: vec![],
            skip_emit: false,
            quota_precision: Some(0.0001),
            quotas: vec![QuotaSample {
                bucket_id: "5h".to_string(),
                remaining_fraction: 1.0,
                reset_time: Some("2026-11-18T14:00:00Z".to_string()),
            }],
            timestamp: t0,
        };
        let (_, changes1) = apply_token_and_quota_tick(&mut state, &tick1);
        assert_eq!(changes1.len(), 1);
        assert_eq!(state.buckets["5h"].locked_reset_time, None);

        // Tick 2: still 100%, reset_time slides forward by 30s
        let tick2 = TickInput {
            timestamp: t0 + 30,
            quotas: vec![QuotaSample {
                bucket_id: "5h".to_string(),
                remaining_fraction: 1.0,
                reset_time: Some("2026-11-18T14:00:30Z".to_string()),
            }],
            ..tick1.clone()
        };
        let (_, changes2) = apply_token_and_quota_tick(&mut state, &tick2);
        assert!(changes2.is_empty());
        assert_eq!(
            state.buckets["5h"].effective_reset_time(),
            Some("2026-11-18T14:00:30Z")
        );
        assert_eq!(state.buckets["5h"].locked_reset_time, None);

        // Tick 3: drops below 100% -> locks expiry!
        let tick3 = TickInput {
            timestamp: t0 + 60,
            quotas: vec![QuotaSample {
                bucket_id: "5h".to_string(),
                remaining_fraction: 0.95,
                reset_time: Some("2026-11-18T14:01:00Z".to_string()),
            }],
            ..tick1.clone()
        };
        let (_, changes3) = apply_token_and_quota_tick(&mut state, &tick3);
        assert_eq!(changes3.len(), 1);
        assert_eq!(
            state.buckets["5h"].locked_reset_time.as_deref(),
            Some("2026-11-18T14:01:00Z")
        );

        // Tick 4: drops further, minor jitter in reported reset_time -> keeps locked expiry!
        let tick4 = TickInput {
            timestamp: t0 + 90,
            quotas: vec![QuotaSample {
                bucket_id: "5h".to_string(),
                remaining_fraction: 0.90,
                reset_time: Some("2026-11-18T14:01:15Z".to_string()),
            }],
            ..tick1
        };
        apply_token_and_quota_tick(&mut state, &tick4);
        assert_eq!(
            state.buckets["5h"].effective_reset_time(),
            Some("2026-11-18T14:01:00Z")
        );
    }
}
