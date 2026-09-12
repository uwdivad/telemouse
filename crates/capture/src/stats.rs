//! Process-wide counters. Metrics are logs here (see CONVENTIONS.md): the
//! reporter renders one structured `info` event every 5s from atomic snapshots.
//!
//! Two things matter for the hot path:
//!
//! * The counters T1 owns live in their own [`T1Counters`] block, aligned to a
//!   cache line so T2/T3 writes never invalidate the line T1 stores into.
//! * T1 keeps its running totals in thread-local registers and *stores* them
//!   (relaxed) rather than doing a read-modify-write per event.
//!
//! Latency lives in [`telemouse_core::LatencyHist`] — the workspace-wide
//! histogram, so a p99 measured here is directly comparable with the viz
//! bridge's. Recording into it is `observability` work; the counters
//! themselves are always kept, because the final "session finished" line
//! reports them whatever the build.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use telemouse_core::{LatencyHist, LatencySnapshot, Percentile};

/// Padding that fills out T1's cache line. The exact number is checked by
/// [`tests::t1_counters_own_their_cache_line`]; `repr(align(64))` would round
/// the size up anyway, this only says so out loud.
#[cfg(feature = "observability")]
const T1_PAD_WORDS: usize = 3;
#[cfg(not(feature = "observability"))]
const T1_PAD_WORDS: usize = 4;

/// The counters T1 is the sole writer of, on their own cache line.
///
/// T1 stores monotonic running totals; every other thread only loads them.
#[derive(Debug, Default)]
#[repr(align(64))]
pub struct T1Counters {
    /// Raw events pushed into the ring by T1.
    pub events: AtomicU64,
    /// QPC of the most recent event T1 saw. 0 until the first event.
    pub last_event_qpc: AtomicU64,
    /// Events dropped because the SPSC ring was full. Monotonic, never reset.
    pub ring_drops: AtomicU32,
    /// `WM_INPUT` frames skipped because they carried absolute coordinates.
    pub abs_frames: AtomicU32,
    /// `GetRawInputBuffer` calls that returned the error sentinel. Distinct
    /// from an empty drain, which is just a still mouse.
    pub raw_read_errors: AtomicU32,
    /// T1's current estimate of the device's report interval, in µs — the
    /// [`crate::raw_input::DrainStamper`] step. 0 before the first drain.
    #[cfg(feature = "observability")]
    pub report_interval_us: AtomicU32,
    /// Buffered raw-input drains performed.
    #[cfg(feature = "observability")]
    pub drains: AtomicU64,
    /// Padding so the counters T2/T3 write never share this cache line.
    _pad: [u64; T1_PAD_WORDS],
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
    /// Datagrams the local socket buffer had no room for. Best-effort sink,
    /// so it is not an error — but it *is* a live viz missing frames, which
    /// `udp_unreachable` (nobody listening at all) does not say.
    pub udp_would_block: AtomicU64,
    /// Envelopes accepted by JSONL but not successfully flush-confirmed.
    pub jsonl_queued: AtomicU64,
    /// JSONL envelopes dropped because its queue was full or writer had failed.
    pub jsonl_dropped: AtomicU64,
    /// Envelopes not written before a writer failure or shutdown timeout.
    pub jsonl_abandoned: AtomicU64,
    /// Envelopes handed to the Kafka forwarder but not yet produced.
    pub kafka_queued: AtomicU64,
    /// Kafka envelopes dropped because its queue was full or worker had failed.
    pub kafka_dropped: AtomicU64,
    /// Kafka envelopes accepted but not produced due to delivery failure,
    /// initialization failure, panic, or a bounded shutdown timeout.
    pub kafka_abandoned: AtomicU64,
    /// High-water mark of ring occupancy observed by T2.
    pub ring_high_water: AtomicU64,
    /// Slowest periodic JSONL flush, in µs.
    pub jsonl_flush_max_us: AtomicU64,
    /// T2 loop iterations, and T3 ticks. The main thread watches both for a
    /// worker that stopped making progress (see [`StallWatch`]).
    pub t2_iters: AtomicU64,
    pub t3_ticks: AtomicU64,
    /// Capture→ship latency measured from a batch's *first* event.
    pub ship_latency_first: LatencyHist,
    /// Capture→ship latency measured from a batch's *last* event.
    pub ship_latency_last: LatencyHist,
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

