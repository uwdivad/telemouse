//! The latency histogram every component reports from.
//!
//! The capture agent measures capture→ship, the viz bridge measures
//! capture→browser, and until now each had its own histogram with its own
//! bucketing (log2 in one, 250µs-linear in the other), so a p99 from one
//! could not be compared with a p99 from the other. This is the shared one:
//! linear 250µs buckets over 0–256ms, which is the range the plan's <10ms
//! end-to-end target actually lives in, at a resolution fine enough to tell
//! 2ms from 4ms.
//!
//! Recording is one relaxed atomic add — cheap enough for the shipping
//! thread's per-batch path. Reading walks 1024 atomics, which only the
//! periodic reporter and the stats endpoints do.

use std::sync::atomic::{AtomicU64, Ordering};

/// Width of one bucket, in µs.
pub const BUCKET_US: u64 = 250;
/// Bucket count. The last one is the overflow bucket: it has no upper bound.
pub const BUCKETS: usize = 1024;
/// Lower bound of the overflow bucket (255_750µs ≈ 256ms). A sample at or
/// above this is not a latency measurement any more, it is a stall.
pub const OVERFLOW_FROM_US: u64 = (BUCKETS as u64 - 1) * BUCKET_US;

/// A lock-free fixed-bucket histogram of microsecond latencies.
#[derive(Debug)]
pub struct LatencyHist {
    counts: [AtomicU64; BUCKETS],
}

impl Default for LatencyHist {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHist {
    /// An empty histogram, `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; BUCKETS],
        }
    }

    /// Record one observation, in µs. Anything past [`OVERFLOW_FROM_US`]
    /// lands in the overflow bucket.
    pub fn record(&self, us: u64) {
        let idx = ((us / BUCKET_US) as usize).min(BUCKETS - 1);
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
    }

    /// Record a signed observation, returning true when it was negative.
    ///
    /// Latency computed across two machines' clocks can come out negative
    /// (skew, not time travel). Clamping to zero keeps the sample, and the
    /// return value lets the caller count how often it happened — a nonzero
    /// count is the tell that the percentiles need a grain of salt.
    pub fn record_clamped(&self, us: i64) -> bool {
        self.record(us.max(0) as u64);
        us < 0
    }

    /// Read the histogram without disturbing it. Counters are monotonic, so
    /// a window is [`LatencySnapshot::delta`] between two of these.
    pub fn snapshot(&self) -> LatencySnapshot {
        let mut counts = [0u64; BUCKETS];
        for (slot, c) in counts.iter_mut().zip(self.counts.iter()) {
            *slot = c.load(Ordering::Relaxed);
        }
        LatencySnapshot { counts }
    }
}

/// Point-in-time copy of a [`LatencyHist`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencySnapshot {
    pub counts: [u64; BUCKETS],
}

impl Default for LatencySnapshot {
    fn default() -> Self {
        Self {
            counts: [0; BUCKETS],
        }
    }
}

/// A percentile read off the bucket boundaries: the real value is at most
/// `us`, unless the quantile fell in the overflow bucket, where all that is
/// known is that it is at least `us`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentile {
    /// Upper bound of the bucket the quantile fell in — or, when
    /// `saturated`, the overflow bucket's lower bound.
    pub us: u64,
    /// The quantile landed in the overflow bucket.
    pub saturated: bool,
}

impl std::fmt::Display for Percentile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.saturated {
            write!(f, ">={}", self.us)
        } else {
            write!(f, "{}", self.us)
        }
    }
}

impl LatencySnapshot {
    /// Samples recorded.
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The `q` quantile (`0.0..=1.0`), or `None` when nothing was recorded —
    /// an empty histogram has no p99, and reporting one as `0` reads as
    /// "instant" rather than "no data".
    pub fn percentile(&self, q: f64) -> Option<Percentile> {
        let total = self.total();
        if total == 0 {
            return None;
        }
        let q = if q.is_finite() {
            q.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let rank = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Some(at_bucket(i));
            }
        }
        Some(at_bucket(BUCKETS - 1))
    }

    /// The `q` quantile in µs, `0` when nothing was recorded — the shape the
    /// periodic report lines want, where a bare number is all that fits.
    pub fn percentile_us(&self, q: f64) -> u64 {
        self.percentile(q).map_or(0, |p| p.us)
    }

    /// Counts recorded since `prev`, for a per-interval report.
    pub fn delta(&self, prev: &Self) -> Self {
        let mut counts = [0u64; BUCKETS];
        for (i, slot) in counts.iter_mut().enumerate() {
            *slot = self.counts[i].saturating_sub(prev.counts[i]);
        }
        Self { counts }
    }

    /// Upper bound of the highest bucket that recorded anything, `None` when
    /// nothing did. The overflow bucket reports its *lower* bound, since it
    /// has no upper one — see [`Self::percentile`] for the exact value.
    pub fn max_us(&self) -> Option<u64> {
        self.counts
            .iter()
            .rposition(|&c| c > 0)
            .map(|i| at_bucket(i).us)
    }
}

