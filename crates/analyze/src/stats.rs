//! Small descriptive-statistics helpers.
//!
//! Deliberately dependency-free: every metric module needs medians and
//! percentiles over `f64` slices and nothing heavier.

/// Length of the vector `(x, y)`.
///
/// Not `f64::hypot`: that calls the CRT's `_hypot`, which is written to be
/// exact for arguments whose squares overflow or flush to zero, and on MSVC it
/// was **8.5% of all analyzer CPU** — more than `kinematics` (see
/// `docs/PERFORMANCE-2026-09-20.md`, A3). Everything this crate measures is a
/// mouse-count quantity: displacements are at most a few thousand counts per
/// grid cell and speeds a few hundred thousand counts/s, so `x*x + y*y` stays
/// around 1e11 against a `f64` range that reaches 1e308. The overflow-safe
/// path buys nothing here and costs a call.
///
/// The results differ from `hypot`'s by at most one ulp, so anything that
/// compares two magnitudes must compare them to a tolerance — which is what
/// the metric tests already do. Grid parity tests that demand exact equality
/// compare two computations that both come through here.
#[inline]
pub fn mag(x: f64, y: f64) -> f64 {
    (x * x + y * y).sqrt()
}

/// Sort a copy of `xs` ascending, dropping non-finite values.
///
/// `sort_unstable_by` rather than `sort_by`: the samples are plain `f64`s with
/// nothing to keep stable, and the stable sort allocates an `n/2` scratch
/// buffer — which on an 8 M-interval quality pass is 32 MB of pure overhead.
/// `f64::total_cmp` is a total order over every bit pattern, so the comparator
/// cannot fail and the old `.expect("finite")` panic branch disappears with it.
pub fn sorted_finite(xs: &[f64]) -> Vec<f64> {
    let mut v: Vec<f64> = xs.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_unstable_by(f64::total_cmp);
    v
}

