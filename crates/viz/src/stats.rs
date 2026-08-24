//! Counters for the periodic observability log line, the `/api/stats`
//! endpoint, and the `viz_stats` frame pushed to the page every second.
//!
//! Metrics are logs here (per `docs/CONVENTIONS.md`), so this is mostly a bag
//! of atomics plus a snapshot type the periodic reporter diffs against the
//! previous sample. The one non-trivial piece is [`LatencyHist`], a lock-free
//! fixed-bucket histogram that turns "capture said this batch started at
//! `ts_anchor_us`, we saw it at `now`" into a p50/p99 the plan's <10ms target
//! can actually be measured against.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Envelope tag of the stats frame pushed through the hub to the page.
pub const VIZ_STATS_TYPE: &str = "viz_stats";

/// Width of one latency histogram bucket, in µs.
pub const LAT_BUCKET_US: u64 = 250;
/// Bucket count: 1024 × 250µs covers 0–256ms. Anything slower saturates the
/// last bucket, but `max_us` stays exact, so a pathological outlier is still
/// visible.
pub const LAT_BUCKETS: usize = 1024;

/// Wall-clock UTC microseconds. The capture agent stamps `ts_anchor_us` from
/// the same clock, so the difference is an end-to-end bridge latency (modulo
/// clock skew when capture and viz run on different machines).
pub fn now_utc_us() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i64,
        Err(e) => -(e.duration().as_micros() as i64),
    }
}

/// A fixed-bucket latency histogram. Recording is three relaxed atomic adds;
/// reading walks 1024 atomics, which only the 5s reporter and `/api/stats` do.
#[derive(Debug)]
pub struct LatencyHist {
    buckets: Vec<AtomicU64>,
    sum_us: AtomicU64,
    max_us: AtomicU64,
    negative: AtomicU64,
}

impl Default for LatencyHist {
    fn default() -> Self {
        Self {
            buckets: (0..LAT_BUCKETS).map(|_| AtomicU64::new(0)).collect(),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            negative: AtomicU64::new(0),
        }
    }
}

impl LatencyHist {
    /// Record one observation. Negative values (clock skew between the capture
    /// host and this one) are counted separately and clamped to zero rather
    /// than thrown away — a nonzero `negative` is the tell that the numbers
    /// need a grain of salt.
    pub fn record(&self, us: i64) {
        if us < 0 {
            self.negative.fetch_add(1, Ordering::Relaxed);
        }
        let v = us.max(0) as u64;
        let idx = ((v / LAT_BUCKET_US) as usize).min(LAT_BUCKETS - 1);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(v, Ordering::Relaxed);
        self.max_us.fetch_max(v, Ordering::Relaxed);
    }

    /// Read the histogram without disturbing it.
    pub fn snapshot(&self) -> LatencySnapshot {
        self.read(false)
    }

    /// Read the histogram and clear it, starting a fresh window.
    pub fn take(&self) -> LatencySnapshot {
        self.read(true)
    }

    fn read(&self, reset: bool) -> LatencySnapshot {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|b| {
                if reset {
                    b.swap(0, Ordering::Relaxed)
                } else {
                    b.load(Ordering::Relaxed)
                }
            })
            .collect();
        let (sum_us, max_us, negative) = if reset {
            (
                self.sum_us.swap(0, Ordering::Relaxed),
                self.max_us.swap(0, Ordering::Relaxed),
                self.negative.swap(0, Ordering::Relaxed),
            )
        } else {
            (
                self.sum_us.load(Ordering::Relaxed),
                self.max_us.load(Ordering::Relaxed),
                self.negative.load(Ordering::Relaxed),
            )
        };
        let samples: u64 = counts.iter().sum();
        LatencySnapshot {
            samples,
            p50_us: percentile(&counts, samples, 0.50, max_us),
            p99_us: percentile(&counts, samples, 0.99, max_us),
            max_us,
            mean_us: sum_us.checked_div(samples).unwrap_or(0),
            negative,
        }
    }
}