    #[cfg_attr(not(feature = "observability"), allow(dead_code))]
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
            raw_read_errors: self.t1.raw_read_errors.load(Ordering::Relaxed) as u64,
            #[cfg(feature = "observability")]
            report_interval_us: self.t1.report_interval_us.load(Ordering::Relaxed),
            #[cfg(feature = "observability")]
            drains: self.t1.drains.load(Ordering::Relaxed),
            udp_errors: self.udp_errors.load(Ordering::Relaxed),
            jsonl_errors: self.jsonl_errors.load(Ordering::Relaxed),
            kafka_errors: self.kafka_errors.load(Ordering::Relaxed),
            udp_unreachable: self.udp_unreachable.load(Ordering::Relaxed),
            udp_oversized: self.udp_oversized.load(Ordering::Relaxed),
            udp_would_block: self.udp_would_block.load(Ordering::Relaxed),
            jsonl_queued: self.jsonl_queued.load(Ordering::Relaxed),
            jsonl_dropped: self.jsonl_dropped.load(Ordering::Relaxed),
            jsonl_abandoned: self.jsonl_abandoned.load(Ordering::Relaxed),
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub events: u64,
    pub abs_frames: u64,
    pub batches: u64,
    pub markers: u64,
    pub ring_drops: u64,
    pub raw_read_errors: u64,
    /// T1's report-interval estimate in µs, and the number of buffered drains
    /// it has done. Together they are the mouse's observed polling rate.
    #[cfg(feature = "observability")]
    pub report_interval_us: u32,
    #[cfg(feature = "observability")]
    pub drains: u64,
    pub udp_errors: u64,
    pub jsonl_errors: u64,
    pub kafka_errors: u64,
    pub udp_unreachable: u64,
    pub udp_oversized: u64,
    pub udp_would_block: u64,
    pub jsonl_queued: u64,
    pub jsonl_dropped: u64,
    pub jsonl_abandoned: u64,
    pub kafka_queued: u64,
    pub kafka_dropped: u64,
    pub kafka_abandoned: u64,
    pub ring_high_water: u64,
    pub jsonl_flush_max_us: u64,
    pub ship_latency_first: LatencySnapshot,
    pub ship_latency_last: LatencySnapshot,
}

/// A percentile the way a report line should show it: the bucket bound,
/// `>=255750` when the histogram saturated, and `-` when nothing was
/// recorded — because a p99 printed as `0` reads as "instant" rather than
/// "no data", which is the opposite of the truth.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportPercentile(pub Option<Percentile>);

impl ReportPercentile {
    /// The `q` quantile of `snap`.
    pub fn of(snap: &LatencySnapshot, q: f64) -> Self {
        Self(snap.percentile(q))
    }
}

impl std::fmt::Display for ReportPercentile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(p) => write!(f, "{p}"),
            None => f.write_str("-"),
        }
    }
}

/// Observed report rate of the mouse from T1's interval estimate, or `None`
/// before the first drain. 1kHz reads as ~1000.
#[cfg_attr(not(feature = "observability"), allow(dead_code))]
pub fn poll_hz(report_interval_us: u32) -> Option<f64> {
    (report_interval_us > 0).then(|| 1_000_000.0 / report_interval_us as f64)
}

