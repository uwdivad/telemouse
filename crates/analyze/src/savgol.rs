//! Savitzky–Golay smoothing and differentiation.
//!
//! The plan calls for SG rather than a boxcar so we do not amplify sensor
//! noise when differentiating raw counts into velocity/acceleration/jerk.
//!
//! Rather than hard-coding the textbook 5- and 7-point quadratic kernels this
//! derives the coefficients from the least-squares normal equations, which
//! buys two things:
//!
//! * arbitrary window / polynomial order / derivative order, and
//! * *exact* edge handling — the first and last `half` outputs are evaluated
//!   off-center from the nearest full window instead of being mirrored, so the
//!   defining SG property (a polynomial of degree ≤ `order` passes through
//!   unchanged) holds across the whole series, edges included.
//!
//! For `half = 2, order = 2, deriv = 0` the derived kernel is the familiar
//! `(-3, 12, 17, 12, -3)/35`; for `half = 3` it is `(-2, 3, 6, 7, 6, 3, -2)/21`.

/// A prepared Savitzky–Golay operator: window half-width, polynomial order,
/// derivative order, and the per-offset weight table.
#[derive(Debug, Clone)]
pub struct SavGol {
    half: usize,
    order: usize,
    deriv: usize,
    /// `table[o]` are the weights producing the output whose position inside
    /// its window is `o` (so `table[half]` is the centered kernel).
    table: Vec<Vec<f64>>,
}

impl SavGol {
    /// Build an operator over a `2 * half + 1` sample window.
    ///
    /// Panics if `2 * half < order` (an underdetermined fit).
    pub fn new(half: usize, order: usize, deriv: usize) -> Self {
        assert!(2 * half >= order, "window too small for polynomial order");
        let table = (0..=2 * half)
            .map(|o| coeffs(half, order, deriv, o as f64 - half as f64))
            .collect();
        Self {
            half,
            order,
            deriv,
            table,
        }
    }

    /// Smoothing operator (derivative order 0).
    pub fn smoother(half: usize, order: usize) -> Self {
        Self::new(half, order, 0)
    }

    /// The centered kernel — the coefficients a textbook would print.
    pub fn kernel(&self) -> &[f64] {
        &self.table[self.half]
    }

    /// Apply to `y`, sampled on a uniform grid of spacing `dt`.
    ///
    /// The result is scaled by `dt^-deriv`, so a first-derivative operator on
    /// a 1 ms grid returns units-per-second directly. Series shorter than one
    /// window degrade gracefully: the widest usable window is used, and a
    /// series too short to fit the polynomial returns the input (deriv 0) or
    /// zeros (deriv > 0).
    pub fn apply(&self, y: &[f64], dt: f64) -> Vec<f64> {
        let n = y.len();
        if n == 0 {
            return Vec::new();
        }
        let scale = dt.powi(-(self.deriv as i32));
        if n > 2 * self.half {
            return self.apply_with(&self.table, self.half, y, scale);
        }
        // Short series: shrink the window to what the data supports.
        let h = (n - 1) / 2;
        if 2 * h < self.order {
            return if self.deriv == 0 {
                y.to_vec()
            } else {
                vec![0.0; n]
            };
        }
        let table: Vec<Vec<f64>> = (0..=2 * h)
            .map(|o| coeffs(h, self.order, self.deriv, o as f64 - h as f64))
            .collect();
        self.apply_with(&table, h, y, scale)
    }

    fn apply_with(&self, table: &[Vec<f64>], half: usize, y: &[f64], scale: f64) -> Vec<f64> {
        let n = y.len();
        let last_start = n - (2 * half + 1);
        let mut out = vec![0.0; n];
        for (i, o) in out.iter_mut().enumerate() {
            let start = i.saturating_sub(half).min(last_start);
            let w = &table[i - start];
            let mut acc = 0.0;
            for (k, wk) in w.iter().enumerate() {
                acc += wk * y[start + k];
            }
            *o = acc * scale;
        }
        out
    }
}

/// Weights that evaluate the `deriv`-th derivative of the least-squares
/// polynomial fit at position `eval_at` (in samples, relative to the window
/// center) from the `2 * half + 1` samples of the window.
pub fn coeffs(half: usize, order: usize, deriv: usize, eval_at: f64) -> Vec<f64> {
    let n = 2 * half + 1;
    let p = order + 1;
    let xs: Vec<f64> = (0..n).map(|i| i as f64 - half as f64).collect();

    // Normal matrix M = A^T A with A[i][j] = x_i^j. M[a][b] = sum x_i^(a+b).
    let mut m = vec![0.0; p * p];
    for a in 0..p {
        for b in 0..p {
            m[a * p + b] = xs.iter().map(|x| x.powi((a + b) as i32)).sum();
        }
    }

    // e_j = d^deriv/dx^deriv (x^j) evaluated at eval_at.
    let mut e = vec![0.0; p];
    for (j, ej) in e.iter_mut().enumerate() {
        if j >= deriv {
            let mut falling = 1.0;
            for k in 0..deriv {
                falling *= (j - k) as f64;
            }
            *ej = falling * eval_at.powi((j - deriv) as i32);
        }
    }

    // c = M^-1 e; the sample weight is w_i = sum_j c_j * x_i^j.
    let c = solve(&mut m, &mut e, p);
    xs.iter()
        .map(|x| (0..p).map(|j| c[j] * x.powi(j as i32)).sum())
        .collect()
}

