use crate::math::nnls::solve_nnls;
use crate::models::{
    ConfidenceInterval, Identifiability, IntervalRecord, ModelStatus, ModelWeightEstimate, TokenMix,
};
use nalgebra::{DMatrix, DVector};
use statrs::distribution::{ContinuousCDF, StudentsT};
use std::collections::{BTreeMap, HashMap};

const HALF_LIFE_DAYS: f64 = 14.0;
const MAX_IRLS_ITERS: usize = 15;
const IRLS_TOL: f64 = 1e-5;

#[derive(Debug, Clone)]
pub struct IrlsFitResult {
    pub models: Vec<ModelWeightEstimate>,
    pub feature_names: Vec<String>,
    pub weights: Vec<f64>,
    pub covariance_matrix: Vec<f64>,
    pub sigma_hat: f64,
    pub token_mix: TokenMix,
    pub residuals: Vec<f64>,
    pub assigned_weights: Vec<f64>,
}

pub fn fit_irls_nnls(
    intervals: &[IntervalRecord],
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

    // 3. Initial sample weights: time-decay * inverse quantization variance
    let lambda = 0.5_f64.powf(1.0 / HALF_LIFE_DAYS);
    let mut init_weights = Vec::with_capacity(n);
    for iv in intervals {
        let dt_days = ((now - iv.ended_at).max(0) as f64) / 86_400.0;
        let time_w = lambda.powf(dt_days);

        let prec = iv.quota_precision.max(1e-6);
        let quant_w = (iv.delta_pct / prec).powi(2).min(1.0);

        init_weights.push((time_w * quant_w).max(1e-6));
    }

    let mut weights = init_weights.clone();
    let mut w_vec = DVector::zeros(p);

    // 3b. High-breakdown LTS (Least Trimmed Squares) seed when n >= 12:
    // Prevents a 20-35% cluster of positive unlogged-usage spikes from pulling
    // iteration-0 OLS into a bad local basin and inflating MAD.
    if n >= 12 && p <= 12 {
        let h = ((n as f64) * 0.60).ceil() as usize;
        let h = h.max(p + 1).min(n);

        let solve_subset = |subset_weights: &[f64]| -> DVector<f64> {
            let mut a_iso = DMatrix::zeros(n, p);
            let mut b = DVector::zeros(n);
            for i in 0..n {
                let sw = subset_weights[i].sqrt();
                b[i] = y[i] * sw;
                for m_idx in 0..m_count {
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
            let mut w = DVector::zeros(p);
            for m_idx in 0..m_count {
                let base = m_idx * 3;
                let d_cache = delta[base];
                let d_in_step = delta[base + 1];
                let d_out_step = delta[base + 2];
                w[base] = d_cache + d_in_step;
                w[base + 1] = d_cache + d_in_step + d_out_step;
                w[base + 2] = d_cache;
            }
            w
        };

        let eval_rel_sq = |w: &DVector<f64>| -> Vec<(usize, f64)> {
            let preds = &x * w;
            (0..n)
                .map(|i| {
                    let scale = preds[i].abs().max(intervals[i].quota_precision).max(1e-4);
                    let r = (y[i] - preds[i]) / scale;
                    (i, r * r)
                })
                .collect()
        };

        // Generate deterministic candidate seeds:
        // 1) full init_weights, 2) lower-y/token halves, 3) strided windows of size h
        let mut seeds: Vec<Vec<f64>> = Vec::new();
        seeds.push(weights.clone());

        let mut by_rate: Vec<(usize, f64)> = (0..n)
            .map(|i| {
                let tok_sum: f64 = (0..p).map(|j| x[(i, j)]).sum::<f64>().max(1e-6);
                (i, y[i] / tok_sum)
            })
            .collect();
        by_rate.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut lower_half = vec![1e-8_f64; n];
        for &(idx, _) in by_rate.iter().take(h) {
            lower_half[idx] = weights[idx];
        }
        seeds.push(lower_half);

        let num_strides = 6.min(n);
        for s in 0..num_strides {
            let mut stride_w = vec![1e-8_f64; n];
            let mut count = 0usize;
            for k in 0..n {
                let idx = (s + k * num_strides) % n;
                if count < h {
                    stride_w[idx] = weights[idx];
                    count += 1;
                }
            }
            seeds.push(stride_w);
        }

        let mut best_obj = f64::INFINITY;
        let mut best_weights = weights.clone();
        for seed in seeds {
            let mut cur_w = seed;
            let mut cur_vec = solve_subset(&cur_w);
            for _c_step in 0..3 {
                let mut errs = eval_rel_sq(&cur_vec);
                errs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                let mut next_w = vec![1e-8_f64; n];
                for &(idx, _) in errs.iter().take(h) {
                    next_w[idx] = init_weights[idx];
                }
                cur_w = next_w;
                cur_vec = solve_subset(&cur_w);
            }
            let mut final_errs = eval_rel_sq(&cur_vec);
            final_errs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let trimmed_sum: f64 = final_errs.iter().take(h).map(|x| x.1).sum();
            if trimmed_sum < best_obj {
                best_obj = trimmed_sum;
                best_weights = cur_w;
            }
        }
        weights = best_weights;
    }

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

        // Asymmetric Huber: tighter clipping on positive residuals (unlogged usage on another device).
        // After 5 Huber warmup iterations (when n >= 10), redescend extreme positive outliers
        // (e.g. transient reservation spikes) to 1e-4 so high-leverage spikes cannot pull weights.
        let delta_pos = 0.8 * mad;
        let delta_neg = 1.345 * mad;
        let use_tukey_pos = _iter >= 5 && n >= 10;
        let tukey_pos_cutoff = 4.0 * mad.max(0.05);

        let mut reweighted = Vec::with_capacity(n);
        for i in 0..n {
            let r = rel_residuals[i];
            let huber = if r > 0.0 {
                if use_tukey_pos && r > tukey_pos_cutoff && residuals[i] > 0.05 {
                    1e-4
                } else if r <= delta_pos {
                    1.0
                } else {
                    delta_pos / r
                }
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

        weights = reweighted;

        if diff < IRLS_TOL && _iter > 0 && (n < 10 || _iter >= 6) {
            break;
        }
    }

    // Final residuals and token mix
    let preds = &x * &w_vec;
    let residuals: Vec<f64> = (0..n).map(|i| y[i] - preds[i]).collect();

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
        token_mix,
        residuals,
        assigned_weights: weights,
    })
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
    use crate::models::UsageEntry;

    #[test]
    fn test_irls_recovers_weights_and_rejects_positive_outliers() {
        let now = 1_700_200_000;

        // True weights: in = 2.0, out = 10.0, cached = 0.2 pct per 1M
        let mut intervals = Vec::new();
        for i in 0..60 {
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
                ingested_at: now - 30,
            });
        }

        let fit = fit_irls_nnls(&intervals, now, 14).expect("fit should succeed");
        let est = &fit.models[0];
        assert!((est.pct_per_1m_in - 2.0).abs() < 0.2, "in={}", est.pct_per_1m_in);
        assert!((est.pct_per_1m_out - 10.0).abs() < 0.5, "out={}", est.pct_per_1m_out);
        assert!(
            (est.pct_per_1m_cached_in - 0.2).abs() < 0.2,
            "cache={}",
            est.pct_per_1m_cached_in
        );
    }

    #[test]
    fn test_irls_enforces_monotonic_weights_on_tiny_sample() {
        let now = 1_700_000_000;

        let intervals = vec![
            IntervalRecord {
                id: 1,
                idempotency_key: "m1".to_string(),
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
                ingested_at: now - 100,
            },
            IntervalRecord {
                id: 2,
                idempotency_key: "m2".to_string(),
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
                ingested_at: now - 10,
            },
        ];

        let res = fit_irls_nnls(&intervals, now, 7).unwrap();
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

    #[test]
    fn test_irls_lts_seed_resists_30pct_positive_spikes() {
        let now = 1_700_000_000;

        let mut intervals = Vec::new();
        for i in 0..20 {
            let t_in = 3_000 + (i as u64 * 2_500);
            let t_out = 500 + ((i % 4) as u64 * 1_200);
            let t_cache = 100_000 + ((i % 5) as u64 * 200_000);
            let base_pct =
                (t_in as f64 * 5.0 + t_out as f64 * 25.0 + t_cache as f64 * 0.25) / 1_000_000.0;
            // 30% of intervals (6/20) have +0.50% or +1.00% extra unlogged spikes
            let spike = if i % 3 == 0 && i < 18 {
                if i % 2 == 0 { 1.0 } else { 0.5 }
            } else {
                0.0
            };
            intervals.push(IntervalRecord {
                id: i as i64 + 1,
                idempotency_key: format!("lts-{i}"),
                provider: "antigravity".to_string(),
                tier: "pro".to_string(),
                window_type: "gemini-5h".to_string(),
                started_at: now - 1000 + i * 20,
                ended_at: now - 990 + i * 20,
                delta_pct: base_pct + spike,
                quota_precision: 0.0001,
                usage: vec![UsageEntry {
                    model: "gemini-4".to_string(),
                    tokens_in: t_in,
                    tokens_out: t_out,
                    tokens_cached_in: t_cache,
                    request_count: 1,
                }],
                ingested_at: now - 990 + i * 20,
            });
        }

        let res = fit_irls_nnls(&intervals, now, 14).unwrap();
        let m = &res.models[0];
        assert!((m.pct_per_1m_in - 5.0).abs() < 0.1, "in={}", m.pct_per_1m_in);
        assert!(
            (m.pct_per_1m_out - 25.0).abs() < 0.2,
            "out={}",
            m.pct_per_1m_out
        );
        assert!(
            (m.pct_per_1m_cached_in - 0.25).abs() < 0.01,
            "cached={}",
            m.pct_per_1m_cached_in
        );
    }
}