/// Bucket-resolution percentile: the upper edge of the bucket the requested
/// quantile falls in, never overstating the observed maximum.
fn percentile(counts: &[u64], samples: u64, q: f64, max_us: u64) -> u64 {
    if samples == 0 {
        return 0;
    }
    let want = ((samples as f64) * q).ceil().max(1.0) as u64;
    let mut acc = 0u64;
    for (i, c) in counts.iter().enumerate() {
        acc += c;
        if acc >= want {
            // The last bucket is unbounded above, so the only honest number
            // for it is the observed maximum.
            if i == counts.len() - 1 {
                return max_us;
            }
            return ((i as u64 + 1) * LAT_BUCKET_US).min(max_us);
        }
    }
    max_us
}

/// Point-in-time read of a [`LatencyHist`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LatencySnapshot {
    pub samples: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub mean_us: u64,
    /// Observations that came out negative before clamping — capture/viz clock
    /// skew, not a real negative latency.
    pub negative: u64,
}

#[derive(Debug)]
pub struct Stats {
    /// UDP datagrams received (parseable or not).
    pub datagrams: AtomicU64,
    /// Datagrams rejected by [`crate::hub::classify_datagram`].
    pub parse_errors: AtomicU64,
    /// Envelopes accepted and handed to the broadcast channel.
    pub forwarded: AtomicU64,
    /// Envelopes a WebSocket client missed because it could not keep up.
    pub lag_drops: AtomicU64,
    /// WebSocket clients that were disconnected for lagging.
    pub lag_disconnects: AtomicU64,
    /// Currently connected WebSocket clients.
    pub clients: AtomicI64,
    /// Bridge latency for the current (in-progress) reporting window.
    pub latency: LatencyHist,
    /// The most recently *completed* latency window, so `/api/stats` and the
    /// pushed `viz_stats` frame report a stable number instead of one that
    /// resets to near-empty every reporting interval.
    last_latency: Mutex<LatencySnapshot>,
    /// Most recent per-interval datagram rate (f64 bits), published by the
    /// periodic reporter so `/api/stats` can answer without keeping its own
    /// snapshot history.
    rate_bits: AtomicU64,
    started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            datagrams: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            lag_drops: AtomicU64::new(0),
            lag_disconnects: AtomicU64::new(0),
            clients: AtomicI64::new(0),
            latency: LatencyHist::default(),
            last_latency: Mutex::new(LatencySnapshot::default()),
            rate_bits: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

/// Point-in-time copy of the [`Stats`] counters, used to compute per-interval
/// rates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub datagrams: u64,
    pub parse_errors: u64,
    pub forwarded: u64,
    pub lag_drops: u64,
    pub lag_disconnects: u64,
    pub clients: i64,
}

impl Stats {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            datagrams: self.datagrams.load(Ordering::Relaxed),
            parse_errors: self.parse_errors.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
            lag_drops: self.lag_drops.load(Ordering::Relaxed),
            lag_disconnects: self.lag_disconnects.load(Ordering::Relaxed),
            clients: self.clients.load(Ordering::Relaxed),
        }
    }

    pub fn client_connected(&self) {
        self.clients.fetch_add(1, Ordering::Relaxed);
    }

    pub fn client_disconnected(&self) {
        self.clients.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn record_lag(&self, missed: u64) {
        self.lag_drops.fetch_add(missed, Ordering::Relaxed);
        self.lag_disconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_latency(&self, us: i64) {
        self.latency.record(us);
    }

    /// Close the current latency window and make it the reported one.
    pub fn roll_latency(&self) -> LatencySnapshot {
        let done = self.latency.take();
        if done.samples > 0 {
            *self.last_latency.lock().unwrap() = done;
        }
        done
    }

    /// Latency to report: the last completed window, falling back to the
    /// in-progress one before the first roll (so a page opened in the first
    /// few seconds still sees numbers).
    pub fn latency_snapshot(&self) -> LatencySnapshot {
        let last = *self.last_latency.lock().unwrap();
        if last.samples > 0 {
            last
        } else {
            self.latency.snapshot()
        }
    }

    pub fn uptime_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Publish the interval rate the periodic reporter just computed.
    pub fn set_datagrams_per_s(&self, v: f64) {
        self.rate_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn datagrams_per_s(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Relaxed))
    }

    /// The JSON body shared by `/api/stats` and the pushed `viz_stats` frame.
    pub fn payload(&self, datagrams_per_s: f64, session_cached: bool) -> StatsPayload {
        let s = self.snapshot();
        StatsPayload {
            kind: VIZ_STATS_TYPE,
            uptime_s: self.uptime_s(),
            datagrams: s.datagrams,
            datagrams_per_s,
            forwarded: s.forwarded,
            parse_errors: s.parse_errors,
            lag_drops: s.lag_drops,
            lag_disconnects: s.lag_disconnects,
            clients: s.clients,
            session_cached,
            latency: self.latency_snapshot(),
        }
    }
}

