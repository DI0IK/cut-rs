use crate::models::{ChangeEvent, ChangeEventType, IntervalRecord};
use statrs::distribution::{ContinuousCDF, FisherSnedecor};

pub const W_LONG: usize = 1500;
pub const W_SHORT: usize = 60;
const CUSUM_K: f64 = 0.5;
const CUSUM_H: f64 = 6.0;
const MIN_SHIFT_SAMPLES: usize = 4;

#[derive(Debug, Clone)]
pub struct ChangePointResult {
    pub cusum_pos: f64,
    pub cusum_neg: f64,
    pub regime_changed: bool,
    pub event: Option<ChangeEvent>,
}

/// Evaluates two-sided CUSUM on standardized residuals for single-user cost shift detection
/// and computes a dual-window F-test p-value (W_long vs W_short).
pub fn evaluate_changepoint(
    intervals: &[IntervalRecord],
    residuals: &[f64],
    sigma_hat: f64,
    prev_cusum_pos: f64,
    prev_cusum_neg: f64,
    now: i64,
) -> ChangePointResult {
    if intervals.len() < 30 || residuals.len() != intervals.len() {
        return ChangePointResult {
            cusum_pos: prev_cusum_pos,
            cusum_neg: prev_cusum_neg,
            regime_changed: false,
            event: None,
        };
    }

    let eff_sigma = sigma_hat.max(0.01);
    let probe_start = intervals.len().saturating_sub(W_SHORT);
    let mut s_pos = prev_cusum_pos * 0.5;
    let mut s_neg = prev_cusum_neg * 0.5;

    for &res in &residuals[probe_start..] {
        let z = (res / eff_sigma).clamp(-5.0, 5.0);
        s_pos = (s_pos + z - CUSUM_K).max(0.0);
        s_neg = (s_neg - z - CUSUM_K).max(0.0);
    }

    if s_pos <= CUSUM_H && s_neg <= CUSUM_H {
        return ChangePointResult {
            cusum_pos: s_pos,
            cusum_neg: s_neg,
            regime_changed: false,
            event: None,
        };
    }

    let direction = if s_pos >= s_neg { 1.0 } else { -1.0 };
    let shift_count = residuals[probe_start..]
        .iter()
        .filter(|&&r| (r / eff_sigma) * direction > 1.5)
        .count();

    let p_value = dual_window_f_test_p_value(residuals, probe_start);

    if shift_count >= MIN_SHIFT_SAMPLES {
        let cusum_val = s_pos.max(s_neg);
        let dir_label = if direction > 0.0 {
            "increase"
        } else {
            "decrease"
        };
        let event = ChangeEvent {
            timestamp: now,
            event_type: ChangeEventType::CostChange,
            description: format!(
                "Detected quota cost {dir_label} (CUSUM={cusum_val:.2}, {shift_count} samples)"
            ),
            cusum_value: Some(cusum_val),
            p_value,
        };

        ChangePointResult {
            cusum_pos: 0.0,
            cusum_neg: 0.0,
            regime_changed: true,
            event: Some(event),
        }
    } else {
        ChangePointResult {
            cusum_pos: s_pos,
            cusum_neg: s_neg,
            regime_changed: false,
            event: None,
        }
    }
}

/// Computes a two-sample F-test (squared t-stat ~ F(1, df)) comparing W_short vs baseline residuals.
fn dual_window_f_test_p_value(residuals: &[f64], probe_start: usize) -> Option<f64> {
    if probe_start < 10 || residuals.len() - probe_start < 5 {
        return None;
    }
    let long = &residuals[..probe_start];
    let short = &residuals[probe_start..];

    let n1 = long.len() as f64;
    let n2 = short.len() as f64;
    let m1 = long.iter().sum::<f64>() / n1;
    let m2 = short.iter().sum::<f64>() / n2;

    let v1 = long.iter().map(|r| (r - m1).powi(2)).sum::<f64>() / (n1 - 1.0);
    let v2 = short.iter().map(|r| (r - m2).powi(2)).sum::<f64>() / (n2 - 1.0);

    let se_sq = (v1 / n1) + (v2 / n2);
    if se_sq <= 1e-12 {
        return None;
    }
    let f_stat = (m2 - m1).powi(2) / se_sq;
    let df2 = (n1 + n2 - 2.0).max(1.0);
    FisherSnedecor::new(1.0, df2)
        .ok()
        .map(|dist| (1.0 - dist.cdf(f_stat)).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::UsageEntry;

    fn make_iv(id: i64) -> IntervalRecord {
        IntervalRecord {
            id,
            idempotency_key: format!("k{id}"),
            provider: "antigravity".to_string(),
            tier: "pro".to_string(),
            window_type: "5h".to_string(),
            started_at: 100,
            ended_at: 200,
            delta_pct: 1.0,
            quota_precision: 0.01,
            usage: vec![UsageEntry {
                model: "m".to_string(),
                tokens_in: 1000,
                tokens_out: 100,
                tokens_cached_in: 0,
                request_count: 1,
            }],
            ingested_at: 200,
        }
    }

    #[test]
    fn test_sustained_shift_triggers_regime_change() {
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        for i in 0..30 {
            intervals.push(make_iv(i));
            residuals.push(0.01);
        }

        for i in 0..6 {
            intervals.push(make_iv(30 + i));
            residuals.push(0.5);
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.1, 0.0, 0.0, 500);
        assert!(res.regime_changed, "sustained shift should trigger");
        assert!(res.event.is_some());
        assert_eq!(res.cusum_pos, 0.0);
    }
}
