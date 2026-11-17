use crate::cli::common::{
    CommonClientArgs, QuotaSample, TickInput, run_stdin_adapter,
};
use clap::Args;
use serde::Deserialize;
use std::collections::HashMap;

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
    pub model: Option<ModelInfo>,
    pub context_window: Option<ContextWindowInfo>,
    #[serde(default)]
    pub quota: HashMap<String, QuotaBucketInfo>,
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
    run_stdin_adapter(&args.common, "agy-cli-state.json", |raw, now| {
        let payload: StatusLinePayload = serde_json::from_str(raw)?;
        Ok(Some(payload.into_tick(now)))
    })
    .await;
}

impl StatusLinePayload {
    pub fn into_tick(self, now: i64) -> TickInput {
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

        TickInput {
            session_id,
            provider: self.product.unwrap_or_else(|| "antigravity".to_string()),
            tier: self.plan_tier.unwrap_or_else(|| "unknown".to_string()),
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
}