/// Arithmetic mean, or `None` for an empty/all-NaN slice.
pub fn mean(xs: &[f64]) -> Option<f64> {
    let mut n = 0usize;
    let mut sum = 0.0;
    for &x in xs {
        if x.is_finite() {
            sum += x;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f64)
}

/// Population standard deviation.
pub fn stddev(xs: &[f64]) -> Option<f64> {
    let m = mean(xs)?;
    let mut n = 0usize;
    let mut acc = 0.0;
    for &x in xs {
        if x.is_finite() {
            acc += (x - m) * (x - m);
            n += 1;
        }
    }
    (n > 0).then(|| (acc / n as f64).sqrt())
}

/// Root mean square.
pub fn rms(xs: &[f64]) -> Option<f64> {
    let mut n = 0usize;
    let mut acc = 0.0;
    for &x in xs {
        if x.is_finite() {
            acc += x * x;
            n += 1;
        }
    }
    (n > 0).then(|| (acc / n as f64).sqrt())
}

/// Linear-interpolated percentile, `q` in `[0, 1]`.
pub fn percentile(xs: &[f64], q: f64) -> Option<f64> {
    let v = sorted_finite(xs);
    percentile_sorted(&v, q)
}

/// Percentile over an already-sorted, all-finite slice.
pub fn percentile_sorted(v: &[f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    if v.len() == 1 {
        return Some(v[0]);
    }
    let q = q.clamp(0.0, 1.0);
    let pos = q * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(v[lo] + (v[hi] - v[lo]) * frac)
}

/// The `q` percentile of the finite sample `v` (same definition as
/// [`percentile_sorted`]) by selection instead of sorting. `v[..from]` must
/// already hold the `from` smallest elements (in any order) — true after a
/// previous call returned `from` as its `lo` — so the partition only has to
/// touch `v[from..]`. Returns the value and the index `lo` of the lower
/// order statistic it used; `v` is left partitioned around it.
fn percentile_select(v: &mut [f64], from: usize, q: f64) -> (f64, usize) {
    let n = v.len();
    debug_assert!(n > 0);
    if n == 1 {
        return (v[0], 0);
    }
    let q = q.clamp(0.0, 1.0);
    let pos = q * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    let from = from.min(lo);
    let tail = &mut v[from..];
    let (_, &mut lo_val, right) = tail.select_nth_unstable_by(lo - from, f64::total_cmp);
    if hi == lo || frac == 0.0 {
        return (lo_val, lo);
    }
    // `hi == lo + 1 < n`, so the right partition is non-empty and its minimum
    // is exactly what a sorted slice would hold at `hi`.
    let hi_val = right.iter().copied().fold(f64::INFINITY, f64::min);
    (lo_val + (hi_val - lo_val) * frac, lo)
}

pub fn median(xs: &[f64]) -> Option<f64> {
    percentile(xs, 0.5)
}

pub fn max(xs: &[f64]) -> Option<f64> {
    xs.iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold(None, |acc: Option<f64>, x| {
            Some(acc.map_or(x, |a| a.max(x)))
        })
}

pub fn min(xs: &[f64]) -> Option<f64> {
    xs.iter()
        .copied()
        .filter(|x| x.is_finite())
        .fold(None, |acc: Option<f64>, x| {
            Some(acc.map_or(x, |a| a.min(x)))
        })
}

/// The five-number-ish summary attached to most metric groups.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub n: usize,
    pub mean: f64,
    pub median: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub stddev: f64,
}

impl Summary {
    /// Summarize a sample. Returns an all-zero summary with `n == 0` for an
    /// empty input so downstream JSON stays shape-stable.
    ///
    /// The three quantiles come from `select_nth_unstable` (O(n), and each
    /// later one only partitions the tail the previous one left) rather than
    /// a full sort: on a long session the kinematics samples are millions of
    /// cells each, and the sort was the phase's dominant cost. Results are
    /// identical to sorting first — the selected element and the minimum of
    /// its right partition are exactly the two order statistics the linear
    /// interpolation in [`percentile_sorted`] reads.
    pub fn of(xs: &[f64]) -> Self {
        Self::of_vec(xs.iter().copied().filter(|x| x.is_finite()).collect())
    }

    /// [`Summary::of`] for a sample the caller owns: the quantile selection
    /// partitions `v` in place instead of copying it first. The kinematics and
    /// click passes build their sample vectors and never look at them again,
    /// so the copy was pure duplication — several million `f64`s on a long
    /// session.
    pub fn of_vec(mut v: Vec<f64>) -> Self {
        v.retain(|x| x.is_finite());
        if v.is_empty() {
            return Self::of_sorted(&v);
        }
        let n = v.len();
        let mean = mean(&v).unwrap_or(0.0);
        let stddev = stddev(&v).unwrap_or(0.0);
        let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut from = 0usize;
        let mut q = [0.0f64; 3];
        for (slot, quantile) in q.iter_mut().zip([0.5, 0.90, 0.99]) {
            let (value, lo) = percentile_select(&mut v, from, quantile);
            *slot = value;
            from = lo;
        }
        Self {
            n,
            mean,
            median: q[0],
            p90: q[1],
            p99: q[2],
            max,
            stddev,
        }
    }

    /// Summarize an already-sorted, all-finite sample without re-sorting it.
    pub fn of_sorted(v: &[f64]) -> Self {
        if v.is_empty() {
            return Self {
                n: 0,
                mean: 0.0,
                median: 0.0,
                p90: 0.0,
                p99: 0.0,
                max: 0.0,
                stddev: 0.0,
            };
        }
        Self {
            n: v.len(),
            mean: mean(v).unwrap_or(0.0),
            median: percentile_sorted(v, 0.5).unwrap_or(0.0),
            p90: percentile_sorted(v, 0.90).unwrap_or(0.0),
            p99: percentile_sorted(v, 0.99).unwrap_or(0.0),
            max: *v.last().unwrap(),
            stddev: stddev(v).unwrap_or(0.0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The summary of `k * x` for every sample `x` behind this summary, for
    /// `k > 0`. Every field is order-preserving-linear in the sample (mean,
    /// percentiles, max, stddev all scale by `k`; `n` is unchanged), so a
    /// unit conversion never needs a second sort of the sample — which for
    /// kinematics on a long session was six extra sorts of several million
    /// `f64`s each. Agrees with `Summary::of` over the scaled sample to
    /// floating-point rounding (see the test).
    pub fn scaled(&self, k: f64) -> Self {
        debug_assert!(k > 0.0, "scaled() needs a positive factor, got {k}");
        Self {
            n: self.n,
            mean: self.mean * k,
            median: self.median * k,
            p90: self.p90 * k,
            p99: self.p99 * k,
            max: self.max * k,
            stddev: self.stddev * k,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Summary::of` (selection) must match summarizing the sorted sample —
    /// bit-identical order statistics — across sizes that exercise every interpolation branch
    /// (exact positions, fractional positions, n = 1, 2, ties, NaN dropped).
    #[test]
    fn selection_summary_is_bit_identical_to_sorted_summary() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 10_000) as f64 * 0.125
        };
        for n in [
            1usize, 2, 3, 4, 5, 7, 10, 11, 99, 100, 101, 1000, 1001, 4097,
        ] {
            let mut xs: Vec<f64> = (0..n).map(|_| next()).collect();
            if n > 5 {
                xs[2] = f64::NAN;
                xs[3] = xs[4]; // a tie
                xs[1] = f64::INFINITY;
            }
            let via_sort = Summary::of_sorted(&sorted_finite(&xs));
            let via_select = Summary::of(&xs);
            // Order statistics are exact; mean/stddev are summed in a
            // different order and may differ by rounding.
            assert_eq!(via_sort.n, via_select.n, "n={n}");
            assert_eq!(
                via_sort.median.to_bits(),
                via_select.median.to_bits(),
                "n={n}"
            );
            assert_eq!(via_sort.p90.to_bits(), via_select.p90.to_bits(), "n={n}");
            assert_eq!(via_sort.p99.to_bits(), via_select.p99.to_bits(), "n={n}");
            assert_eq!(via_sort.max.to_bits(), via_select.max.to_bits(), "n={n}");
            assert!((via_sort.mean - via_select.mean).abs() <= 1e-9 * via_sort.mean.abs().max(1.0));
            assert!(
                (via_sort.stddev - via_select.stddev).abs()
                    <= 1e-9 * via_sort.stddev.abs().max(1.0)
            );
        }
    }

    /// The in-place form must be the same summary, not merely a similar one.
    #[test]
    fn of_vec_matches_of_on_the_same_sample() {
        let xs: Vec<f64> = (0..1001)
            .map(|i| ((i * 7919) % 997) as f64 * 0.5)
            .chain([f64::NAN, f64::INFINITY])
            .collect();
        assert_eq!(Summary::of_vec(xs.clone()), Summary::of(&xs));
        assert_eq!(Summary::of_vec(Vec::new()), Summary::of(&[]));
        assert_eq!(Summary::of_vec(vec![f64::NAN]), Summary::of(&[]));
    }

    #[test]
    fn scaled_summary_matches_summary_of_scaled_sample() {
        let xs: Vec<f64> = (0..1000)
            .map(|i| ((i * 7919) % 1000) as f64 * 0.37 + 1.5)
            .collect();
        let k = 2.54 / 1600.0;
        let direct = Summary::of(&xs.iter().map(|x| x * k).collect::<Vec<_>>());
        let scaled = Summary::of(&xs).scaled(k);
        assert_eq!(direct.n, scaled.n);
        for (a, b) in [
            (direct.mean, scaled.mean),
            (direct.median, scaled.median),
            (direct.p90, scaled.p90),
            (direct.p99, scaled.p99),
            (direct.max, scaled.max),
            (direct.stddev, scaled.stddev),
        ] {
            assert!((a - b).abs() <= 1e-12 * a.abs().max(1.0), "{a} vs {b}");
        }
        // Empty stays empty and shape-stable.
        assert_eq!(Summary::of(&[]).scaled(k), Summary::of(&[]));
    }

    #[test]
    fn mean_median_basic() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(mean(&xs), Some(2.5));
        assert_eq!(median(&xs), Some(2.5));
        assert_eq!(max(&xs), Some(4.0));
        assert_eq!(min(&xs), Some(1.0));
    }

    #[test]
    fn median_odd_length_is_exact_middle() {
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
    }

    #[test]
    fn percentile_endpoints_and_interpolation() {
        let xs = [0.0, 10.0];
        assert_eq!(percentile(&xs, 0.0), Some(0.0));
        assert_eq!(percentile(&xs, 1.0), Some(10.0));
        assert_eq!(percentile(&xs, 0.25), Some(2.5));
    }

    #[test]
    fn rms_and_stddev() {
        assert_eq!(rms(&[3.0, 4.0]), Some(((9.0 + 16.0) / 2.0f64).sqrt()));
        assert_eq!(stddev(&[2.0, 2.0, 2.0]), Some(0.0));
    }

    #[test]
    fn empty_inputs_are_none_and_summary_is_zeroed() {
        assert_eq!(mean(&[]), None);
        assert_eq!(median(&[]), None);
        let s = Summary::of(&[]);
        assert!(s.is_empty());
        assert_eq!(s.max, 0.0);
    }

    #[test]
    fn nan_values_are_ignored() {
        let xs = [1.0, f64::NAN, 3.0];
        assert_eq!(mean(&xs), Some(2.0));
        assert_eq!(Summary::of(&xs).n, 2);
    }
}
