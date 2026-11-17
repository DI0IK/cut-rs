use crate::cli::common::{
    CliToolState, CommonClientArgs, QuotaSample, TickInput, run_stdin_adapter_with_state,
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

        if state.session_id.as_deref() != Some(session_id.as_str()) {
            state.active_subagent_transcripts.clear();
            state.last_subagent_usd = None;
        }

        let mut precomputed_delta = None;
        if let Some(t_path_str) = self.transcript_path.as_deref().filter(|s| !s.trim().is_empty()) {
            let t_path = Path::new(t_path_str);
            if let Some(mut delta) = read_transcript_deltas(t_path, state, true) {
                let sub_paths = state.active_subagent_transcripts.clone();
                for sub_p in sub_paths {
                    if let Some(sub_d) = read_transcript_deltas(Path::new(&sub_p), state, false) {
                        delta.0 = delta.0.saturating_add(sub_d.0);
                        delta.1 = delta.1.saturating_add(sub_d.1);
                        delta.2 = delta.2.saturating_add(sub_d.2);
                        delta.3 = delta.3.saturating_add(sub_d.3);
                    }
                }
                precomputed_delta = Some(delta);
            }
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
        let skip_emit = subagent_detected && state.active_subagent_transcripts.is_empty();

        TickInput {
            session_id,
            provider: self.product.unwrap_or_else(|| "antigravity".to_string()),
            tier: self.plan_tier.unwrap_or_else(|| "unknown".to_string()),
            model: self.model.map(|m| m.id),
            total_input_tokens,
            total_output_tokens,
            current_cached_tokens,
            total_cached_tokens: None,
            precomputed_delta,
            skip_emit,
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
    state
        .file_offsets
        .insert(key, offset + (last_nl as u64) + 1);

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
}
