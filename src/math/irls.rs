use crate::math::nnls::solve_nnls;
use crate::models::{
    ClientRecord, ConfidenceInterval, Identifiability, IntervalRecord, ModelStatus,
    ModelWeightEstimate, TokenMix,
};
use nalgebra::{DMatrix, DVector};
use statrs::distribution::{ContinuousCDF, StudentsT};
use std::collections::{BTreeMap, HashMap};

const HALF_LIFE_DAYS: f64 = 14.0;
const MAX_IRLS_ITERS: usize = 15;
const IRLS_TOL: f64 = 1e-5;
const CLIENT_CAP_FRAC: f64 = 0.05;

#[derive(Debug, Clone)]
pub struct IrlsFitResult {
    pub models: Vec<ModelWeightEstimate>,
    pub feature_names: Vec<String>,
    pub weights: Vec<f64>,
    pub covariance_matrix: Vec<f64>,
    pub sigma_hat: f64,
    pub effective_contributors: f64,
    pub token_mix: TokenMix,
    pub residuals: Vec<f64>,
    pub assigned_weights: Vec<f64>,
}

pub fn fit_irls_nnls(
    intervals: &[IntervalRecord],
    clients: &HashMap<String, ClientRecord>,
    now: i64,
    model_stale_days: i64,
) -> Option<IrlsFitResult> {
    if intervals.is_empty() {
        return None;
    }

    // 1. Discover models present in the window, their sample counts, solo counts, and last_seen_at
    let mut model_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut solo_counts: HashMap<String, usize> = HashMap::new();
    let mut model_last_seen: HashMap<String, i64> = HashMap::new();
    for iv in intervals {
        let mut seen_in_iv = std::collections::HashSet::new();
        for u in &iv.usage {
            let m = u.model.trim().to_ascii_lowercase();
            if seen_in_iv.insert(m.clone()) {
                *model_counts.entry(m.clone()).or_insert(0) += 1;
            }
            let entry = model_last_seen.entry(m).or_insert(0);
            if iv.ended_at > *entry {
                *entry = iv.ended_at;
            }
        }
        if seen_in_iv.len() == 1
            && let Some(only_m) = seen_in_iv.into_iter().next()
        {
            *solo_counts.entry(only_m).or_insert(0) += 1;
        }
    }

    // Keep models that have >= min_support, and if any model in this bucket has a solo
    // interval, require solo_count >= 1 to exclude cross-bucket spillover models.
    let min_support = if intervals.len() < 10 { 1 } else { 2 };
    let any_has_solo = model_counts
        .keys()
        .any(|m| solo_counts.get(m).copied().unwrap_or(0) >= 1);
    let active_models: Vec<String> = model_counts
        .iter()
        .filter(|&(m, &cnt)| {
            cnt >= min_support && (!any_has_solo || solo_counts.get(m).copied().unwrap_or(0) >= 1)
        })
        .map(|(m, _)| m.clone())
        .collect();

    if active_models.is_empty() {
        return None;
    }

    let n = intervals.len();
    let m_count = active_models.len();
    let p = m_count * 3; // tokens_in, tokens_out, tokens_cached_in per model

    let model_idx: HashMap<&str, usize> = active_models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.as_str(), i))
        .collect();

    let mut feature_names = Vec::with_capacity(p);
    for m in &active_models {
        feature_names.push(format!("{m}:in"));
        feature_names.push(format!("{m}:out"));
        feature_names.push(format!("{m}:cached_in"));
    }

    // 2. Build design matrix X (n x p) in millions of tokens, and target vector y (delta_pct)
    let mut x = DMatrix::zeros(n, p);
    let mut y = DVector::zeros(n);

    for (row, iv) in intervals.iter().enumerate() {
        y[row] = iv.delta_pct;
        for u in &iv.usage {
            let m = u.model.trim().to_ascii_lowercase();
            if let Some(&idx) = model_idx.get(m.as_str()) {
                let base = idx * 3;
                x[(row, base)] += u.tokens_in as f64 / 1_000_000.0;
                x[(row, base + 1)] += u.tokens_out as f64 / 1_000_000.0;
                x[(row, base + 2)] += u.tokens_cached_in as f64 / 1_000_000.0;
            }
        }
    }

    // 3. Initial sample weights: time-decay * inverse quantization variance * client trust
    let lambda = 0.5_f64.powf(1.0 / HALF_LIFE_DAYS);
    let mut init_weights = Vec::with_capacity(n);
    for iv in intervals {
        let dt_days = ((now - iv.ended_at).max(0) as f64) / 86_400.0;
        let time_w = lambda.powf(dt_days);

        let prec = iv.quota_precision.max(1e-6);
        let quant_w = (iv.delta_pct / prec).powi(2).min(1.0);

        let trust_w = clients
            .get(&iv.client_id)
            .map(|c| c.trust_score.clamp(0.01, 1.0))
            .unwrap_or(0.10);

        init_weights.push((time_w * quant_w * trust_w).max(1e-6));
    }

    let mut weights = enforce_client_cap(&init_weights, intervals, CLIENT_CAP_FRAC);
    let mut w_vec = DVector::zeros(p);

    // 4. IRLS loop wrapping monotonic Active-Set NNLS (w_out >= w_in >= w_cached >= 0 per model)
    let num_models = active_models.len();
    for _iter in 0..MAX_IRLS_ITERS {
        let mut a_iso = DMatrix::zeros(n, p);
        let mut b = DVector::zeros(n);
        for i in 0..n {
            let sw = weights[i].sqrt();
            b[i] = y[i] * sw;
            for m_idx in 0..num_models {
                let base = m_idx * 3;
                let t_in = x[(i, base)];
                let t_out = x[(i, base + 1)];
                let t_cache = x[(i, base + 2)];
                a_iso[(i, base)] = (t_in + t_out + t_cache) * sw;
                a_iso[(i, base + 1)] = (t_in + t_out) * sw;
                a_iso[(i, base + 2)] = t_out * sw;
            }
        }

        let delta = solve_nnls(&a_iso, &b);
        let mut next_w = DVector::zeros(p);
        for m_idx in 0..num_models {
            let base = m_idx * 3;
            let d_cache = delta[base];
            let d_in_step = delta[base + 1];
            let d_out_step = delta[base + 2];
            next_w[base] = d_cache + d_in_step;
            next_w[base + 1] = d_cache + d_in_step + d_out_step;
            next_w[base + 2] = d_cache;
        }
        let diff = (&next_w - &w_vec).norm() / next_w.norm().max(1.0);
        w_vec = next_w;

        let preds = &x * &w_vec;
        let residuals: Vec<f64> = (0..n).map(|i| y[i] - preds[i]).collect();
        // Scale residuals by predicted magnitude (floored by quota_precision) so Huber
        // handles heteroscedastic turns without over-penalizing large valid intervals.
        let rel_residuals: Vec<f64> = (0..n)
            .map(|i| {
                let scale = preds[i].abs().max(intervals[i].quota_precision).max(1e-4);
                residuals[i] / scale
            })
            .collect();
        let mad = compute_mad(&rel_residuals).max(1e-4);

        // Asymmetric Huber: tighter clipping on positive residuals (unlogged usage on another device)
        let delta_pos = 0.8 * mad;
        let delta_neg = 1.345 * mad;

        let mut reweighted = Vec::with_capacity(n);
        for i in 0..n {
            let r = rel_residuals[i];
            let huber = if r > 0.0 {
                if r <= delta_pos { 1.0 } else { delta_pos / r }
            } else {
                let abs_r = -r;
                if abs_r <= delta_neg {
                    1.0
                } else {
                    delta_neg / abs_r
                }
            };
            reweighted.push((init_weights[i] * huber).max(1e-8));
        }

        weights = enforce_client_cap(&reweighted, intervals, CLIENT_CAP_FRAC);

        if diff < IRLS_TOL && _iter > 0 {
            break;
        }
    }

    // Final residuals and effective contributors
    let preds = &x * &w_vec;
    let residuals: Vec<f64> = (0..n).map(|i| y[i] - preds[i]).collect();

    let effective_contributors = compute_effective_contributors(&weights, intervals);
    let token_mix = compute_token_mix(&weights, intervals);

    // 5. Uncertainty & Covariance: Sigma_w = sigma_hat^2 * (X_active^T W X_active)^-1
    let sum_w: f64 = weights.iter().sum();
    let sum_w_sq: f64 = weights.iter().map(|w| w * w).sum();
    let n_eff = if sum_w_sq > 0.0 {
        (sum_w * sum_w) / sum_w_sq
    } else {
        n as f64
    };
    let p_active = w_vec.iter().filter(|&&v| v > 1e-9).count().max(1);
    let dof = (n_eff - p_active as f64).max(1.0);

    let weighted_rss: f64 = (0..n)
        .map(|i| weights[i] * residuals[i] * residuals[i])
        .sum();
    // Normalize weighted RSS by average weight so sigma_hat is in quota % units
    let avg_w = (sum_w / n as f64).max(1e-12);
    let sigma_sq = (weighted_rss / avg_w) / dof;
    let sigma_hat = sigma_sq.sqrt();

    // Compute (X^T W X) normalized by avg_w
    let mut xtwx: DMatrix<f64> = DMatrix::zeros(p, p);
    for i in 0..n {
        let wi = weights[i] / avg_w;
        if wi <= 0.0 {
            continue;
        }
        for j in 0..p {
            let xij = x[(i, j)];
            if xij == 0.0 {
                continue;
            }
            for k in j..p {
                let val = wi * xij * x[(i, k)];
                xtwx[(j, k)] += val;
                if j != k {
                    xtwx[(k, j)] += val;
                }
            }
        }
    }

    // Column-normalized, data-driven ridge-damped inverse of (X^T W X) for stable CIs
    // without hardcoding any token cost ratios.
    let mut col_scale = vec![1.0_f64; p];
    for j in 0..p {
        col_scale[j] = xtwx[(j, j)].sqrt().max(1e-6);
    }
    let mut xtwx_norm = DMatrix::zeros(p, p);
    for j in 0..p {
        for k in 0..p {
            xtwx_norm[(j, k)] = xtwx[(j, k)] / (col_scale[j] * col_scale[k]);
        }
    }
    let ridge = (1.0 / n_eff.max(1.0)).clamp(1e-4, 0.05);
    for j in 0..p {
        xtwx_norm[(j, j)] += ridge;
    }

    let svd = xtwx_norm.svd(true, true);
    let mut inv_xtwx = DMatrix::zeros(p, p);
    if let (Some(u), Some(v_t)) = (&svd.u, &svd.v_t) {
        let max_sv = svd.singular_values.iter().cloned().fold(0.0_f64, f64::max);
        let sv_tol = max_sv * 1e-8;
        let mut s_inv = DMatrix::zeros(p, p);
        for i in 0..p {
            let sv = svd.singular_values[i];
            if sv > sv_tol {
                s_inv[(i, i)] = 1.0 / sv;
            }
        }
        let inv_norm = v_t.transpose() * s_inv * u.transpose();
        for j in 0..p {
            for k in 0..p {
                inv_xtwx[(j, k)] = inv_norm[(j, k)] / (col_scale[j] * col_scale[k]);
            }
        }
    }

    let cov = &inv_xtwx * sigma_sq;
    let t_crit = StudentsT::new(0.0, 1.0, dof)
        .map(|dist| dist.inverse_cdf(0.975))
        .unwrap_or(1.96);

    // 6. Build per-model estimates and identifiability
    let stale_cutoff = now - model_stale_days * 86_400;
    let mut models = Vec::with_capacity(m_count);
    for (idx, model_name) in active_models.iter().enumerate() {
        let base = idx * 3;
        let w_in = w_vec[base];
        let w_out = w_vec[base + 1];
        let w_cache = w_vec[base + 2];

        if m_count > 1 && w_in <= 1e-4 && w_out <= 1e-4 && w_cache <= 1e-4 {
            continue;
        }

        let se_in = cov[(base, base)].max(0.0).sqrt();
        let se_out = cov[(base + 1, base + 1)].max(0.0).sqrt();
        let se_cache = cov[(base + 2, base + 2)].max(0.0).sqrt();

        let sample_count = *model_counts.get(model_name).unwrap_or(&0);
        let last_seen_at = *model_last_seen.get(model_name).unwrap_or(&now);

        // Identifiability based on per-model sample count, column leverage/variance, and condition number
        let col_norm_in = x.column(base).norm();
        let col_norm_out = x.column(base + 1).norm();
        let model_sub = x.columns(base, 3).into_owned();
        let model_cond = compute_condition_number(&model_sub);
        let identifiability = if sample_count < 15 {
            Identifiability::InsufficientSamples
        } else if col_norm_in < 1e-6 || col_norm_out < 1e-6 || model_cond > 1_000.0 {
            Identifiability::LowCollinear
        } else if model_cond > 100.0 || sample_count < 30 {
            Identifiability::Moderate
        } else {
            Identifiability::High
        };

        let status = if last_seen_at < stale_cutoff {
            ModelStatus::Retired
        } else {
            ModelStatus::Active
        };
        let model_mix = compute_model_token_mix(&weights, intervals, model_name);

        models.push(ModelWeightEstimate {
            model: model_name.clone(),
            pct_per_1m_in: w_in,
            pct_per_1m_in_ci: ConfidenceInterval {
                lower: (w_in - t_crit * se_in).max(0.0),
                upper: w_in + t_crit * se_in,
            },
            pct_per_1m_out: w_out,
            pct_per_1m_out_ci: ConfidenceInterval {
                lower: (w_out - t_crit * se_out).max(0.0),
                upper: w_out + t_crit * se_out,
            },
            pct_per_1m_cached_in: w_cache,
            pct_per_1m_cached_in_ci: ConfidenceInterval {
                lower: (w_cache - t_crit * se_cache).max(0.0),
                upper: w_cache + t_crit * se_cache,
            },
            identifiability,
            sample_count,
            last_seen_at,
            status,
            token_mix: Some(model_mix),
        });
    }

    let mut covariance_matrix = Vec::with_capacity(p * p);
    for r in 0..p {
        for c in 0..p {
            covariance_matrix.push(cov[(r, c)]);
        }
    }

    Some(IrlsFitResult {
        models,
        feature_names,
        weights: w_vec.as_slice().to_vec(),
        covariance_matrix,
        sigma_hat,
        effective_contributors,
        token_mix,
        residuals,
        assigned_weights: weights,
    })
}