/// The reportable value of bucket `i`.
fn at_bucket(i: usize) -> Percentile {
    if i >= BUCKETS - 1 {
        Percentile {
            us: OVERFLOW_FROM_US,
            saturated: true,
        }
    } else {
        Percentile {
            us: (i as u64 + 1) * BUCKET_US,
            saturated: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_histogram_has_no_percentile() {
        let s = LatencyHist::new().snapshot();
        assert_eq!(s.total(), 0);
        assert_eq!(s.percentile(0.5), None);
        assert_eq!(s.percentile_us(0.99), 0);
        assert_eq!(s.max_us(), None);
        assert_eq!(s, LatencySnapshot::default());
    }

    #[test]
    fn samples_land_in_the_bucket_that_contains_them() {
        let h = LatencyHist::new();
        h.record(0);
        h.record(249);
        h.record(250);
        let s = h.snapshot();
        assert_eq!(s.counts[0], 2, "0 and 249 share the first bucket");
        assert_eq!(s.counts[1], 1);
        assert_eq!(s.total(), 3);
    }

    #[test]
    fn percentiles_report_the_bucket_upper_bound() {
        let h = LatencyHist::new();
        for _ in 0..99 {
            h.record(1_000);
        }
        h.record(50_000);
        let s = h.snapshot();
        assert_eq!(s.total(), 100);
        // 1000µs is the first value of bucket 4 (1000..1250) -> 1250.
        let p50 = s.percentile(0.50).unwrap();
        assert_eq!((p50.us, p50.saturated), (1_250, false));
        assert_eq!(p50.to_string(), "1250");
        // The 100th sample is the outlier, so p99 is still the fast bucket.
        assert_eq!(s.percentile_us(0.99), 1_250);
        assert_eq!(s.percentile_us(1.0), 50_250);
        assert_eq!(s.max_us(), Some(50_250));
    }

    #[test]
    fn the_last_bucket_saturates_and_says_so() {
        let h = LatencyHist::new();
        h.record(5_000_000); // 5 s
        let s = h.snapshot();
        let p = s.percentile(0.5).unwrap();
        assert!(p.saturated);
        assert_eq!(p.us, OVERFLOW_FROM_US);
        assert_eq!(p.to_string(), ">=255750");
        assert_eq!(s.max_us(), Some(OVERFLOW_FROM_US));
        // The boundary itself is already the overflow bucket.
        let h = LatencyHist::new();
        h.record(OVERFLOW_FROM_US);
        assert!(h.snapshot().percentile(1.0).unwrap().saturated);
        let h = LatencyHist::new();
        h.record(OVERFLOW_FROM_US - 1);
        assert!(!h.snapshot().percentile(1.0).unwrap().saturated);
    }

    #[test]
    fn out_of_range_quantiles_are_clamped() {
        let h = LatencyHist::new();
        h.record(100);
        let s = h.snapshot();
        assert_eq!(s.percentile_us(-1.0), 250);
        assert_eq!(s.percentile_us(2.0), 250);
        assert_eq!(s.percentile_us(f64::NAN), 250);
    }

    #[test]
    fn a_window_is_the_difference_between_two_snapshots() {
        let h = LatencyHist::new();
        h.record(100);
        let first = h.snapshot();
        h.record(10_000);
        let window = h.snapshot().delta(&first);
        assert_eq!(window.total(), 1);
        assert_eq!(window.percentile_us(0.5), 10_250);
        // A stale "previous" can never produce a negative count.
        assert_eq!(first.delta(&h.snapshot()).total(), 0);
    }

    #[test]
    fn negative_samples_are_clamped_and_reported() {
        let h = LatencyHist::new();
        assert!(h.record_clamped(-5_000));
        assert!(!h.record_clamped(1_000));
        let s = h.snapshot();
        assert_eq!(s.total(), 2);
        assert_eq!(s.counts[0], 1, "the negative sample clamped to 0");
    }

    #[test]
    fn percentile_of_a_two_percent_tail_sees_the_tail() {
        let h = LatencyHist::new();
        for _ in 0..98 {
            h.record(500);
        }
        for _ in 0..2 {
            h.record(80_000);
        }
        let s = h.snapshot();
        assert!(s.percentile_us(0.50) <= 750);
        assert!(s.percentile_us(0.99) >= 80_000);
    }
}
