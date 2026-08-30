//! Process-wide counters. Metrics are logs here (see CONVENTIONS.md): the
//! reporter renders one structured `info` event every 5s from atomic snapshots.
//!
//! Two things matter for the hot path:
//!
//! * The counters T1 owns live in their own [`T1Counters`] block, aligned to a
//!   cache line so T2/T3 writes never invalidate the line T1 stores into.
//! * T1 keeps its running totals in thread-local registers and *stores* them
//!   (relaxed) rather than doing a read-modify-write per event.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Number of log2 buckets in a [`Histogram`]. Bucket `i > 0` covers
/// `[2^(i-1), 2^i)` µs, bucket 0 is exactly 0µs, so the top bucket saturates
/// at ~2^31 µs (~35 minutes) — far beyond anything we'd still call latency.
pub const HIST_BUCKETS: usize = 32;

/// Fixed, allocation-free log2 histogram of microsecond latencies.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; HIST_BUCKETS],
}

/// Which bucket a µs value lands in. Pure, so the bucketing is testable.
pub fn bucket_of(us: u64) -> usize {
    if us == 0 {
        return 0;
    }
    ((u64::BITS - us.leading_zeros()) as usize).min(HIST_BUCKETS - 1)
}

/// Inclusive upper bound of a bucket, used as its representative value when
/// approximating percentiles (conservative: never understates latency).
///
/// The top bucket is the overflow bucket — it has no real upper bound, and
/// reports its nominal one (~2^31 µs ≈ 36 minutes). Anything landing there is
/// not a latency measurement, it is a broken machine.
pub fn bucket_upper_us(bucket: usize) -> u64 {
    match bucket {
        0 => 0,
        b => (1u64 << b.min(HIST_BUCKETS - 1)) - 1,
    }
}

