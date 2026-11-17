use crate::cli::common::{
    CliToolState, CommonClientArgs, ModelDelta, QuotaSample, TickInput, epoch_to_rfc3339,
    run_stdin_adapter_with_state,
};
use clap::Args;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct ClaudeCliArgs {
    #[command(flatten)]
    pub common: CommonClientArgs,

    /// Subscription tier (e.g. pro, max_5x, max_20x) since Claude Code statusLine omits plan tier
    #[arg(long, env = "CUT_RS_TIER", default_value = "default")]
    pub tier: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeStatusLinePayload {
    pub session_id: Option<String>,
    pub model: Option<ClaudeModel>,
    pub context_window: Option<ClaudeContextWindow>,
    pub rate_limits: Option<HashMap<String, ClaudeLimitBucket>>,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeModel {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeContextWindow {
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    pub current_usage: Option<ClaudeCurrentUsage>,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeCurrentUsage {
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct ClaudeLimitBucket {
    pub used_percentage: Option<f64>,
    pub resets_at: Option<i64>,
}

pub async fn run_claude_cli(args: ClaudeCliArgs) {
    let tier = args.tier.clone();
    let projects_dir = dirs::home_dir().map(|h| h.join(".claude").join("projects"));
    run_stdin_adapter_with_state(&args.common, "claude-cli-state.json", move |raw, state, now| {
        let payload: ClaudeStatusLinePayload = serde_json::from_str(raw)?;
        Ok(Some(payload.into_tick_with_state(
            &tier,
            state,
            projects_dir.as_deref(),
            now,
        )))
    })
    .await;
}

impl ClaudeStatusLinePayload {
    pub fn into_tick(self, tier: &str, now: i64) -> TickInput {
        let mut dummy = CliToolState::default();
        self.into_tick_with_state(tier, &mut dummy, None, now)
    }

    pub fn into_tick_with_state(
        self,
        tier: &str,
        state: &mut CliToolState,
        projects_dir: Option<&Path>,
        now: i64,
    ) -> TickInput {
        let session_id = self.session_id.unwrap_or_else(|| "default".to_string());
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
            .rate_limits
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(k, b)| {
                let used = b.used_percentage?;
                if !used.is_finite() {
                    return None;
                }
                let bucket_id = match k.as_str() {
                    "five_hour" => "5h".to_string(),
                    "seven_day" => "weekly".to_string(),
                    other => other.to_string(),
                };
                Some(QuotaSample {
                    bucket_id,
                    remaining_fraction: ((100.0 - used) / 100.0).clamp(0.0, 1.0),
                    reset_time: b.resets_at.and_then(epoch_to_rfc3339),
                })
            })
            .collect();

        let multi_deltas = projects_dir
            .filter(|d| d.exists())
            .and_then(|d| scan_claude_projects(d, state, 1800));

        TickInput {
            session_id,
            provider: "claude".to_string(),
            tier: tier.to_string(),
            model: self.model.map(|m| m.id),
            total_input_tokens,
            total_output_tokens,
            current_cached_tokens,
            total_cached_tokens: None,
            precomputed_delta: None,
            multi_deltas,
            skip_emit: false,
            quotas,
            timestamp: now,
        }
    }
}

fn scan_claude_projects(
    projects_dir: &Path,
    state: &mut CliToolState,
    max_age_secs: u64,
) -> Option<Vec<ModelDelta>> {
    let mut jsonl_files: Vec<PathBuf> = Vec::new();
    let now = std::time::SystemTime::now();
    let Ok(project_entries) = std::fs::read_dir(projects_dir) else {
        return None;
    };
    for proj in project_entries.flatten() {
        let proj_path = proj.path();
        if !proj_path.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&proj_path) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let is_recent = f
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|mt| now.duration_since(mt).ok())
                .is_some_and(|age| age.as_secs() <= max_age_secs);
            if is_recent {
                jsonl_files.push(p);
            }
        }
    }

    if jsonl_files.is_empty() {
        return None;
    }

    let mut by_model: HashMap<String, (u64, u64, u64, u64)> = HashMap::new();
    for path in jsonl_files {
        read_claude_jsonl_deltas(&path, state, &mut by_model);
    }

    Some(
        by_model
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
            .collect(),
    )
}

fn read_claude_jsonl_deltas(
    path: &Path,
    state: &mut CliToolState,
    by_model: &mut HashMap<String, (u64, u64, u64, u64)>,
) {
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let Ok(meta) = file.metadata() else {
        return;
    };
    let file_len = meta.len();
    let key = path.to_string_lossy().into_owned();

    let mut offset = state.file_offsets.get(&key).copied().unwrap_or(0);
    if offset > file_len {
        offset = 0;
    }
    if offset == file_len {
        return;
    }

    if file.seek(SeekFrom::Start(offset)).is_err() {
        return;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return;
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return;
    };

    state
        .file_offsets
        .insert(key, offset + (last_nl as u64) + 1);

    let text = String::from_utf8_lossy(&buf[..=last_nl]);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(msg) = val.get("message") else {
            continue;
        };
        let Some(usage) = msg.get("usage") else {
            continue;
        };
        let Some(model) = msg.get("model").and_then(|v| v.as_str()) else {
            continue;
        };

        let in_tok = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .saturating_add(
                usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            );
        let out_tok = usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cached_tok = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if in_tok > 0 || out_tok > 0 || cached_tok > 0 {
            let e = by_model.entry(model.to_string()).or_insert((0, 0, 0, 0));
            e.0 = e.0.saturating_add(in_tok);
            e.1 = e.1.saturating_add(out_tok);
            e.2 = e.2.saturating_add(cached_tok);
            e.3 = e.3.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::common::{CliToolState, apply_token_and_quota_tick};

    #[test]
    fn test_claude_cli_parses_rate_limits_and_emits_interval() {
        let mut state = CliToolState::default();

        let p1: ClaudeStatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "cc-1",
                "model": { "id": "claude-3-7-sonnet" },
                "context_window": {
                    "total_input_tokens": 10000,
                    "total_output_tokens": 1000,
                    "current_usage": { "cache_read_input_tokens": 2000 }
                },
                "rate_limits": {
                    "five_hour": { "used_percentage": 10.0, "resets_at": 1794920000 },
                    "seven_day": { "used_percentage": 2.0, "resets_at": 1795400000 }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            apply_token_and_quota_tick(&mut state, &p1.into_tick("pro", 1000)),
            0
        );

        let p2: ClaudeStatusLinePayload = serde_json::from_str(
            r#"{
                "session_id": "cc-1",
                "model": { "id": "claude-3-7-sonnet" },
                "context_window": {
                    "total_input_tokens": 50000,
                    "total_output_tokens": 4000,
                    "current_usage": { "cache_read_input_tokens": 8000 }
                },
                "rate_limits": {
                    "five_hour": { "used_percentage": 12.5, "resets_at": 1794920000 },
                    "seven_day": { "used_percentage": 2.5, "resets_at": 1795400000 }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            apply_token_and_quota_tick(&mut state, &p2.into_tick("pro", 1060)),
            2
        );
        assert_eq!(state.pending_intervals.len(), 2);
        assert_eq!(state.pending_intervals[0].provider, "claude");
        assert_eq!(state.pending_intervals[0].tier, "pro");
    }
}