/// A rate limit with no condition attached: "has `interval` passed since the
/// last time this said yes?". T1 uses one per failure kind so a timer that
/// breaks at 1kHz does not write a log line per report.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    next_at: Option<Instant>,
    suppressed: u64,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_at: None,
            suppressed: 0,
        }
    }

    /// `Some(suppressed)` when the caller should log now, reporting how many
    /// were swallowed since the last one; `None` while inside the window.
    pub fn allow(&mut self, now: Instant) -> Option<u64> {
        match self.next_at {
            Some(at) if now < at => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.next_at = Some(now + self.interval);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

/// Watches the two worker loops that have no handle to post a quit to.
///
/// T2 and T3 bump a counter every iteration; a counter that has not moved on
/// two consecutive checks is a thread that is alive (so the `AliveGuard`
/// watchdog says nothing) but no longer doing its job. One check is not
/// enough: the very first one has no baseline, and a single 60s window can
/// legitimately straddle a long park.
#[derive(Debug, Default)]
pub struct StallWatch {
    last: Option<(u64, u64)>,
    t2_strikes: u32,
    t3_strikes: u32,
}

/// Which loop stopped, if either. T2 is reported first: a stalled shipper is
/// the one that costs events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stalled {
    None,
    Shipping,
    Context,
}

/// Consecutive unchanged checks before a loop is called stalled.
pub const STALL_STRIKES: u32 = 2;

impl StallWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one check of the two loop counters.
    pub fn observe(&mut self, t2_iters: u64, t3_ticks: u64) -> Stalled {
        let Some((t2_prev, t3_prev)) = self.last.replace((t2_iters, t3_ticks)) else {
            return Stalled::None;
        };
        self.t2_strikes = if t2_iters == t2_prev {
            self.t2_strikes + 1
        } else {
            0
        };
        self.t3_strikes = if t3_ticks == t3_prev {
            self.t3_strikes + 1
        } else {
            0
        };
        if self.t2_strikes >= STALL_STRIKES {
            Stalled::Shipping
        } else if self.t3_strikes >= STALL_STRIKES {
            Stalled::Context
        } else {
            Stalled::None
        }
    }
}

/// How often a persisting problem is allowed to say so again.
#[cfg(feature = "observability")]
pub const ESCALATION_INTERVAL: Duration = Duration::from_secs(60);

/// What the reporter should say about one named condition this round.
#[cfg(feature = "observability")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// Nothing to report: the condition is not holding and was not holding.
    Quiet,
    /// Log the warning.
    Warn,
    /// The condition holds but was warned about less than
    /// [`ESCALATION_INTERVAL`] ago.
    Silent,
    /// It just cleared — log the recovery, once.
    Recovered,
}

/// One `warn!` per condition per [`ESCALATION_INTERVAL`] for as long as the
/// condition holds, and a fresh one the moment it recurs after clearing.
///
/// Unlike [`crate::shipping::WarnLimiter`], which only ever throttles, this
/// tracks the condition's *state*: a Kafka outage that ends and starts again
/// two minutes later is two incidents, and the second one is worth a line
/// even though the first was warned about recently.
#[cfg(feature = "observability")]
#[derive(Debug)]
pub struct ConditionLimiter {
    interval: Duration,
    entries: Vec<ConditionEntry>,
}

#[cfg(feature = "observability")]
#[derive(Debug)]
struct ConditionEntry {
    name: &'static str,
    /// `Some` while the condition holds: when it may next be warned about.
    warn_at: Option<Instant>,
}

#[cfg(feature = "observability")]
impl ConditionLimiter {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            entries: Vec::new(),
        }
    }

    /// Report `name`'s state this round. `active` is whether the condition
    /// holds right now.
    pub fn observe(&mut self, name: &'static str, active: bool, now: Instant) -> Escalation {
        let idx = match self.entries.iter().position(|e| e.name == name) {
            Some(i) => i,
            None => {
                if !active {
                    return Escalation::Quiet;
                }
                self.entries.push(ConditionEntry {
                    name,
                    warn_at: Some(now + self.interval),
                });
                return Escalation::Warn;
            }
        };
        let entry = &mut self.entries[idx];
        match (active, entry.warn_at) {
            (false, None) => Escalation::Quiet,
            (false, Some(_)) => {
                entry.warn_at = None;
                Escalation::Recovered
            }
            (true, Some(at)) if now < at => Escalation::Silent,
            (true, _) => {
                entry.warn_at = Some(now + self.interval);
                Escalation::Warn
            }
        }
    }
}