impl Histogram {
    pub fn record(&self, us: u64) {
        self.buckets[bucket_of(us)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistSnapshot {
        let mut counts = [0u64; HIST_BUCKETS];
        for (slot, atomic) in counts.iter_mut().zip(self.buckets.iter()) {
            *slot = atomic.load(Ordering::Relaxed);
        }
        HistSnapshot { counts }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistSnapshot {
    pub counts: [u64; HIST_BUCKETS],
}

impl Default for HistSnapshot {
    fn default() -> Self {
        Self {
            counts: [0; HIST_BUCKETS],
        }
    }
}

impl HistSnapshot {
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// Counts recorded since `prev` (the histogram is monotonic, so a windowed
    /// report is just a difference).
    pub fn since(&self, prev: &Self) -> Self {
        let mut counts = [0u64; HIST_BUCKETS];
        for (i, slot) in counts.iter_mut().enumerate() {
            *slot = self.counts[i].saturating_sub(prev.counts[i]);
        }
        Self { counts }
    }

    /// Approximate percentile in µs: the upper bound of the bucket the `q`th
    /// sample falls in. `q` is clamped to `[0, 1]`; an empty histogram is 0.
    pub fn percentile_us(&self, q: f64) -> u64 {
        let total = self.total();
        if total == 0 {
            return 0;
        }
        let q = q.clamp(0.0, 1.0);
        let rank = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut cumulative = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            cumulative += c;
            if cumulative >= rank {
                return bucket_upper_us(i);
            }
        }
        bucket_upper_us(HIST_BUCKETS - 1)
    }
}

/// The counters T1 is the sole writer of, on their own cache line.
///
/// T1 stores monotonic running totals; every other thread only loads them.
#[derive(Debug, Default)]
#[repr(align(64))]
pub struct T1Counters {
    /// Raw events pushed into the ring by T1.
    pub events: AtomicU64,
    /// Events dropped because the SPSC ring was full. Monotonic, never reset.
    pub ring_drops: AtomicU32,
    /// `WM_INPUT` frames skipped because they carried absolute coordinates.
    pub abs_frames: AtomicU32,
    /// QPC of the most recent event T1 saw. 0 until the first event.
    pub last_event_qpc: AtomicU64,
    /// Padding so the counters T2/T3 write never share this cache line.
    _pad: [u8; 32],
}

/// Every counter the agent keeps. Shared via `Arc` across all threads.
#[derive(Debug, Default)]
pub struct Stats {
    /// Written only by T1 (see [`T1Counters`]).
    pub t1: T1Counters,
    /// Batches assembled and fanned out by T2.
    pub batches: AtomicU64,
    /// Hotkey markers emitted.
    pub markers: AtomicU64,
    pub udp_errors: AtomicU64,
    pub jsonl_errors: AtomicU64,
    pub kafka_errors: AtomicU64,
    /// Datagrams the OS reported as unreachable (ICMP port-unreachable comes
    /// back as `WSAECONNRESET` on a connected socket). Expected while no viz
    /// is running, so it is a counter and not a warning.
    pub udp_unreachable: AtomicU64,
    /// Envelopes too large for one datagram.
    pub udp_oversized: AtomicU64,
    /// Envelopes handed to the Kafka forwarder but not yet produced.
    pub kafka_queued: AtomicU64,
    /// Envelopes dropped because the Kafka forwarding channel was full.
    pub kafka_dropped: AtomicU64,
    /// Envelopes still queued when the bounded shutdown drain gave up.
    pub kafka_abandoned: AtomicU64,
    /// High-water mark of ring occupancy observed by T2.
    pub ring_high_water: AtomicU64,
    /// Slowest periodic JSONL flush, in µs.
    pub jsonl_flush_max_us: AtomicU64,
    /// Capture→ship latency measured from a batch's *first* event.
    pub ship_latency_first: Histogram,
    /// Capture→ship latency measured from a batch's *last* event.
    pub ship_latency_last: Histogram,
}

impl Stats {
    pub fn count_sink_error(&self, sink: &str) {
        let c = match sink {
            "udp" => &self.udp_errors,
            "jsonl" => &self.jsonl_errors,
            "kafka" => &self.kafka_errors,
            _ => return,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn events(&self) -> u64 {
        self.t1.events.load(Ordering::Relaxed)
    }

    pub fn ring_drops(&self) -> u32 {
        self.t1.ring_drops.load(Ordering::Relaxed)
    }

    pub fn abs_frames(&self) -> u32 {
        self.t1.abs_frames.load(Ordering::Relaxed)
    }

    pub fn last_event_qpc(&self) -> u64 {
        self.t1.last_event_qpc.load(Ordering::Relaxed)
    }

    /// Raise the ring high-water mark if `slots` beats it.
    pub fn observe_ring_slots(&self, slots: u64) {
        let mut best = self.ring_high_water.load(Ordering::Relaxed);
        while slots > best {
            match self.ring_high_water.compare_exchange_weak(
                best,
                slots,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => best = actual,
            }
        }
    }

    /// Raise the slowest-JSONL-flush mark if `us` beats it.
    pub fn observe_jsonl_flush(&self, us: u64) {
        let mut best = self.jsonl_flush_max_us.load(Ordering::Relaxed);
        while us > best {
            match self.jsonl_flush_max_us.compare_exchange_weak(
                best,
                us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => best = actual,
            }
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            events: self.events(),
            abs_frames: self.abs_frames() as u64,
            batches: self.batches.load(Ordering::Relaxed),
            markers: self.markers.load(Ordering::Relaxed),
            ring_drops: self.ring_drops() as u64,
            udp_errors: self.udp_errors.load(Ordering::Relaxed),
            jsonl_errors: self.jsonl_errors.load(Ordering::Relaxed),
            kafka_errors: self.kafka_errors.load(Ordering::Relaxed),
            udp_unreachable: self.udp_unreachable.load(Ordering::Relaxed),
            udp_oversized: self.udp_oversized.load(Ordering::Relaxed),
            kafka_queued: self.kafka_queued.load(Ordering::Relaxed),
            kafka_dropped: self.kafka_dropped.load(Ordering::Relaxed),
            kafka_abandoned: self.kafka_abandoned.load(Ordering::Relaxed),
            ring_high_water: self.ring_high_water.load(Ordering::Relaxed),
            jsonl_flush_max_us: self.jsonl_flush_max_us.load(Ordering::Relaxed),
            ship_latency_first: self.ship_latency_first.snapshot(),
            ship_latency_last: self.ship_latency_last.snapshot(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub events: u64,
    pub abs_frames: u64,
    pub batches: u64,
    pub markers: u64,
    pub ring_drops: u64,
    pub udp_errors: u64,
    pub jsonl_errors: u64,
    pub kafka_errors: u64,
    pub udp_unreachable: u64,
    pub udp_oversized: u64,
    pub kafka_queued: u64,
    pub kafka_dropped: u64,
    pub kafka_abandoned: u64,
    pub ring_high_water: u64,
    pub jsonl_flush_max_us: u64,
    pub ship_latency_first: HistSnapshot,
    pub ship_latency_last: HistSnapshot,
}

/// The fields of one periodic report line. Computed purely so the arithmetic
/// (rates, deltas, percentiles) is testable without a subscriber.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReportFields {
    pub events_per_s: f64,
    pub events: u64,
    pub batches_per_s: f64,
    pub batches: u64,
    pub drops: u64,
    pub drops_delta: u64,
    pub abs_frames: u64,
    pub markers: u64,
    pub udp_errors: u64,
    pub jsonl_errors: u64,
    pub kafka_errors: u64,
    pub udp_unreachable: u64,
    pub udp_oversized: u64,
    pub kafka_queued: u64,
    pub kafka_dropped: u64,
    pub kafka_abandoned: u64,
    pub ring_high_water: u64,
    pub jsonl_flush_max_us: u64,
    /// Approximate p50/p99 of capture→ship latency over the report window,
    /// measured from a batch's first event (worst case for a full batch).
    pub capture_to_ship_us_p50: u64,
    pub capture_to_ship_us_p99: u64,
    /// Same, from the batch's last event: pure shipping overhead.
    pub ship_tail_us_p99: u64,
    /// Seconds since T1 last saw an event. Distinguishes "idle" from "broken".
    pub idle_for_s: u64,
}

/// Compute one report window from two counter snapshots.
pub fn compute_report(
    prev: &StatsSnapshot,
    now: &StatsSnapshot,
    elapsed_secs: f64,
    idle_for_s: u64,
) -> ReportFields {
    let secs = if elapsed_secs > 0.0 {
        elapsed_secs
    } else {
        1.0
    };
    let events_delta = now.events.saturating_sub(prev.events);
    let batches_delta = now.batches.saturating_sub(prev.batches);
    let first = now.ship_latency_first.since(&prev.ship_latency_first);
    let last = now.ship_latency_last.since(&prev.ship_latency_last);
    ReportFields {
        events_per_s: events_delta as f64 / secs,
        events: now.events,
        batches_per_s: batches_delta as f64 / secs,
        batches: now.batches,
        drops: now.ring_drops,
        drops_delta: now.ring_drops.saturating_sub(prev.ring_drops),
        abs_frames: now.abs_frames,
        markers: now.markers,
        udp_errors: now.udp_errors,
        jsonl_errors: now.jsonl_errors,
        kafka_errors: now.kafka_errors,
        udp_unreachable: now.udp_unreachable,
        udp_oversized: now.udp_oversized,
        kafka_queued: now.kafka_queued,
        kafka_dropped: now.kafka_dropped,
        kafka_abandoned: now.kafka_abandoned,
        ring_high_water: now.ring_high_water,
        jsonl_flush_max_us: now.jsonl_flush_max_us,
        capture_to_ship_us_p50: first.percentile_us(0.50),
        capture_to_ship_us_p99: first.percentile_us(0.99),
        ship_tail_us_p99: last.percentile_us(0.99),
        idle_for_s,
    }
}

/// Seconds since the last captured event, given the current QPC reading.
/// Returns 0 while no event has ever arrived (nothing to be idle from yet).
pub fn idle_for_secs(last_event_qpc: u64, now_qpc: u64, qpc_freq: u64) -> u64 {
    if last_event_qpc == 0 || qpc_freq == 0 {
        return 0;
    }
    now_qpc.saturating_sub(last_event_qpc) / qpc_freq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_errors_are_routed_by_name() {
        let s = Stats::default();
        s.count_sink_error("udp");
        s.count_sink_error("kafka");
        s.count_sink_error("kafka");
        s.count_sink_error("nope");
        let snap = s.snapshot();
        assert_eq!(snap.udp_errors, 1);
        assert_eq!(snap.kafka_errors, 2);
        assert_eq!(snap.jsonl_errors, 0);
    }

    #[test]
    fn report_shows_rates_and_deltas() {
        let prev = StatsSnapshot {
            events: 1_000,
            batches: 40,
            ring_drops: 2,
            ..Default::default()
        };
        let now = StatsSnapshot {
            events: 6_000,
            batches: 240,
            ring_drops: 5,
            ..Default::default()
        };
        let r = compute_report(&prev, &now, 5.0, 3);
        assert_eq!(r.events_per_s, 1000.0);
        assert_eq!(r.batches_per_s, 40.0);
        assert_eq!((r.drops, r.drops_delta), (5, 3));
        assert_eq!(r.idle_for_s, 3);
    }

    #[test]
    fn report_tolerates_zero_elapsed() {
        let z = StatsSnapshot::default();
        let r = compute_report(&z, &z, 0.0, 0);
        assert_eq!(r.events_per_s, 0.0);
        assert_eq!(r.capture_to_ship_us_p99, 0);
    }

    #[test]
    fn buckets_are_log2_and_saturate() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(1), 1);
        assert_eq!(bucket_of(2), 2);
        assert_eq!(bucket_of(3), 2);
        assert_eq!(bucket_of(4), 3);
        assert_eq!(bucket_of(7), 3);
        assert_eq!(bucket_of(8), 4);
        assert_eq!(bucket_of(1_000), 10); // [512, 1024)
        assert_eq!(bucket_of(u64::MAX), HIST_BUCKETS - 1);
        // Every measurable value lands in a bucket whose range contains it.
        for v in [0u64, 1, 5, 63, 64, 65, 100_000, 1_000_000_000] {
            let b = bucket_of(v);
            assert!(v <= bucket_upper_us(b), "{v} > upper({b})");
        }
        // Past ~36 minutes everything piles into the saturating top bucket,
        // which reports its nominal bound rather than a meaningless u64::MAX.
        assert_eq!(bucket_of(1 << 40), HIST_BUCKETS - 1);
        assert_eq!(bucket_upper_us(HIST_BUCKETS - 1), (1u64 << 31) - 1);
    }

    #[test]
    fn histogram_percentiles_approximate_from_buckets() {
        let h = Histogram::default();
        // 99 samples at ~100µs, 1 at ~1s.
        for _ in 0..99 {
            h.record(100);
        }
        h.record(1_000_000);
        let s = h.snapshot();
        assert_eq!(s.total(), 100);
        // 100µs sits in [64,128) -> reported as 127.
        assert_eq!(s.percentile_us(0.50), 127);
        assert_eq!(s.percentile_us(0.90), 127);
        // The one big sample is the 100th, so p99 (rank 99) is still the small
        // bucket and p100 is the big one: [524288, 1048576) -> 1048575.
        assert_eq!(s.percentile_us(0.99), 127);
        assert_eq!(s.percentile_us(1.0), 1_048_575);
    }

    #[test]
    fn empty_histogram_reports_zero() {
        let s = Histogram::default().snapshot();
        assert_eq!(s.total(), 0);
        assert_eq!(s.percentile_us(0.5), 0);
        assert_eq!(s.percentile_us(0.99), 0);
    }

    #[test]
    fn histogram_windows_are_differences() {
        let h = Histogram::default();
        h.record(10);
        let first = h.snapshot();
        h.record(10_000);
        let second = h.snapshot();
        let window = second.since(&first);
        assert_eq!(window.total(), 1);
        // Only the 10ms sample is in the window.
        assert_eq!(
            window.percentile_us(0.5),
            bucket_upper_us(bucket_of(10_000))
        );
    }

    #[test]
    fn high_water_marks_only_ever_rise() {
        let s = Stats::default();
        s.observe_ring_slots(10);
        s.observe_ring_slots(3);
        s.observe_ring_slots(42);
        s.observe_ring_slots(41);
        assert_eq!(s.snapshot().ring_high_water, 42);

        s.observe_jsonl_flush(500);
        s.observe_jsonl_flush(100);
        assert_eq!(s.snapshot().jsonl_flush_max_us, 500);
    }

    #[test]
    fn idle_seconds_needs_a_first_event() {
        assert_eq!(idle_for_secs(0, 10_000_000_000, 10_000_000), 0);
        assert_eq!(idle_for_secs(1_000, 1_000, 10_000_000), 0);
        assert_eq!(idle_for_secs(1_000, 1_000 + 35_000_000, 10_000_000), 3);
    }

    #[test]
    fn t1_counters_own_their_cache_line() {
        assert_eq!(align_of::<T1Counters>(), 64);
        assert_eq!(size_of::<T1Counters>(), 64);
        let s = Stats::default();
        let base = &s as *const Stats as usize;
        let after = &s.batches as *const AtomicU64 as usize;
        assert!(
            after - base >= 64,
            "T2/T3 counters share T1's cache line (offset {})",
            after - base
        );
    }
}
