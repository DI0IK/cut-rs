use crate::cli::common::{
    CliToolState, CommonClientArgs, ModelDelta, QuotaSample, TickInput,
    run_stdin_adapter_with_state,
};
use clap::Args;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct AgyCliArgs {
    #[command(flatten)]
    pub common: CommonClientArgs,
}

#[derive(Debug, Deserialize)]
pub struct StatusLinePayload {
    pub session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub transcript_path: Option<String>,
    pub product: Option<String>,
    pub plan_tier: Option<String>,
    pub model: Option<ModelInfo>,
    pub context_window: Option<ContextWindowInfo>,
    #[serde(default)]
    pub quota: HashMap<String, QuotaBucketInfo>,
    #[serde(default)]
    pub subagents: Vec<SubagentInfo>,
    pub cost: Option<CostInfo>,
}

#[derive(Debug, Deserialize)]
pub struct SubagentInfo {
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CostInfo {
    pub subagent_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ModelInfo {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct ContextWindowInfo {
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    pub current_usage: Option<CurrentUsageInfo>,
}

#[derive(Debug, Deserialize)]
pub struct CurrentUsageInfo {
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct QuotaBucketInfo {
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

pub async fn run_agy_cli(args: AgyCliArgs) {
    run_stdin_adapter_with_state(&args.common, "agy-cli-state.json", |raw, state, now| {
        let payload: StatusLinePayload = serde_json::from_str(raw)?;
        Ok(Some(payload.into_tick_with_state(state, now)))
    })
    .await;
}

impl StatusLinePayload {
    pub fn into_tick(self, now: i64) -> TickInput {
        let mut dummy = CliToolState::default();
        self.into_tick_with_state(&mut dummy, now)
    }

    pub fn into_tick_with_state(self, state: &mut CliToolState, now: i64) -> TickInput {
        let session_id = self
            .session_id
            .or(self.conversation_id)
            .unwrap_or_else(|| "default".to_string());

        let (total_input_tokens, total_output_tokens, current_cached_tokens) =
            match self.context_window {
                Some(cw) => (
                    cw.total_input_tokens,
                    cw.total_output_tokens,
                    cw.current_usage
                        .map(|u| u.cache_read_input_tokens)
                        .unwrap_or(0),
                ),
                None => (0, 0, 0),
            };

        let quotas = self
            .quota
            .into_iter()
            .filter_map(|(bucket_id, q)| {
                if q.disabled == Some(true) {
                    return None;
                }
                let remaining_fraction = q.remaining_fraction?;
                Some(QuotaSample {
                    bucket_id,
                    remaining_fraction,
                    reset_time: q.reset_time,
                })
            })
            .collect();

        let current_model = self.model.map(|m| m.id);
        if let Some(m) = &current_model {
            state.session_models.insert(session_id.clone(), m.clone());
        }

        // Keep session_models bounded
        if state.session_models.len() > 200 {
            state.session_models.retain(|k, _| k == &session_id);
        }

        let mut multi_deltas: Option<Vec<ModelDelta>> = None;
        let mut by_model: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
        let mut any_transcript_read = false;

        // Collect explicit transcript_path first (so subagents are discovered immediately),
        // then all other recently active transcripts under <brain_root>.
        let mut candidate_paths: Vec<PathBuf> = Vec::new();
        if let Some(t_path_str) = self
            .transcript_path
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let t_path = PathBuf::from(t_path_str);
            candidate_paths.push(t_path.clone());
            if let Some(brain_root) = resolve_brain_root(&t_path) {
                for p in discover_recent_brain_transcripts(&brain_root, 1800) {
                    if !candidate_paths.contains(&p) {
                        candidate_paths.push(p);
                    }
                }
            }
        }

        let mut unattributed_sibling = false;
        let mut idx = 0;
        while idx < candidate_paths.len() {
            let path = candidate_paths[idx].clone();
            idx += 1;

            let conv_id = extract_conv_id_from_transcript(&path);
            let model_opt = conv_id
                .as_deref()
                .and_then(|id| state.session_models.get(id).cloned())
                .or_else(|| {
                    if self.transcript_path.as_deref() == Some(path.to_string_lossy().as_ref()) {
                        current_model.clone()
                    } else {
                        None
                    }
                });

            let Some(model_name) = model_opt else {
                // Defer unseen sibling tabs until their own statusLine records their model,
                // but flag if they already logged model usage so we don't emit a poisoned interval.
                if let Some((di, do_, dc, _)) = read_transcript_deltas_inner(&path, state, false, false)
                    && (di > 0 || do_ > 0 || dc > 0)
                {
                    unattributed_sibling = true;
                }
                continue;
            };

            let prev_sub_count = state.active_subagent_transcripts.len();
            if let Some((di, do_, dc, reqs)) = read_transcript_deltas(&path, state, true) {
                any_transcript_read = true;
                if di > 0 || do_ > 0 || dc > 0 {
                    let e = by_model.entry(model_name.clone()).or_insert((0, 0, 0, 0));
                    e.0 = e.0.saturating_add(di);
                    e.1 = e.1.saturating_add(do_);
                    e.2 = e.2.saturating_add(dc);
                    e.3 = e.3.saturating_add(reqs);
                }
                // Queue any newly discovered subagent transcripts with parent's model
                for new_sub in state.active_subagent_transcripts[prev_sub_count..].iter() {
                    let sub_p = PathBuf::from(new_sub);
                    if let Some(sub_id) = extract_conv_id_from_transcript(&sub_p) {
                        state
                            .session_models
                            .entry(sub_id)
                            .or_insert_with(|| model_name.clone());
                    }
                    if !candidate_paths.contains(&sub_p) {
                        candidate_paths.push(sub_p);
                    }
                }
            }
        }

        if any_transcript_read {
            multi_deltas = Some(
                by_model
                    .into_iter()
                    .map(|(model, (tokens_in, tokens_out, tokens_cached_in, request_count))| {
                        ModelDelta {
                            model,
                            tokens_in,
                            tokens_out,
                            tokens_cached_in,
                            request_count,
                        }
                    })
                    .collect(),
            );
        }

        let subagent_usd_increased = match (
            state.last_subagent_usd,
            self.cost.as_ref().and_then(|c| c.subagent_usd),
        ) {
            (Some(prev), Some(curr)) => curr > prev + 1e-9,
            (None, Some(curr)) => curr > 1e-9,
            _ => false,
        };
        if let Some(curr) = self.cost.as_ref().and_then(|c| c.subagent_usd) {
            state.last_subagent_usd = Some(curr);
        }

        let subagent_detected = !self.subagents.is_empty() || subagent_usd_increased;
        let skip_emit = (subagent_detected && state.active_subagent_transcripts.is_empty())
            || unattributed_sibling;

        TickInput {
            session_id,
            provider: self.product.unwrap_or_else(|| "antigravity".to_string()),
            tier: self.plan_tier.unwrap_or_else(|| "unknown".to_string()),
            model: current_model,
            total_input_tokens,
            total_output_tokens,
            current_cached_tokens,
            total_cached_tokens: None,
            precomputed_delta: None,
            multi_deltas,
            skip_emit,
            quota_precision: Some(0.0001),
            quotas,
            timestamp: now,
        }
    }
}

/// Incrementally reads complete JSONL lines from `path` starting at `state.file_offsets[path]`,
/// summing `(tokens_in, tokens_out, tokens_cached_in, request_count)` for `PLANNER_RESPONSE` steps.
fn read_transcript_deltas(
    path: &Path,
    state: &mut CliToolState,
    discover_subagents: bool,
) -> Option<(u64, u64, u64, u64)> {
    read_transcript_deltas_inner(path, state, discover_subagents, true)
}

fn read_transcript_deltas_inner(
    path: &Path,
    state: &mut CliToolState,
    discover_subagents: bool,
    advance_offset: bool,
) -> Option<(u64, u64, u64, u64)> {
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let key = path.to_string_lossy().into_owned();

    let mut offset = state.file_offsets.get(&key).copied().unwrap_or(0);
    if offset > file_len {
        offset = 0;
    }
    if offset == file_len {
        return Some((0, 0, 0, 0));
    }

    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;

    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Some((0, 0, 0, 0));
    };

    let complete_slice = &buf[..=last_nl];
    if advance_offset {
        state
            .file_offsets
            .insert(key, offset + (last_nl as u64) + 1);
    }

    let text = String::from_utf8_lossy(complete_slice);
    let mut tok_in = 0u64;
    let mut tok_out = 0u64;
    let mut tok_cached = 0u64;
    let mut reqs = 0u64;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if val.get("source").and_then(|v| v.as_str()) == Some("MODEL")
            && val.get("type").and_then(|v| v.as_str()) == Some("PLANNER_RESPONSE")
        {
            tok_in = tok_in.saturating_add(
                val.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            );
            tok_out = tok_out.saturating_add(
                val.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            );
            tok_cached = tok_cached.saturating_add(
                val.get("cache_read_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            );
            reqs = reqs.saturating_add(1);
        }

        if discover_subagents
            && let Some(content) = val.get("content").and_then(|v| v.as_str())
            && content.contains("conversationId")
            && let Some(sub_id) = extract_uuid_after_key(content, "conversationId")
            && let Some(sub_path) = resolve_sibling_transcript(path, &sub_id)
        {
            let sub_str = sub_path.to_string_lossy().into_owned();
            if sub_path.exists() && !state.active_subagent_transcripts.contains(&sub_str) {
                state.active_subagent_transcripts.push(sub_str);
            }
        }
    }

    Some((tok_in, tok_out, tok_cached, reqs))
}

fn extract_uuid_after_key(haystack: &str, key: &str) -> Option<String> {
    let idx = haystack.find(key)?;
    let rest = &haystack[idx + key.len()..];
    for (i, _) in rest.match_indices('-') {
        if i >= 8 && rest.len() >= i + 28 {
            let candidate = &rest[i - 8..i + 28];
            if candidate.len() == 36
                && candidate.chars().enumerate().all(|(pos, c)| {
                    if matches!(pos, 8 | 13 | 18 | 23) {
                        c == '-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
            {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn resolve_brain_root(transcript_path: &Path) -> Option<PathBuf> {
    // Expected layout: <brain_root>/<conv_id>/.system_generated/logs/transcript.jsonl
    transcript_path
        .parent()? // logs
        .parent()? // .system_generated
        .parent()? // <conv_id>
        .parent() // <brain_root>
        .map(|p| p.to_path_buf())
}

fn extract_conv_id_from_transcript(transcript_path: &Path) -> Option<String> {
    transcript_path
        .parent()? // logs
        .parent()? // .system_generated
        .parent()? // <conv_id>
        .file_name()?
        .to_str()
        .map(|s| s.to_string())
}

fn discover_recent_brain_transcripts(brain_root: &Path, max_age_secs: u64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(brain_root) else {
        return out;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let p = entry
            .path()
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        let is_recent = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() <= max_age_secs);
        if is_recent {
            out.push(p);
        }
    }
    out
}

fn resolve_sibling_transcript(parent_transcript: &Path, subagent_id: &str) -> Option<PathBuf> {
    // Expected layout: <brain_root>/<conv_id>/.system_generated/logs/transcript.jsonl
    let brain_root = parent_transcript
        .parent()? // logs
        .parent()? // .system_generated
        .parent()? // <conv_id>
        .parent()?; // <brain_root>
    Some(
        brain_root
            .join(subagent_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::common::{CliToolState, apply_token_and_quota_tick};

    #[test]
    fn test_agy_cli_diffs_turns_and_handles_bucket_lifecycle() {
        let mut state = CliToolState::default();

        let p1: StatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "s1",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": { "id": "Gemini 4 Argon (Medium)" },
                "context_window": {
                    "total_input_tokens": 10000,
                    "total_output_tokens": 1000,
                    "current_usage": { "cache_read_input_tokens": 2000 }
                },
                "quota": {
                    "gemini-weekly": { "remaining_fraction": 0.95, "reset_time": "2026-07-06T07:50:32Z" }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(apply_token_and_quota_tick(&mut state, &p1.into_tick(1000)), 0);

        let p2: StatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "s1",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": { "id": "Gemini 4 Argon (Medium)" },
                "context_window": {
                    "total_input_tokens": 60000,
                    "total_output_tokens": 6000,
                    "current_usage": { "cache_read_input_tokens": 5000 }
                },
                "quota": {
                    "gemini-weekly": { "remaining_fraction": 0.94, "reset_time": "2026-07-06T07:50:32Z" },
                    "new-bucket": { "remaining_fraction": 1.00, "reset_time": "2026-07-06T07:50:32Z" }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(apply_token_and_quota_tick(&mut state, &p2.into_tick(1060)), 1);
        assert_eq!(state.pending_intervals.len(), 1);
        let iv = &state.pending_intervals[0];
        assert_eq!(iv.window_type, "gemini-weekly");
        assert!((iv.quota_before_pct - 95.0).abs() < 1e-6);
        assert!((iv.quota_after_pct - 94.0).abs() < 1e-6);
        assert_eq!(iv.usage[0].tokens_in, 50_000);
        assert_eq!(iv.usage[0].tokens_out, 5_000);
        assert_eq!(iv.usage[0].tokens_cached_in, 5_000);
        assert!(state.buckets.contains_key("new-bucket"));

        let p3: StatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "s1",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": { "id": "Gemini 4 Argon (Medium)" },
                "context_window": {
                    "total_input_tokens": 70000,
                    "total_output_tokens": 7000
                },
                "quota": {
                    "gemini-weekly": { "remaining_fraction": 1.00, "reset_time": "2026-07-13T07:50:32Z" }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(apply_token_and_quota_tick(&mut state, &p3.into_tick(1120)), 0);
        assert!(!state.buckets.contains_key("new-bucket"));
    }

    #[test]
    fn test_agy_cli_tails_transcript_and_attributes_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let brain = dir.path();
        let parent_id = "11111111-2222-3333-4444-555555555555";
        let sub_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

        let parent_logs = brain.join(parent_id).join(".system_generated").join("logs");
        let sub_logs = brain.join(sub_id).join(".system_generated").join("logs");
        std::fs::create_dir_all(&parent_logs).unwrap();
        std::fs::create_dir_all(&sub_logs).unwrap();

        let parent_tx = parent_logs.join("transcript.jsonl");
        let sub_tx = sub_logs.join("transcript.jsonl");

        std::fs::write(
            &sub_tx,
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","input_tokens":500,"cache_read_tokens":2000,"output_tokens":100}
"#,
        )
        .unwrap();

        std::fs::write(
            &parent_tx,
            format!(
                "{{\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1000,\"cache_read_tokens\":5000,\"output_tokens\":200}}\n\
                 {{\"source\":\"MODEL\",\"type\":\"GENERIC\",\"content\":\"Launched subagent conversationId: {sub_id}\"}}\n\
                 {{\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":300,\"cache_read_tokens\":6000,\"output_tokens\":150}}\n"
            ),
        )
        .unwrap();

        let mut state = CliToolState::default();

        // Tick 1: baseline quota
        let p1: StatusLinePayload = serde_json::from_str(&format!(
            r#"{{
                "session_id": "{parent_id}",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": {{ "id": "gemini 4 hydrogen (medium)" }},
                "quota": {{ "gemini-5h": {{ "remaining_fraction": 0.90 }} }}
            }}"#
        ))
        .unwrap();
        let t1 = p1.into_tick_with_state(&mut state, 1000);
        assert_eq!(apply_token_and_quota_tick(&mut state, &t1), 0);

        // Tick 2: transcript has 2 parent turns + 1 subagent turn, quota drops 0.90 -> 0.89
        let p2: StatusLinePayload = serde_json::from_str(&format!(
            r#"{{
                "session_id": "{parent_id}",
                "transcript_path": "{}",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": {{ "id": "gemini 4 hydrogen (medium)" }},
                "subagents": [{{ "status": "completed" }}],
                "quota": {{ "gemini-5h": {{ "remaining_fraction": 0.89 }} }}
            }}"#,
            parent_tx.display().to_string().replace('\\', "\\\\")
        ))
        .unwrap();
        let t2 = p2.into_tick_with_state(&mut state, 1060);
        assert!(!t2.skip_emit);
        assert_eq!(apply_token_and_quota_tick(&mut state, &t2), 1);

        let iv = &state.pending_intervals[0];
        assert_eq!(iv.usage[0].tokens_in, 1800);
        assert_eq!(iv.usage[0].tokens_out, 450);
        assert_eq!(iv.usage[0].tokens_cached_in, 13_000);
        assert_eq!(iv.usage[0].request_count, 3);
    }

    #[test]
    fn test_agy_cli_skips_unattributed_subagent_burn() {
        let mut state = CliToolState::default();
        let p1: StatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "s1",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": { "id": "gemini 4 hydrogen (medium)" },
                "context_window": { "total_input_tokens": 1000, "total_output_tokens": 100 },
                "quota": { "gemini-5h": { "remaining_fraction": 0.90 } }
            }"#,
        )
        .unwrap();
        let t1 = p1.into_tick_with_state(&mut state, 1000);
        assert_eq!(apply_token_and_quota_tick(&mut state, &t1), 0);

        let p2: StatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "s1",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": { "id": "gemini 4 hydrogen (medium)" },
                "context_window": { "total_input_tokens": 5000, "total_output_tokens": 500 },
                "subagents": [{ "status": "running" }],
                "quota": { "gemini-5h": { "remaining_fraction": 0.88 } }
            }"#,
        )
        .unwrap();
        let t2 = p2.into_tick_with_state(&mut state, 1060);
        assert!(t2.skip_emit);
        assert_eq!(apply_token_and_quota_tick(&mut state, &t2), 0);
        assert!(state.pending_intervals.is_empty());
        assert!((state.buckets["gemini-5h"].last_remaining_fraction - 0.88).abs() < 1e-9);
    }

    #[test]
    fn test_agy_cli_concurrent_tabs_defer_unseen_and_attribute_both() {
        let dir = tempfile::tempdir().unwrap();
        let brain = dir.path();
        let tab_a = "11111111-1111-1111-1111-111111111111";
        let tab_b = "22222222-2222-2222-2222-222222222222";

        let logs_a = brain.join(tab_a).join(".system_generated").join("logs");
        let logs_b = brain.join(tab_b).join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs_a).unwrap();
        std::fs::create_dir_all(&logs_b).unwrap();

        let tx_a = logs_a.join("transcript.jsonl");
        let tx_b = logs_b.join("transcript.jsonl");

        std::fs::write(&tx_a, "").unwrap();
        std::fs::write(
            &tx_b,
            "{\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":500,\"cache_read_tokens\":1000,\"output_tokens\":50}\n",
        )
        .unwrap();

        let mut state = CliToolState::default();

        // Tick 1: Tab A initializes quota at 0.90 (Tab B unseen -> deferred, offset not advanced!)
        let p_a1: StatusLinePayload = serde_json::from_str(&format!(
            r#"{{
                "session_id": "{tab_a}",
                "transcript_path": "{}",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": {{ "id": "gemini 4 hydrogen (medium)" }},
                "quota": {{ "gemini-5h": {{ "remaining_fraction": 0.90 }} }}
            }}"#,
            tx_a.display().to_string().replace('\\', "\\\\")
        ))
        .unwrap();
        let t_a1 = p_a1.into_tick_with_state(&mut state, 1000);
        assert_eq!(apply_token_and_quota_tick(&mut state, &t_a1), 0);
        assert!(!state.file_offsets.contains_key(&tx_b.to_string_lossy().into_owned()));

        // Now Tab A completes a turn after baseline is established
        std::fs::write(
            &tx_a,
            "{\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"input_tokens\":1000,\"cache_read_tokens\":4000,\"output_tokens\":200}\n",
        )
        .unwrap();

        // Tick 2: Tab B's statusLine fires with its own model, and quota drops to 0.89
        let p_b1: StatusLinePayload = serde_json::from_str(&format!(
            r#"{{
                "session_id": "{tab_b}",
                "transcript_path": "{}",
                "product": "antigravity",
                "plan_tier": "Pro",
                "model": {{ "id": "gemini 4 flash" }},
                "quota": {{ "gemini-5h": {{ "remaining_fraction": 0.89 }} }}
            }}"#,
            tx_b.display().to_string().replace('\\', "\\\\")
        ))
        .unwrap();
        let t_b1 = p_b1.into_tick_with_state(&mut state, 1060);
        assert_eq!(apply_token_and_quota_tick(&mut state, &t_b1), 1);

        let iv = &state.pending_intervals[0];
        assert_eq!(iv.usage.len(), 2);
        let u_a = iv.usage.iter().find(|u| u.model == "gemini 4 hydrogen (medium)").unwrap();
        let u_b = iv.usage.iter().find(|u| u.model == "gemini 4 flash").unwrap();
        assert_eq!(u_a.tokens_in, 1000);
        assert_eq!(u_b.tokens_in, 500);
    }
}