/// The fields of one periodic report line. Computed purely so the arithmetic
/// (rates, deltas, percentiles) is testable without a subscriber.
#[cfg(feature = "observability")]
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
    pub raw_read_errors: u64,
    /// Observed mouse report rate, `None` before T1's first drain.
    pub poll_hz: Option<f64>,
    /// Buffered raw-input drains per second, and how many reports each one
    /// returned on average — the two halves of "is the read batching paying
    /// for itself?".
    pub drains_per_s: f64,
    pub reports_per_drain: f64,
    pub udp_errors: u64,
    pub jsonl_errors: u64,
    pub kafka_errors: u64,
    pub udp_unreachable: u64,
    pub udp_oversized: u64,
    pub udp_would_block: u64,
    pub jsonl_queued: u64,
    pub jsonl_dropped: u64,
    pub jsonl_abandoned: u64,
    pub jsonl_errors_delta: u64,
    pub jsonl_dropped_delta: u64,
    pub jsonl_abandoned_delta: u64,
    pub kafka_queued: u64,
    pub kafka_dropped: u64,
    pub kafka_abandoned: u64,
    pub kafka_errors_delta: u64,
    pub kafka_dropped_delta: u64,
    pub kafka_abandoned_delta: u64,
    pub ring_high_water: u64,
    pub jsonl_flush_max_us: u64,
    /// p50/p99 of capture→ship latency over the report window, measured from
    /// a batch's first event (worst case for a full batch).
    pub capture_to_ship_us_p50: ReportPercentile,
    pub capture_to_ship_us_p99: ReportPercentile,
    /// Same, from the batch's last event: pure shipping overhead.
    pub ship_tail_us_p99: ReportPercentile,
    /// Seconds since T1 last saw an event. Distinguishes "idle" from "broken".
    pub idle_for_s: u64,
}

#[cfg(feature = "observability")]
impl ReportFields {
    /// True when a sink is failing, dropping or abandoning envelopes in this
    /// window — the escalation condition for `jsonl` / `kafka`.
    pub fn sink_is_losing(&self, sink: &str) -> bool {
        match sink {
            "jsonl" => {
                self.jsonl_errors_delta > 0
                    || self.jsonl_dropped_delta > 0
                    || self.jsonl_abandoned_delta > 0
            }
            "kafka" => {
                self.kafka_errors_delta > 0
                    || self.kafka_dropped_delta > 0
                    || self.kafka_abandoned_delta > 0
            }
            _ => false,
        }
    }
}

/// Compute one report window from two counter snapshots.
#[cfg(feature = "observability")]
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
    let drains_delta = now.drains.saturating_sub(prev.drains);
    let first = now.ship_latency_first.delta(&prev.ship_latency_first);
    let last = now.ship_latency_last.delta(&prev.ship_latency_last);
    ReportFields {
        events_per_s: events_delta as f64 / secs,
        events: now.events,
        batches_per_s: batches_delta as f64 / secs,
        batches: now.batches,
        drops: now.ring_drops,
        drops_delta: now.ring_drops.saturating_sub(prev.ring_drops),
        abs_frames: now.abs_frames,
        markers: now.markers,
        raw_read_errors: now.raw_read_errors,
        poll_hz: poll_hz(now.report_interval_us),
        drains_per_s: drains_delta as f64 / secs,
        reports_per_drain: if drains_delta > 0 {
            events_delta as f64 / drains_delta as f64
        } else {
            0.0
        },
        udp_errors: now.udp_errors,
        jsonl_errors: now.jsonl_errors,
        kafka_errors: now.kafka_errors,
        udp_unreachable: now.udp_unreachable,
        udp_oversized: now.udp_oversized,
        udp_would_block: now.udp_would_block,
        jsonl_queued: now.jsonl_queued,
        jsonl_dropped: now.jsonl_dropped,
        jsonl_abandoned: now.jsonl_abandoned,
        jsonl_errors_delta: now.jsonl_errors.saturating_sub(prev.jsonl_errors),
        jsonl_dropped_delta: now.jsonl_dropped.saturating_sub(prev.jsonl_dropped),
        jsonl_abandoned_delta: now.jsonl_abandoned.saturating_sub(prev.jsonl_abandoned),
        kafka_queued: now.kafka_queued,
        kafka_dropped: now.kafka_dropped,
        kafka_abandoned: now.kafka_abandoned,
        kafka_errors_delta: now.kafka_errors.saturating_sub(prev.kafka_errors),
        kafka_dropped_delta: now.kafka_dropped.saturating_sub(prev.kafka_dropped),
        kafka_abandoned_delta: now.kafka_abandoned.saturating_sub(prev.kafka_abandoned),
        ring_high_water: now.ring_high_water,
        jsonl_flush_max_us: now.jsonl_flush_max_us,
        capture_to_ship_us_p50: ReportPercentile::of(&first, 0.50),
        capture_to_ship_us_p99: ReportPercentile::of(&first, 0.99),
        ship_tail_us_p99: ReportPercentile::of(&last, 0.99),
        idle_for_s,
    }
}

