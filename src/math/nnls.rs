use nalgebra::{DMatrix, DVector};

/// Lawson-Hanson Active-Set Non-Negative Least Squares (NNLS) solver.
/// Solves: min_{w >= 0} || A w - b ||_2^2
pub fn solve_nnls(a: &DMatrix<f64>, b: &DVector<f64>) -> DVector<f64> {
    let n = a.nrows();
    let p = a.ncols();
    if n == 0 || p == 0 {
        return DVector::zeros(p);
    }

    let mut w = DVector::zeros(p);
    let mut passive = vec![false; p]; // true = in passive (unconstrained) set P; false = active set Z (w_j = 0)

    let scale = a.norm().max(b.norm()).max(1.0);
    let tol = 1e-10 * scale * (p as f64).max(1.0);
    let max_outer = 5 * p + 20;

    for _ in 0..max_outer {
        let residual = b - a * &w;
        let grad = a.transpose() * &residual;

        // Find variable in active set Z with largest positive gradient
        let mut best_idx = None;
        let mut best_grad = tol;
        for j in 0..p {
            if !passive[j] && grad[j] > best_grad {
                best_grad = grad[j];
                best_idx = Some(j);
            }
        }

        let Some(t) = best_idx else {
            break;
        };
        passive[t] = true;

        // Inner loop: solve unconstrained LS on passive set P and step back if any s_j <= 0
        let max_inner = 5 * p + 20;
        for _ in 0..max_inner {
            let p_indices: Vec<usize> = (0..p).filter(|&j| passive[j]).collect();
            if p_indices.is_empty() {
                break;
            }

            let mut a_p = DMatrix::zeros(n, p_indices.len());
            for (col_idx, &j) in p_indices.iter().enumerate() {
                a_p.set_column(col_idx, &a.column(j));
            }

            // Solve a_p * s_p = b via SVD pseudo-inverse for numerical stability on collinear columns
            let svd = a_p.svd(true, true);
            let s_p = match svd.solve(b, 1e-12) {
                Ok(sol) => sol,
                Err(_) => break,
            };

            let mut s = DVector::zeros(p);
            for (col_idx, &j) in p_indices.iter().enumerate() {
                s[j] = s_p[col_idx];
            }

            if p_indices.iter().all(|&j| s[j] > 0.0) {
                w = s;
                break;
            }

            // Compute maximum feasible step alpha in [0, 1]
            let mut alpha = f64::INFINITY;
            for &j in &p_indices {
                if s[j] <= 0.0 {
                    let denom = w[j] - s[j];
                    if denom > 0.0 {
                        let ratio = w[j] / denom;
                        if ratio < alpha {
                            alpha = ratio;
                        }
                    } else {
                        alpha = 0.0;
                    }
                }
            }
            if !alpha.is_finite() {
                alpha = 0.0;
            }

            w = &w + alpha * (&s - &w);

            // Move variables with w[j] <= 1e-12 back to active set Z
            let mut moved = false;
            for &j in &p_indices {
                if w[j] <= 1e-12 {
                    w[j] = 0.0;
                    passive[j] = false;
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
    }

    // Clamp tiny numerical noise
    for j in 0..p {
        if w[j] < 0.0 {
            w[j] = 0.0;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nnls_recovers_positive_and_clamps_negative() {
        // y = 2.0 * x1 + 0.0 * x2 + 0.5 * x3 (where unconstrained LS would give negative for x2)
        let a = DMatrix::from_row_slice(
            6,
            3,
            &[
                1.0, 0.2, 0.0,
                0.0, 1.0, 0.5,
                2.0, 0.1, 1.0,
                1.0, 0.5, 2.0,
                3.0, 0.0, 1.0,
                0.5, 0.8, 0.2,
            ],
        );
        let true_w = DVector::from_vec(vec![2.0, 0.0, 0.5]);
        let mut b = &a * &true_w;
        // Perturb slightly so unconstrained x2 would be negative
        b[1] -= 0.1;

        let w = solve_nnls(&a, &b);
        assert!(w.iter().all(|&v| v >= 0.0));
        assert!((w[0] - 2.0).abs() < 0.1);
        assert!(w[1] < 0.05);
        assert!((w[2] - 0.5).abs() < 0.1);
    }
}