/// Enforces that no single `client_id` exceeds `cap_frac` (5%) of total effective weight.
/// When distinct clients > 20, performs iterative water-filling so every client <= 5% of final sum.
/// When distinct clients <= 20, caps each client's total weight at `cap_frac * total_initial_weight`.
pub fn enforce_client_cap(
    raw_weights: &[f64],
    intervals: &[IntervalRecord],
    cap_frac: f64,
) -> Vec<f64> {
    let n = raw_weights.len();
    if n == 0 {
        return vec![];
    }

    let mut client_totals: HashMap<&str, f64> = HashMap::new();
    let mut total = 0.0;
    for (i, iv) in intervals.iter().enumerate() {
        let w = raw_weights[i].max(0.0);
        *client_totals.entry(iv.client_id.as_str()).or_insert(0.0) += w;
        total += w;
    }

    if total <= 0.0 {
        return vec![0.0; n];
    }

    let num_clients = client_totals.len();
    let mut client_scale: HashMap<&str, f64> = HashMap::new();

    if num_clients as f64 * cap_frac <= 1.0 + 1e-9 {
        // Fewer than ~20 clients: cap each client at cap_frac * total initial weight
        let max_per_client = cap_frac * total;
        for (&cid, &ctot) in &client_totals {
            let scale = if ctot > max_per_client && ctot > 0.0 {
                max_per_client / ctot
            } else {
                1.0
            };
            client_scale.insert(cid, scale);
        }
    } else {
        // >= 21 clients: iterative water-filling so every client <= cap_frac of final total
        let mut capped: HashMap<&str, f64> = client_totals.clone();
        for _ in 0..10 {
            let cur_total: f64 = capped.values().sum();
            let limit = cap_frac * cur_total;
            let mut changed = false;
            for val in capped.values_mut() {
                if *val > limit + 1e-12 {
                    *val = limit;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (&cid, &orig) in &client_totals {
            let target = *capped.get(cid).unwrap_or(&orig);
            let scale = if orig > 0.0 { target / orig } else { 1.0 };
            client_scale.insert(cid, scale);
        }
    }

    intervals
        .iter()
        .enumerate()
        .map(|(i, iv)| {
            let s = client_scale.get(iv.client_id.as_str()).copied().unwrap_or(1.0);
            raw_weights[i] * s
        })
        .collect()
}

pub fn compute_token_mix(weights: &[f64], intervals: &[IntervalRecord]) -> TokenMix {
    compute_token_mix_filtered(weights, intervals, None)
}

pub fn compute_model_token_mix(
    weights: &[f64],
    intervals: &[IntervalRecord],
    model_name: &str,
) -> TokenMix {
    compute_token_mix_filtered(weights, intervals, Some(model_name))
}

fn compute_token_mix_filtered(
    weights: &[f64],
    intervals: &[IntervalRecord],
    model_filter: Option<&str>,
) -> TokenMix {
    let mut sum_in = 0.0;
    let mut sum_out = 0.0;
    let mut sum_cached = 0.0;

    for (i, iv) in intervals.iter().enumerate() {
        let w = weights[i].max(0.0);
        for u in &iv.usage {
            if let Some(target) = model_filter
                && u.model.trim().to_ascii_lowercase() != target
            {
                continue;
            }
            sum_in += w * (u.tokens_in as f64);
            sum_out += w * (u.tokens_out as f64);
            sum_cached += w * (u.tokens_cached_in as f64);
        }
    }

    let total = sum_in + sum_out + sum_cached;
    if total <= 0.0 {
        return TokenMix::default();
    }
    TokenMix {
        in_share: sum_in / total,
        out_share: sum_out / total,
        cached_share: sum_cached / total,
    }
}

pub fn compute_effective_contributors(weights: &[f64], intervals: &[IntervalRecord]) -> f64 {
    let mut client_totals: HashMap<&str, f64> = HashMap::new();
    for (i, iv) in intervals.iter().enumerate() {
        *client_totals.entry(iv.client_id.as_str()).or_insert(0.0) += weights[i].max(0.0);
    }
    let sum_w: f64 = client_totals.values().sum();
    let sum_w_sq: f64 = client_totals.values().map(|w| w * w).sum();
    if sum_w_sq > 0.0 {
        (sum_w * sum_w) / sum_w_sq
    } else {
        0.0
    }
}

pub fn compute_mad(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let med = median(values);
    let abs_devs: Vec<f64> = values.iter().map(|v| (v - med).abs()).collect();
    1.4826 * median(&abs_devs)
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        0.5 * (sorted[mid - 1] + sorted[mid])
    }
}

fn compute_condition_number(x: &DMatrix<f64>) -> f64 {
    let p = x.ncols();
    if p == 0 || x.nrows() == 0 {
        return f64::INFINITY;
    }
    // Column-normalize non-zero columns to measure collinearity independently of feature scale
    let mut x_norm = DMatrix::zeros(x.nrows(), p);
    let mut non_zero_cols = 0usize;
    for j in 0..p {
        let col = x.column(j);
        let norm = col.norm();
        if norm > 1e-12 {
            x_norm.set_column(non_zero_cols, &(col / norm));
            non_zero_cols += 1;
        }
    }
    if non_zero_cols == 0 {
        return f64::INFINITY;
    }
    let sub = x_norm.columns(0, non_zero_cols).into_owned();
    let svd = sub.svd(false, false);
    let svs = svd.singular_values;
    let s_max = svs.iter().cloned().fold(0.0_f64, f64::max);
    let s_min = svs.iter().cloned().fold(f64::INFINITY, f64::min);
    if s_min <= 1e-12 {
        f64::INFINITY
    } else {
        (s_max / s_min).powi(2) // kappa(X^T X) = kappa(X)^2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TrustTier, UsageEntry};

    fn make_client(id: &str, trust_score: f64) -> ClientRecord {
        ClientRecord {
            client_id: id.to_string(),
            created_at: 1_700_000_000,
            subnet_hash: format!("sub_{id}"),
            trust_score,
            trust_tier: TrustTier::Trusted,
            accepted_count: 50,
            ema_abs_z: 0.2,
            last_seen_at: 1_700_100_000,
        }
    }

    #[test]
    fn test_irls_recovers_weights_and_rejects_positive_outliers() {
        let now = 1_700_200_000;
        let mut clients = HashMap::new();
        for c in 0..25 {
            let cid = format!("c{c}");
            clients.insert(cid.clone(), make_client(&cid, 1.0));
        }

        // True weights: in = 2.0, out = 10.0, cached = 0.2 pct per 1M
        let mut intervals = Vec::new();
        for i in 0..60 {
            let cid = format!("c{}", i % 25);
            let t_in = 100_000 + (i as u64 * 17_000) % 500_000;
            let t_out = 20_000 + (i as u64 * 7_000) % 150_000;
            let t_cache = 50_000 + (i as u64 * 13_000) % 300_000;
            let mut delta = 2.0 * (t_in as f64 / 1e6)
                + 10.0 * (t_out as f64 / 1e6)
                + 0.2 * (t_cache as f64 / 1e6);

            // Inject 6 huge positive outliers (unlogged usage on another device)
            if i % 10 == 0 {
                delta += 15.0;
            }

            intervals.push(IntervalRecord {
                id: i as i64 + 1,
                idempotency_key: format!("k{i}"),
                client_id: cid,
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "5h".to_string(),
                started_at: now - 600,
                ended_at: now - 60,
                delta_pct: delta,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "gemini-4".to_string(),
                    tokens_in: t_in,
                    tokens_out: t_out,
                    tokens_cached_in: t_cache,
                    request_count: 1,
                }],
                residual_pct: None,
                weight_assigned: None,
                ingested_at: now - 30,
            });
        }

        let fit = fit_irls_nnls(&intervals, &clients, now, 14).expect("fit should succeed");
        let est = &fit.models[0];
        assert!((est.pct_per_1m_in - 2.0).abs() < 0.2, "in={}", est.pct_per_1m_in);
        assert!((est.pct_per_1m_out - 10.0).abs() < 0.5, "out={}", est.pct_per_1m_out);
        assert!(
            (est.pct_per_1m_cached_in - 0.2).abs() < 0.2,
            "cache={}",
            est.pct_per_1m_cached_in
        );

        // Verify 5% client cap is respected
        let total_w: f64 = fit.assigned_weights.iter().sum();
        let mut by_client: HashMap<&str, f64> = HashMap::new();
        for (i, iv) in intervals.iter().enumerate() {
            *by_client.entry(iv.client_id.as_str()).or_insert(0.0) += fit.assigned_weights[i];
        }
        for (_, cw) in by_client {
            assert!(cw <= 0.0501 * total_w, "client weight {cw} exceeded 5% of {total_w}");
        }
    }

    #[test]
    fn test_irls_enforces_monotonic_weights_on_tiny_sample() {
        let now = 1_700_000_000;
        let mut clients = HashMap::new();
        clients.insert("c1".to_string(), make_client("c1", 1.0));

        let intervals = vec![
            IntervalRecord {
                id: 1,
                idempotency_key: "m1".to_string(),
                client_id: "c1".to_string(),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 200,
                ended_at: now - 100,
                delta_pct: 0.50,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "gemini-4".to_string(),
                    tokens_in: 10_000,
                    tokens_out: 5_000,
                    tokens_cached_in: 1_000_000,
                    request_count: 1,
                }],
                residual_pct: None,
                weight_assigned: None,
                ingested_at: now - 100,
            },
            IntervalRecord {
                id: 2,
                idempotency_key: "m2".to_string(),
                client_id: "c1".to_string(),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 90,
                ended_at: now - 10,
                delta_pct: 1.05,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "gemini-4".to_string(),
                    tokens_in: 20_000,
                    tokens_out: 10_000,
                    tokens_cached_in: 2_000_000,
                    request_count: 1,
                }],
                residual_pct: None,
                weight_assigned: None,
                ingested_at: now - 10,
            },
        ];

        let res = fit_irls_nnls(&intervals, &clients, now, 7).unwrap();
        let m = &res.models[0];
        assert!(m.pct_per_1m_out >= m.pct_per_1m_in);
        assert!(m.pct_per_1m_in >= m.pct_per_1m_cached_in);
        assert!(m.pct_per_1m_cached_in > 0.0);
        assert_eq!(m.identifiability, Identifiability::InsufficientSamples);
        let mix = m.token_mix.expect("per-model token_mix should be populated");
        assert!((mix.in_share + mix.out_share + mix.cached_share - 1.0).abs() < 1e-6);
        assert!(m.pct_per_1m_in_ci.upper.is_finite() && m.pct_per_1m_in_ci.upper < 100.0);
        assert!(m.pct_per_1m_out_ci.upper.is_finite() && m.pct_per_1m_out_ci.upper < 100.0);
    }
}
