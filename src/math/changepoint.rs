use crate::models::{ChangeEvent, ChangeEventType, IntervalRecord};
use statrs::distribution::{ContinuousCDF, FisherSnedecor};

pub const W_LONG: usize = 1500;
pub const W_SHORT: usize = 60;
const CUSUM_K: f64 = 0.5;
const CUSUM_H: f64 = 6.0;
pub const RECENT_TAIL: usize = 6;
const MIN_RECENT_SHIFTED: usize = 4;
const REL_NOISE_FLOOR: f64 = 0.04;
const MIN_Z_SHIFT: f64 = 0.75;
const MIN_REL_SHIFT: f64 = 0.06;

#[derive(Debug, Clone)]
pub struct ChangePointResult {
    pub cusum_pos: f64,
    pub cusum_neg: f64,
    pub regime_changed: bool,
    pub event: Option<ChangeEvent>,
}

fn median_f64(vals: &mut [f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(|a, b| a.total_cmp(b));
    let mid = vals.len() / 2;
    if vals.len() % 2 == 1 {
        vals[mid]
    } else {
        0.5 * (vals[mid - 1] + vals[mid])
    }
}

fn relative_residual(iv: &IntervalRecord, res: f64) -> Option<f64> {
    let min_pred = (5.0 * iv.quota_precision.max(0.001)).max(0.05);
    let pred = iv.delta_pct - res;
    if pred >= min_pred {
        Some(res / pred)
    } else {
        None
    }
}

/// Evaluates two-sided CUSUM on standardized and relative residuals for single-user cost shift
/// detection, requiring a sustained shift in the most recent tail to avoid false positives from
/// sparse background-task or multi-session spikes.
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
    let tail_len = (intervals.len() / 3).clamp(10, W_SHORT);
    let probe_start = intervals.len() - tail_len;

    let mut s_pos = prev_cusum_pos * 0.5;
    let mut s_neg = prev_cusum_neg * 0.5;
    let mut rel_pos = 0.0_f64;
    let mut rel_neg = 0.0_f64;

    for (iv, &res) in intervals[probe_start..]
        .iter()
        .zip(&residuals[probe_start..])
    {
        let z = (res / eff_sigma).clamp(-5.0, 5.0);
        s_pos = (s_pos + z - CUSUM_K).max(0.0);
        s_neg = (s_neg - z - CUSUM_K).max(0.0);

        if let Some(rel) = relative_residual(iv, res) {
            let rz = (rel / REL_NOISE_FLOOR).clamp(-5.0, 5.0);
            rel_pos = (rel_pos + rz - CUSUM_K).max(0.0);
            rel_neg = (rel_neg - rz - CUSUM_K).max(0.0);
        }
    }

    let std_triggered = s_pos > CUSUM_H || s_neg > CUSUM_H;
    let rel_triggered = rel_pos > CUSUM_H || rel_neg > CUSUM_H;
    if !std_triggered && !rel_triggered {
        return ChangePointResult {
            cusum_pos: s_pos,
            cusum_neg: s_neg,
            regime_changed: false,
            event: None,
        };
    }

    let (pos_score, neg_score) = if s_pos.max(s_neg) >= rel_pos.max(rel_neg) {
        (s_pos, s_neg)
    } else {
        (rel_pos, rel_neg)
    };
    let direction = if pos_score >= neg_score { 1.0 } else { -1.0 };

    // Require a sustained shift in the most recent tail so sparse positive spikes do not trigger.
    let recent_start = intervals.len().saturating_sub(RECENT_TAIL);
    let mut recent_z = Vec::with_capacity(RECENT_TAIL);
    let mut recent_rel = Vec::with_capacity(RECENT_TAIL);
    let mut recent_shifted = 0usize;

    for (iv, &res) in intervals[recent_start..]
        .iter()
        .zip(&residuals[recent_start..])
    {
        let dir_z = (res / eff_sigma) * direction;
        recent_z.push(dir_z);

        let dir_rel = relative_residual(iv, res).map(|r| r * direction);
        if let Some(dr) = dir_rel {
            recent_rel.push(dr);
        }

        if dir_z >= MIN_Z_SHIFT || dir_rel.is_some_and(|dr| dr >= MIN_REL_SHIFT) {
            recent_shifted += 1;
        }
    }

    let median_z = median_f64(&mut recent_z);
    let median_rel = if recent_rel.len() >= MIN_RECENT_SHIFTED {
        Some(median_f64(&mut recent_rel))
    } else {
        None
    };
    let sustained = recent_shifted >= MIN_RECENT_SHIFTED
        && (median_z >= MIN_Z_SHIFT || median_rel.is_some_and(|mr| mr >= MIN_REL_SHIFT));

    let p_value = dual_window_f_test_p_value(residuals, probe_start);
    let f_significant = p_value.is_some_and(|p| p < 0.05);

    if sustained && f_significant {
        let cusum_val = pos_score.max(neg_score);
        let dir_label = if direction > 0.0 {
            "increase"
        } else {
            "decrease"
        };
        let event = ChangeEvent {
            timestamp: now,
            event_type: ChangeEventType::CostChange,
            description: format!(
                "Detected quota cost {dir_label} (CUSUM={cusum_val:.2}, {recent_shifted}/{RECENT_TAIL} recent samples)"
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

    #[test]
    fn test_sparse_positive_outliers_do_not_trigger_regime_change() {
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        // 42 intervals where most are exact (res = 0.0) and ~25% have positive spikes
        // (e.g. background tasks / conversation naming), including 2 in the last 6.
        let spike_indices = [2, 7, 13, 19, 25, 29, 33, 37, 39];
        for i in 0..42 {
            let mut iv = make_iv(i);
            if spike_indices.contains(&i) {
                iv.delta_pct = 0.9;
                residuals.push(0.5);
            } else {
                iv.delta_pct = 0.4;
                residuals.push(0.0);
            }
            intervals.push(iv);
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.15, 0.0, 0.0, 500);
        assert!(
            !res.regime_changed,
            "sparse positive outliers should not trigger regime change"
        );
        assert!(res.event.is_none());
    }

    #[test]
    fn test_small_10_percent_sustained_shift_triggers_regime_change() {
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        // 30 baseline intervals around 0.40% delta_pct, plus 6 recent intervals with a +10% shift
        // (+0.04% residual), even with sigma_hat inflated to 0.25% by earlier outliers.
        for i in 0..30 {
            let mut iv = make_iv(i);
            iv.delta_pct = 0.40;
            intervals.push(iv);
            residuals.push(0.0);
        }
        for i in 0..6 {
            let mut iv = make_iv(30 + i);
            iv.delta_pct = 0.44;
            intervals.push(iv);
            residuals.push(0.04);
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.25, 0.0, 0.0, 500);
        assert!(
            res.regime_changed,
            "sustained +10% shift should trigger even when sigma_hat is inflated"
        );
        let ev = res.event.expect("expected cost_change event");
        assert!(ev.description.contains("increase"));
    }

    #[test]
    fn test_small_10_percent_decrease_triggers_regime_change() {
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        for i in 0..30 {
            let mut iv = make_iv(i);
            iv.delta_pct = 0.50;
            intervals.push(iv);
            residuals.push(0.0);
        }
        for i in 0..6 {
            let mut iv = make_iv(30 + i);
            iv.delta_pct = 0.45;
            intervals.push(iv);
            residuals.push(-0.05);
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.25, 0.0, 0.0, 500);
        assert!(
            res.regime_changed,
            "sustained -10% shift should trigger regime change"
        );
        let ev = res.event.expect("expected cost_change event");
        assert!(ev.description.contains("decrease"));
    }

    #[test]
    fn test_insignificant_f_test_does_not_trigger_regime_change() {
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        // High-variance noisy baseline with a small recent shift where F-test p >= 0.05
        for i in 0..30 {
            intervals.push(make_iv(i));
            residuals.push(if i % 2 == 0 { 0.8 } else { -0.8 });
        }
        for i in 0..6 {
            intervals.push(make_iv(30 + i));
            residuals.push(0.25);
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.1, 0.0, 0.0, 500);
        assert!(
            !res.regime_changed,
            "insignificant F-test should not trigger regime change"
        );
    }
}
