//! Small descriptive-statistics helpers.
//!
//! Deliberately dependency-free: every metric module needs medians and
//! percentiles over `f64` slices and nothing heavier.

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
    pub fn of(xs: &[f64]) -> Self {
        Self::of_sorted(&sorted_finite(xs))
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
