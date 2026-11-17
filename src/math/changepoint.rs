use crate::models::{ChangeEvent, ChangeEventType, ClientRecord, IntervalRecord, TrustTier};
use statrs::distribution::{ContinuousCDF, FisherSnedecor};
use std::collections::{HashMap, HashSet};

pub const W_LONG: usize = 1500;
pub const W_SHORT: usize = 60;
const CUSUM_K: f64 = 0.5;
const CUSUM_H: f64 = 6.0;
const MIN_CORROBORATING_CLIENTS: usize = 5;
const MIN_CORROBORATING_SUBNETS: usize = 3;

#[derive(Debug, Clone)]
pub struct ChangePointResult {
    pub cusum_pos: f64,
    pub cusum_neg: f64,
    pub regime_changed: bool,
    pub event: Option<ChangeEvent>,
}

/// Evaluates two-sided CUSUM on standardized residuals from trusted clients,
/// dual-window F-test p-value (W_long=1500 vs W_short=60), and enforces the
/// anti-poisoning consensus rule (>= 5 trusted clients across >= 3 subnets).
pub fn evaluate_changepoint(
    intervals: &[IntervalRecord],
    residuals: &[f64],
    sigma_hat: f64,
    clients: &HashMap<String, ClientRecord>,
    prev_cusum_pos: f64,
    prev_cusum_neg: f64,
    now: i64,
) -> ChangePointResult {
    if intervals.is_empty() || residuals.len() != intervals.len() || sigma_hat <= 1e-9 {
        return ChangePointResult {
            cusum_pos: prev_cusum_pos,
            cusum_neg: prev_cusum_neg,
            regime_changed: false,
            event: None,
        };
    }

    // Evaluate CUSUM over the probe window (last W_SHORT intervals) using ONLY Trusted clients
    let probe_start = intervals.len().saturating_sub(W_SHORT);
    let mut s_pos = prev_cusum_pos * 0.5; // gentle decay of prior accumulator across re-fits
    let mut s_neg = prev_cusum_neg * 0.5;

    for i in probe_start..intervals.len() {
        let iv = &intervals[i];
        let Some(client) = clients.get(&iv.client_id) else {
            continue;
        };
        if client.trust_tier != TrustTier::Trusted {
            continue;
        }
        let z = (residuals[i] / sigma_hat).clamp(-5.0, 5.0);
        s_pos = (s_pos + z - CUSUM_K).max(0.0);
        s_neg = (s_neg - z - CUSUM_K).max(0.0);
    }

    let triggered_pos = s_pos > CUSUM_H;
    let triggered_neg = s_neg > CUSUM_H;

    if !triggered_pos && !triggered_neg {
        return ChangePointResult {
            cusum_pos: s_pos,
            cusum_neg: s_neg,
            regime_changed: false,
            event: None,
        };
    }

    // Check anti-poisoning consensus in the probe window
    let direction = if s_pos >= s_neg { 1.0 } else { -1.0 };
    let mut corrob_clients = HashSet::new();
    let mut corrob_subnets = HashSet::new();

    for i in probe_start..intervals.len() {
        let iv = &intervals[i];
        let Some(client) = clients.get(&iv.client_id) else {
            continue;
        };
        if client.trust_tier != TrustTier::Trusted {
            continue;
        }
        let z = residuals[i] / sigma_hat;
        if z * direction > 1.5 {
            corrob_clients.insert(client.client_id.clone());
            corrob_subnets.insert(client.subnet_hash.clone());
        }
    }

    let p_value = dual_window_f_test_p_value(residuals, probe_start);

    if corrob_clients.len() >= MIN_CORROBORATING_CLIENTS
        && corrob_subnets.len() >= MIN_CORROBORATING_SUBNETS
    {
        let cusum_val = s_pos.max(s_neg);
        let dir_label = if direction > 0.0 {
            "increase"
        } else {
            "decrease"
        };
        let event = ChangeEvent {
            timestamp: now,
            event_type: ChangeEventType::RegimeShift,
            description: format!(
                "Confirmed quota cost {dir_label} (CUSUM={cusum_val:.2}, {} trusted clients across {} subnets)",
                corrob_clients.len(),
                corrob_subnets.len()
            ),
            cusum_value: Some(cusum_val),
            corroborating_clients: Some(corrob_clients.len()),
            corroborating_subnets: Some(corrob_subnets.len()),
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

    fn make_iv(id: i64, client_id: &str) -> IntervalRecord {
        IntervalRecord {
            id,
            idempotency_key: format!("k{id}"),
            client_id: client_id.to_string(),
            provider: "p".to_string(),
            tier: "t".to_string(),
            window_type: "w".to_string(),
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
            residual_pct: None,
            weight_assigned: None,
            ingested_at: 200,
        }
    }

    #[test]
    fn test_sybil_cluster_single_subnet_cannot_trigger_regime_change() {
        let mut clients = HashMap::new();
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        for i in 0..30 {
            let cid = format!("baseline_{i}");
            clients.insert(
                cid.clone(),
                ClientRecord {
                    client_id: cid.clone(),
                    created_at: 0,
                    subnet_hash: format!("sub_{}", i % 5),
                    trust_score: 1.0,
                    trust_tier: TrustTier::Trusted,
                    accepted_count: 50,
                    ema_abs_z: 0.2,
                    last_seen_at: 100,
                },
            );
            intervals.push(make_iv(i, &cid));
            residuals.push(0.01);
        }

        // 10 trusted clients all from only 2 subnets try to poison CUSUM
        for i in 0..10 {
            let cid = format!("sybil_{i}");
            clients.insert(
                cid.clone(),
                ClientRecord {
                    client_id: cid.clone(),
                    created_at: 0,
                    subnet_hash: format!("sybil_sub_{}", i % 2), // only 2 subnets!
                    trust_score: 1.0,
                    trust_tier: TrustTier::Trusted,
                    accepted_count: 50,
                    ema_abs_z: 0.2,
                    last_seen_at: 100,
                },
            );
            intervals.push(make_iv(30 + i, &cid));
            residuals.push(2.0); // z = 2.0 / 0.1 = 20 (clamped to 5.0)
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.1, &clients, 0.0, 0.0, 500);
        assert!(!res.regime_changed, "2 subnets must not trigger regime change");
    }

    #[test]
    fn test_diverse_trusted_clients_trigger_regime_change() {
        let mut clients = HashMap::new();
        let mut intervals = Vec::new();
        let mut residuals = Vec::new();

        for i in 0..30 {
            let cid = format!("baseline_{i}");
            clients.insert(
                cid.clone(),
                ClientRecord {
                    client_id: cid.clone(),
                    created_at: 0,
                    subnet_hash: format!("sub_{}", i % 5),
                    trust_score: 1.0,
                    trust_tier: TrustTier::Trusted,
                    accepted_count: 50,
                    ema_abs_z: 0.2,
                    last_seen_at: 100,
                },
            );
            intervals.push(make_iv(i, &cid));
            residuals.push(0.01);
        }

        // 6 trusted clients across 4 distinct subnets corroborate a shift
        for i in 0..6 {
            let cid = format!("honest_{i}");
            clients.insert(
                cid.clone(),
                ClientRecord {
                    client_id: cid.clone(),
                    created_at: 0,
                    subnet_hash: format!("honest_sub_{}", i % 4),
                    trust_score: 1.0,
                    trust_tier: TrustTier::Trusted,
                    accepted_count: 50,
                    ema_abs_z: 0.2,
                    last_seen_at: 100,
                },
            );
            intervals.push(make_iv(30 + i, &cid));
            residuals.push(0.5);
        }

        let res = evaluate_changepoint(&intervals, &residuals, 0.1, &clients, 0.0, 0.0, 500);
        assert!(res.regime_changed, "6 trusted clients across 4 subnets should trigger");
        assert!(res.event.is_some());
        assert_eq!(res.cusum_pos, 0.0);
    }
}