/// Gaussian elimination with partial pivoting; `m` is row-major `n x n`.
fn solve(m: &mut [f64], b: &mut [f64], n: usize) -> Vec<f64> {
    for col in 0..n {
        let mut pivot = col;
        for r in col + 1..n {
            if m[r * n + col].abs() > m[pivot * n + col].abs() {
                pivot = r;
            }
        }
        if pivot != col {
            for k in 0..n {
                m.swap(col * n + k, pivot * n + k);
            }
            b.swap(col, pivot);
        }
        let d = m[col * n + col];
        if d.abs() < f64::EPSILON {
            continue; // singular; leave the row alone rather than dividing by ~0
        }
        for r in col + 1..n {
            let f = m[r * n + col] / d;
            if f == 0.0 {
                continue;
            }
            for k in col..n {
                m[r * n + k] -= f * m[col * n + k];
            }
            b[r] -= f * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut acc = b[i];
        for k in i + 1..n {
            acc -= m[i * n + k] * x[k];
        }
        let d = m[i * n + i];
        x[i] = if d.abs() < f64::EPSILON { 0.0 } else { acc / d };
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) {
        assert!((a - b).abs() <= tol, "{a} != {b} (tol {tol})");
    }

    #[test]
    fn five_point_quadratic_kernel_matches_textbook() {
        let sg = SavGol::smoother(2, 2);
        let want = [-3.0, 12.0, 17.0, 12.0, -3.0].map(|v| v / 35.0);
        for (got, want) in sg.kernel().iter().zip(want) {
            approx(*got, want, 1e-12);
        }
    }

    #[test]
    fn seven_point_quadratic_kernel_matches_textbook() {
        let sg = SavGol::smoother(3, 2);
        let want = [-2.0, 3.0, 6.0, 7.0, 6.0, 3.0, -2.0].map(|v| v / 21.0);
        for (got, want) in sg.kernel().iter().zip(want) {
            approx(*got, want, 1e-12);
        }
    }

    #[test]
    fn kernel_weights_sum_to_one() {
        for half in 2..=5 {
            let sg = SavGol::smoother(half, 2);
            approx(sg.kernel().iter().sum::<f64>(), 1.0, 1e-12);
        }
    }

    /// The defining SG property: a polynomial of degree <= order passes
    /// through unchanged. Edges included, thanks to the off-center evaluation.
    #[test]
    fn smoothing_preserves_a_pure_quadratic_exactly() {
        let f = |i: usize| {
            let x = i as f64;
            3.0 - 0.7 * x + 0.05 * x * x
        };
        let y: Vec<f64> = (0..40).map(f).collect();
        let out = SavGol::smoother(3, 2).apply(&y, 1.0);
        for (i, got) in out.iter().enumerate() {
            approx(*got, f(i), 1e-9);
        }
    }

    #[test]
    fn first_derivative_of_a_quadratic_is_exact() {
        // y = 2 + 3t + 4t^2 on a 1ms grid -> dy/dt = 3 + 8t (per second).
        let dt = 0.001;
        let y: Vec<f64> = (0..30)
            .map(|i| {
                let t = i as f64 * dt;
                2.0 + 3.0 * t + 4.0 * t * t
            })
            .collect();
        let d = SavGol::new(3, 2, 1).apply(&y, dt);
        for (i, got) in d.iter().enumerate() {
            let t = i as f64 * dt;
            approx(*got, 3.0 + 8.0 * t, 1e-6);
        }
    }

    #[test]
    fn second_derivative_of_a_quadratic_is_constant() {
        let dt = 0.001;
        let y: Vec<f64> = (0..30)
            .map(|i| {
                let t = i as f64 * dt;
                4.0 * t * t
            })
            .collect();
        let d = SavGol::new(3, 2, 2).apply(&y, dt);
        for got in &d {
            approx(*got, 8.0, 1e-3);
        }
    }

    /// Smoothing a noisy straight line must recover the underlying slope.
    #[test]
    fn smoothing_a_noisy_line_recovers_the_slope() {
        // Deterministic zero-mean-ish "noise" so the test never flakes.
        let n = 400;
        let slope = 0.25;
        let noise = |i: usize| ((i * 7919 % 101) as f64 - 50.0) / 50.0; // +/-1
        let y: Vec<f64> = (0..n).map(|i| slope * i as f64 + noise(i)).collect();
        let s = SavGol::smoother(5, 2).apply(&y, 1.0);

        // Residual against truth shrinks a lot versus the raw noise.
        let raw_err: f64 = (0..n).map(|i| noise(i).abs()).sum::<f64>() / n as f64;
        let sm_err: f64 = (0..n).map(|i| (s[i] - slope * i as f64).abs()).sum::<f64>() / n as f64;
        assert!(sm_err < raw_err * 0.6, "raw {raw_err} smoothed {sm_err}");

        // And the recovered slope is right.
        let est = (s[n - 20] - s[19]) / ((n - 20) - 19) as f64;
        approx(est, slope, 0.02);
    }

    #[test]
    fn constant_series_has_zero_derivative() {
        let y = vec![7.0; 50];
        for v in SavGol::new(3, 2, 1).apply(&y, 0.001) {
            approx(v, 0.0, 1e-6);
        }
    }

    #[test]
    fn short_series_degrade_gracefully() {
        let sg = SavGol::smoother(3, 2);
        assert!(sg.apply(&[], 1.0).is_empty());
        assert_eq!(sg.apply(&[5.0], 1.0), vec![5.0]);
        // Five samples with a 7-point operator: falls back to a 5-point window
        // and still reproduces a quadratic.
        let y: Vec<f64> = (0..5).map(|i| (i * i) as f64).collect();
        for (i, v) in sg.apply(&y, 1.0).iter().enumerate() {
            approx(*v, (i * i) as f64, 1e-9);
        }
    }
}