/// What `/api/stats` returns and what the page receives once a second over the
/// WebSocket. Identical shape on both paths on purpose: one parser in the page.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatsPayload {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub uptime_s: f64,
    pub datagrams: u64,
    pub datagrams_per_s: f64,
    pub forwarded: u64,
    pub parse_errors: u64,
    pub lag_drops: u64,
    pub lag_disconnects: u64,
    pub clients: i64,
    pub session_cached: bool,
    pub latency: LatencySnapshot,
}

impl Snapshot {
    /// Per-interval deltas plus a datagram rate, for the periodic log line.
    pub fn delta(&self, prev: &Snapshot, secs: f64) -> Delta {
        let datagrams = self.datagrams.saturating_sub(prev.datagrams);
        Delta {
            datagrams,
            datagrams_per_s: if secs > 0.0 {
                datagrams as f64 / secs
            } else {
                0.0
            },
            parse_errors: self.parse_errors.saturating_sub(prev.parse_errors),
            forwarded: self.forwarded.saturating_sub(prev.forwarded),
            lag_drops: self.lag_drops.saturating_sub(prev.lag_drops),
            lag_disconnects: self.lag_disconnects.saturating_sub(prev.lag_disconnects),
            clients: self.clients,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delta {
    pub datagrams: u64,
    pub datagrams_per_s: f64,
    pub parse_errors: u64,
    pub forwarded: u64,
    pub lag_drops: u64,
    pub lag_disconnects: u64,
    pub clients: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_computes_rates_between_snapshots() {
        let s = Stats::default();
        let a = s.snapshot();
        s.datagrams.fetch_add(200, Ordering::Relaxed);
        s.forwarded.fetch_add(198, Ordering::Relaxed);
        s.parse_errors.fetch_add(2, Ordering::Relaxed);
        s.client_connected();
        let b = s.snapshot();

        let d = b.delta(&a, 5.0);
        assert_eq!(d.datagrams, 200);
        assert_eq!(d.forwarded, 198);
        assert_eq!(d.parse_errors, 2);
        assert_eq!(d.clients, 1);
        assert!((d.datagrams_per_s - 40.0).abs() < 1e-9);
    }

    #[test]
    fn client_gauge_tracks_connect_disconnect() {
        let s = Stats::default();
        s.client_connected();
        s.client_connected();
        s.client_disconnected();
        assert_eq!(s.snapshot().clients, 1);
    }

    #[test]
    fn lag_records_missed_and_disconnect() {
        let s = Stats::default();
        s.record_lag(17);
        let snap = s.snapshot();
        assert_eq!(snap.lag_drops, 17);
        assert_eq!(snap.lag_disconnects, 1);
    }

    #[test]
    fn empty_latency_histogram_is_all_zeroes() {
        let h = LatencyHist::default();
        assert_eq!(h.snapshot(), LatencySnapshot::default());
    }

    #[test]
    fn latency_percentiles_land_in_the_right_bucket() {
        let h = LatencyHist::default();
        // 99 samples at ~1ms, one at ~50ms: p50 must stay near 1ms, p99 must
        // pick up the outlier.
        for _ in 0..99 {
            h.record(1_000);
        }
        h.record(50_000);
        let s = h.snapshot();
        assert_eq!(s.samples, 100);
        // 1000µs lands in bucket 4 (1000..1250), whose upper edge is 1250.
        assert_eq!(s.p50_us, 1_250);
        assert_eq!(s.p99_us, 1_250, "p99 of 100 samples is the 99th, still 1ms");
        assert_eq!(s.max_us, 50_000);
        assert_eq!(s.mean_us, (99 * 1_000 + 50_000) / 100);
    }

    #[test]
    fn latency_p99_catches_a_two_percent_tail() {
        let h = LatencyHist::default();
        for _ in 0..98 {
            h.record(500);
        }
        for _ in 0..2 {
            h.record(80_000);
        }
        let s = h.snapshot();
        assert!(s.p50_us <= 750, "p50 {} should stay fast", s.p50_us);
        assert!(s.p99_us >= 80_000, "p99 {} should see the tail", s.p99_us);
    }

    #[test]
    fn percentile_never_exceeds_the_observed_max() {
        let h = LatencyHist::default();
        h.record(10);
        let s = h.snapshot();
        assert_eq!(s.max_us, 10);
        assert_eq!(s.p50_us, 10, "bucket edge is clamped to the real max");
        assert_eq!(s.p99_us, 10);
    }

    #[test]
    fn negative_latency_is_counted_and_clamped() {
        let h = LatencyHist::default();
        h.record(-5_000);
        h.record(1_000);
        let s = h.snapshot();
        assert_eq!(s.samples, 2);
        assert_eq!(s.negative, 1);
        assert_eq!(s.max_us, 1_000, "the negative sample clamps to 0");
    }

    #[test]
    fn very_large_latency_saturates_the_last_bucket_but_keeps_max() {
        let h = LatencyHist::default();
        h.record(5_000_000); // 5 seconds
        let s = h.snapshot();
        assert_eq!(s.samples, 1);
        assert_eq!(s.max_us, 5_000_000);
        assert_eq!(s.p99_us, 5_000_000);
    }

    #[test]
    fn take_clears_the_window() {
        let h = LatencyHist::default();
        h.record(2_000);
        assert_eq!(h.take().samples, 1);
        assert_eq!(h.snapshot(), LatencySnapshot::default());
    }

    #[test]
    fn roll_latency_publishes_the_completed_window() {
        let s = Stats::default();
        // Before any samples the reported snapshot is simply empty.
        assert_eq!(s.latency_snapshot().samples, 0);

        s.record_latency(3_000);
        // Not yet rolled: the in-progress window is reported.
        assert_eq!(s.latency_snapshot().samples, 1);

        let done = s.roll_latency();
        assert_eq!(done.samples, 1);
        // Rolled: the completed window is reported, and it survives an empty
        // interval rather than blinking back to zero.
        assert_eq!(s.latency_snapshot().samples, 1);
        s.roll_latency();
        assert_eq!(s.latency_snapshot().samples, 1);
    }

    #[test]
    fn payload_has_the_documented_shape() {
        let s = Stats::default();
        s.datagrams.fetch_add(7, Ordering::Relaxed);
        s.parse_errors.fetch_add(1, Ordering::Relaxed);
        s.record_latency(4_000);
        s.roll_latency();

        let p = s.payload(40.0, true);
        assert_eq!(p.kind, VIZ_STATS_TYPE);
        assert_eq!(p.datagrams, 7);
        assert_eq!(p.parse_errors, 1);
        assert!(p.session_cached);
        assert_eq!(p.latency.samples, 1);

        let v: serde_json::Value = serde_json::to_value(&p).unwrap();
        assert_eq!(v["type"], VIZ_STATS_TYPE);
        for key in [
            "uptime_s",
            "datagrams",
            "datagrams_per_s",
            "forwarded",
            "parse_errors",
            "lag_drops",
            "lag_disconnects",
            "clients",
            "session_cached",
            "latency",
        ] {
            assert!(v.get(key).is_some(), "missing {key} in /api/stats payload");
        }
        for key in ["samples", "p50_us", "p99_us", "max_us", "mean_us", "negative"] {
            assert!(v["latency"].get(key).is_some(), "missing latency.{key}");
        }
    }

    #[test]
    fn now_utc_us_is_a_plausible_wall_clock() {
        // Somewhere after 2020 and before 2100, in µs.
        let t = now_utc_us();
        assert!(t > 1_577_836_800_000_000, "{t}");
        assert!(t < 4_102_444_800_000_000, "{t}");
    }
}