/// Seconds since the last captured event, given the current QPC reading.
/// Returns 0 while no event has ever arrived (nothing to be idle from yet).
#[cfg(feature = "observability")]
pub fn idle_for_secs(last_event_qpc: u64, now_qpc: u64, qpc_freq: u64) -> u64 {
    if last_event_qpc == 0 || qpc_freq == 0 {
        return 0;
    }
    now_qpc.saturating_sub(last_event_qpc) / qpc_freq
}

/// Idle past this many seconds is worth one warning: a desk nobody is at is
/// indistinguishable from a capture thread that stopped delivering.
#[cfg(feature = "observability")]
pub const IDLE_WARN_SECS: u64 = 60;

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

    #[test]
    fn the_shared_histogram_is_what_the_snapshot_carries() {
        let s = Stats::default();
        s.ship_latency_first.record(1_000);
        s.ship_latency_first.record(9_000_000); // a stall, not a latency
        s.ship_latency_last.record(300);
        let snap = s.snapshot();
        assert_eq!(snap.ship_latency_first.total(), 2);
        // 1000µs is the first value of the 1000..1250 bucket.
        assert_eq!(snap.ship_latency_first.percentile_us(0.5), 1_250);
        // The outlier saturates, and the report says so rather than lying.
        let p100 = ReportPercentile::of(&snap.ship_latency_first, 1.0);
        assert_eq!(p100.to_string(), ">=255750");
        assert_eq!(
            ReportPercentile::of(&snap.ship_latency_last, 0.99).to_string(),
            "500"
        );
    }

    #[test]
    fn a_percentile_with_no_samples_reads_as_no_data() {
        let empty = LatencySnapshot::default();
        assert_eq!(ReportPercentile::of(&empty, 0.99).to_string(), "-");
        assert_eq!(ReportPercentile::default().to_string(), "-");
    }

    #[test]
    fn poll_rate_is_the_inverse_of_the_report_interval() {
        assert_eq!(poll_hz(1_000), Some(1_000.0));
        assert_eq!(poll_hz(125), Some(8_000.0));
        assert_eq!(poll_hz(8_000), Some(125.0));
        assert_eq!(
            poll_hz(0),
            None,
            "nothing observed yet is not 'infinite Hz'"
        );
    }

    #[test]
    fn a_throttle_reports_what_it_swallowed() {
        let mut t = Throttle::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(t.allow(t0), Some(0));
        for i in 1..=4 {
            assert_eq!(t.allow(t0 + Duration::from_secs(i)), None);
        }
        assert_eq!(t.allow(t0 + Duration::from_secs(10)), Some(4));
        assert_eq!(t.allow(t0 + Duration::from_secs(11)), None);
        assert_eq!(t.allow(t0 + Duration::from_secs(20)), Some(1));
    }

    #[test]
    fn a_loop_that_stops_counting_twice_is_stalled() {
        let mut w = StallWatch::new();
        // The first check only establishes a baseline.
        assert_eq!(w.observe(10, 4), Stalled::None);
        assert_eq!(w.observe(20, 8), Stalled::None);
        // One unchanged check is not enough (a long park is legal).
        assert_eq!(w.observe(20, 12), Stalled::None);
        assert_eq!(w.observe(20, 16), Stalled::Shipping);
        // Progress clears the strikes.
        assert_eq!(w.observe(30, 20), Stalled::None);
        assert_eq!(w.observe(40, 20), Stalled::None);
        assert_eq!(w.observe(50, 20), Stalled::Context);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn report_shows_rates_and_deltas() {
        let prev = StatsSnapshot {
            events: 1_000,
            batches: 40,
            ring_drops: 2,
            drains: 100,
            ..Default::default()
        };
        let now = StatsSnapshot {
            events: 6_000,
            batches: 240,
            ring_drops: 5,
            drains: 1_600,
            report_interval_us: 1_000,
            ..Default::default()
        };
        let r = compute_report(&prev, &now, 5.0, 3);
        assert_eq!(r.events_per_s, 1000.0);
        assert_eq!(r.batches_per_s, 40.0);
        assert_eq!((r.drops, r.drops_delta), (5, 3));
        assert_eq!(r.idle_for_s, 3);
        // 1500 drains over 5s, 5000 events: 300 drains/s, ~3.3 reports each.
        assert_eq!(r.drains_per_s, 300.0);
        assert!((r.reports_per_drain - 10.0 / 3.0).abs() < 1e-9);
        assert_eq!(r.poll_hz, Some(1_000.0));
    }

    #[cfg(feature = "observability")]
    #[test]
    fn report_tolerates_zero_elapsed_and_no_drains() {
        let z = StatsSnapshot::default();
        let r = compute_report(&z, &z, 0.0, 0);
        assert_eq!(r.events_per_s, 0.0);
        assert_eq!(r.capture_to_ship_us_p99.to_string(), "-");
        assert_eq!(r.reports_per_drain, 0.0);
        assert_eq!(r.poll_hz, None);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn per_sink_deltas_drive_the_escalation_predicate() {
        let prev = StatsSnapshot {
            kafka_dropped: 10,
            jsonl_errors: 1,
            ..Default::default()
        };
        let now = StatsSnapshot {
            kafka_dropped: 410,
            jsonl_errors: 1,
            ..Default::default()
        };
        let r = compute_report(&prev, &now, 5.0, 0);
        assert_eq!(r.kafka_dropped_delta, 400);
        assert_eq!(r.jsonl_errors_delta, 0);
        assert!(r.sink_is_losing("kafka"));
        assert!(!r.sink_is_losing("jsonl"));
        assert!(!r.sink_is_losing("udp"), "udp is best-effort by design");
    }

    #[cfg(feature = "observability")]
    #[test]
    fn idle_seconds_needs_a_first_event() {
        assert_eq!(idle_for_secs(0, 10_000_000_000, 10_000_000), 0);
        assert_eq!(idle_for_secs(1_000, 1_000, 10_000_000), 0);
        assert_eq!(idle_for_secs(1_000, 1_000 + 35_000_000, 10_000_000), 3);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn a_condition_warns_once_per_interval_while_it_holds() {
        let mut l = ConditionLimiter::new(Duration::from_secs(60));
        let t0 = Instant::now();
        // Nothing wrong: nothing said, and no entry is kept for it.
        assert_eq!(l.observe("kafka", false, t0), Escalation::Quiet);
        assert_eq!(l.observe("kafka", true, t0), Escalation::Warn);
        for i in 1..=5 {
            assert_eq!(
                l.observe("kafka", true, t0 + Duration::from_secs(i * 5)),
                Escalation::Silent
            );
        }
        assert_eq!(
            l.observe("kafka", true, t0 + Duration::from_secs(60)),
            Escalation::Warn
        );
    }

    #[cfg(feature = "observability")]
    #[test]
    fn a_condition_that_clears_recovers_once_and_rearms() {
        let mut l = ConditionLimiter::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(l.observe("idle", true, t0), Escalation::Warn);
        assert_eq!(
            l.observe("idle", false, t0 + Duration::from_secs(5)),
            Escalation::Recovered
        );
        // The recovery is reported exactly once.
        assert_eq!(
            l.observe("idle", false, t0 + Duration::from_secs(10)),
            Escalation::Quiet
        );
        // Recurring inside the interval still warns: it is a new incident.
        assert_eq!(
            l.observe("idle", true, t0 + Duration::from_secs(15)),
            Escalation::Warn
        );
    }

    #[cfg(feature = "observability")]
    #[test]
    fn conditions_do_not_silence_each_other() {
        let mut l = ConditionLimiter::new(ESCALATION_INTERVAL);
        let t0 = Instant::now();
        assert_eq!(l.observe("kafka", true, t0), Escalation::Warn);
        assert_eq!(l.observe("jsonl", true, t0), Escalation::Warn);
        assert_eq!(l.observe("ring", true, t0), Escalation::Warn);
        assert_eq!(l.observe("kafka", true, t0), Escalation::Silent);
        assert_eq!(l.observe("jsonl", false, t0), Escalation::Recovered);
        assert_eq!(l.observe("kafka", true, t0), Escalation::Silent);
    }
}
