use crate::cli::common::{
    CommonClientArgs, QuotaSample, TickInput, epoch_to_rfc3339, run_stdin_adapter,
};
use clap::Args;
use serde::Deserialize;
use std::collections::HashMap;

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
    run_stdin_adapter(&args.common, "claude-cli-state.json", move |raw, now| {
        let payload: ClaudeStatusLinePayload = serde_json::from_str(raw)?;
        Ok(Some(payload.into_tick(&tier, now)))
    })
    .await;
}

impl ClaudeStatusLinePayload {
    pub fn into_tick(self, tier: &str, now: i64) -> TickInput {
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

        TickInput {
            session_id,
            provider: "claude".to_string(),
            tier: tier.to_string(),
            model: self.model.map(|m| m.id),
            total_input_tokens,
            total_output_tokens,
            current_cached_tokens,
            total_cached_tokens: None,
            quotas,
            timestamp: now,
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
