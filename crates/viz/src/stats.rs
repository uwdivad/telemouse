//! Counters for the periodic observability log line, the `/api/stats`
//! endpoint, and the `viz_stats` frame pushed to the page every second.
//!
//! Metrics are logs here (per `docs/CONVENTIONS.md`), so this is mostly a bag
//! of atomics plus a snapshot type the periodic reporter diffs against the
//! previous sample. The histograms are
//! [`telemouse_core::histogram::LatencyHist`] — the same 250 µs buckets the
//! capture agent reports from, so a p99 measured here is comparable with a p99
//! measured there — with the exact sum / max / negative counters this crate
//! reports kept beside them.
//!
//! Everything the `observability` feature gates lives in [`Obs`]. With the
//! feature off the bridge still counts datagrams, clients and the UDP bind
//! (what the client cap and the minimal `/healthz` need) and nothing else; the
//! recording calls below become no-ops rather than `cfg` blocks at every call
//! site.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

#[cfg(feature = "observability")]
use std::sync::Mutex;
#[cfg(feature = "observability")]
use telemouse_core::histogram::{LatencyHist, LatencySnapshot as HistSnapshot};

/// Wall-clock UTC microseconds — the workspace-wide definition, so the
/// bridge's latency estimate subtracts the same clock the capture agent
/// stamped `ts_anchor_us` with.
pub use telemouse_core::now_utc_us;

/// Envelope tag of the stats frame pushed through the hub to the page.
#[cfg(feature = "observability")]
pub const VIZ_STATS_TYPE: &str = "viz_stats";

/// A bound UDP listener that has not seen a datagram for this long is not
/// idle, it is stalled: the capture agent is gone, or its sink stopped. What
/// `/healthz` says, and what the periodic report logs a transition on.
#[cfg(feature = "observability")]
pub const FEED_STALL_AFTER_S: f64 = 10.0;

/// How the live feed looks from here, for `/healthz` and the stall log:
/// `never` (nothing has ever arrived), `live`, or `stalled`.
#[cfg(feature = "observability")]
pub fn feed_state(udp_bound: bool, age_s: Option<f64>) -> &'static str {
    match age_s {
        _ if !udp_bound => "never",
        None => "never",
        Some(age) if age > FEED_STALL_AFTER_S => "stalled",
        Some(_) => "live",
    }
}

/// Point-in-time read of one latency window, in the shape `/api/stats` and the
/// page have always seen.
#[cfg(feature = "observability")]
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

/// One window of inter-datagram gaps: how evenly the capture agent's batches
/// are actually arriving. A p99 far above the agent's batch window is a
/// scheduling problem somewhere, not a rate problem.
#[cfg(feature = "observability")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GapSnapshot {
    pub samples: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

#[cfg(feature = "observability")]
impl GapSnapshot {
    pub fn p99_ms(&self) -> f64 {
        self.p99_us as f64 / 1000.0
    }

    pub fn max_ms(&self) -> f64 {
        self.max_us as f64 / 1000.0
    }
}

/// The counters only the `observability` feature has a consumer for.
#[cfg(feature = "observability")]
#[derive(Debug, Default)]
struct Obs {
    /// Bridge latency, cumulative. Windows are deltas against `latency_prev`.
    latency: LatencyHist,
    latency_prev: Mutex<HistSnapshot>,
    /// Exact sum / max / negative count for the *current* window: the
    /// histogram gives percentiles, these give the numbers a bucket edge
    /// cannot (a mean, and an outlier's real magnitude).
    win_sum_us: AtomicU64,
    win_max_us: AtomicU64,
    win_negative: AtomicU64,
    /// The most recently *completed* latency window, so `/api/stats` and the
    /// pushed frame report a stable number instead of one that resets to
    /// near-empty every reporting interval.
    last_latency: Mutex<LatencySnapshot>,
    /// Time between consecutive datagrams, same treatment.
    gaps: LatencyHist,
    gaps_prev: Mutex<HistSnapshot>,
    win_gap_max_us: AtomicU64,
    last_gap: Mutex<GapSnapshot>,
    /// Bytes of accepted envelope text handed to the broadcast channel.
    bytes_forwarded: AtomicU64,
    /// Envelopes missing from the capture agent's `seq_no` sequence.
    seq_gaps: AtomicU64,
    /// Broadcast-channel occupancy at the last publish, and the high-water
    /// mark since the last report.
    queue_depth: AtomicU64,
    queue_depth_max: AtomicU64,
    /// Most recent per-interval rates (f64 bits), published by the periodic
    /// reporter so `/api/stats` can answer without its own snapshot history.
    rate_bits: AtomicU64,
    kb_bits: AtomicU64,
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
    #[cfg(feature = "observability")]
    obs: Obs,
    /// The UDP listener is bound and receiving. False until the bind
    /// succeeds — and forever if it never does, which is what `/healthz`
    /// exists to say.
    udp_bound: AtomicBool,
    /// Wall clock (UTC µs) of the most recent datagram, 0 before the first.
    last_datagram_utc_us: AtomicI64,
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
            #[cfg(feature = "observability")]
            obs: Obs::default(),
            udp_bound: AtomicBool::new(false),
            last_datagram_utc_us: AtomicI64::new(0),
            started: Instant::now(),
        }
    }
}

/// What `/healthz` returns. `ok` is false while the UDP listener is not
/// bound: the page and replay still work, but live mode cannot, and a
/// health check that said "ok" then would be lying about the one thing it
/// is asked.
///
/// The feed fields are `observability`; `{ok, udp_bound, uptime_s}` is always
/// there, so a supervisor's health check does not depend on a feature.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthPayload {
    pub ok: bool,
    pub udp_bound: bool,
    pub uptime_s: f64,
    /// Seconds since the last datagram, `None` before the first one.
    #[cfg(feature = "observability")]
    pub last_datagram_age_s: Option<f64>,
    #[cfg(feature = "observability")]
    pub clients: i64,
    /// Bound, but nothing has arrived for [`FEED_STALL_AFTER_S`].
    #[cfg(feature = "observability")]
    pub stalled: bool,
    /// `never` | `live` | `stalled`.
    #[cfg(feature = "observability")]
    pub feed: &'static str,
    #[cfg(feature = "observability")]
    pub udp_addr: String,
    #[cfg(feature = "observability")]
    pub http_addr: String,
    #[cfg(feature = "observability")]
    pub version: &'static str,
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

    pub fn uptime_s(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn set_udp_bound(&self, bound: bool) {
        self.udp_bound.store(bound, Ordering::Relaxed);
    }

    pub fn udp_bound(&self) -> bool {
        self.udp_bound.load(Ordering::Relaxed)
    }

    /// A datagram arrived (parseable or not) at wall-clock `now_utc_us`.
    /// Returns the gap since the previous one in µs, or `None` for the first.
    pub fn note_datagram(&self, now_utc_us: i64) -> Option<i64> {
        let prev = self
            .last_datagram_utc_us
            .swap(now_utc_us, Ordering::Relaxed);
        (prev != 0).then(|| now_utc_us - prev)
    }

    /// Seconds since the last datagram, or `None` before the first.
    #[cfg(feature = "observability")]
    pub fn last_datagram_age_s(&self, now_utc_us: i64) -> Option<f64> {
        let last = self.last_datagram_utc_us.load(Ordering::Relaxed);
        (last != 0).then(|| (now_utc_us - last).max(0) as f64 / 1e6)
    }

    /// The `/healthz` body at wall-clock `now_utc_us`.
    pub fn health(&self, now_utc_us: i64, udp_addr: &str, http_addr: &str) -> HealthPayload {
        let udp_bound = self.udp_bound();
        #[cfg(feature = "observability")]
        let age = self.last_datagram_age_s(now_utc_us);
        #[cfg(not(feature = "observability"))]
        let _ = (now_utc_us, udp_addr, http_addr);
        HealthPayload {
            ok: udp_bound,
            udp_bound,
            uptime_s: self.uptime_s(),
            #[cfg(feature = "observability")]
            last_datagram_age_s: age,
            #[cfg(feature = "observability")]
            clients: self.clients.load(Ordering::Relaxed),
            #[cfg(feature = "observability")]
            stalled: feed_state(udp_bound, age) == "stalled",
            #[cfg(feature = "observability")]
            feed: feed_state(udp_bound, age),
            #[cfg(feature = "observability")]
            udp_addr: udp_addr.to_string(),
            #[cfg(feature = "observability")]
            http_addr: http_addr.to_string(),
            #[cfg(feature = "observability")]
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// The recording side of the observability counters. Stubbed out (see the
/// `not(observability)` impl below) rather than `cfg`-ed at every call site,
/// so the hub reads the same either way.
#[cfg(feature = "observability")]
impl Stats {
    /// One bridge-latency observation, in µs. Negative values (clock skew
    /// between the capture host and this one) are counted separately and
    /// clamped to zero rather than thrown away — a nonzero `negative` is the
    /// tell that the numbers need a grain of salt.
    pub fn record_latency(&self, us: i64) {
        if self.obs.latency.record_clamped(us) {
            self.obs.win_negative.fetch_add(1, Ordering::Relaxed);
        }
        let v = us.max(0) as u64;
        self.obs.win_sum_us.fetch_add(v, Ordering::Relaxed);
        self.obs.win_max_us.fetch_max(v, Ordering::Relaxed);
    }

    /// One inter-datagram gap, in µs.
    pub fn record_gap(&self, us: i64) {
        self.obs.gaps.record_clamped(us);
        self.obs
            .win_gap_max_us
            .fetch_max(us.max(0) as u64, Ordering::Relaxed);
    }

    /// Bytes of one forwarded envelope.
    pub fn note_bytes(&self, n: u64) {
        self.obs.bytes_forwarded.fetch_add(n, Ordering::Relaxed);
    }

    /// `n` envelopes were missing from the `seq_no` sequence.
    pub fn note_seq_gap(&self, n: u64) {
        self.obs.seq_gaps.fetch_add(n, Ordering::Relaxed);
    }

    /// Broadcast-channel occupancy observed at a publish.
    pub fn note_queue_depth(&self, depth: u64) {
        self.obs.queue_depth.store(depth, Ordering::Relaxed);
        self.obs.queue_depth_max.fetch_max(depth, Ordering::Relaxed);
    }

    pub fn bytes_forwarded(&self) -> u64 {
        self.obs.bytes_forwarded.load(Ordering::Relaxed)
    }

    pub fn seq_gaps(&self) -> u64 {
        self.obs.seq_gaps.load(Ordering::Relaxed)
    }

    pub fn queue_depth(&self) -> u64 {
        self.obs.queue_depth.load(Ordering::Relaxed)
    }

    /// The high-water queue depth since the last call, cleared as it is read.
    pub fn take_queue_depth_max(&self) -> u64 {
        self.obs.queue_depth_max.swap(0, Ordering::Relaxed)
    }

    /// Close the current latency window and make it the reported one.
    pub fn roll_latency(&self) -> LatencySnapshot {
        let done = self.latency_window(true);
        if done.samples > 0 {
            *self
                .obs
                .last_latency
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = done;
        }
        done
    }

    /// Close the current gap window and make it the reported one.
    pub fn roll_gaps(&self) -> GapSnapshot {
        let done = self.gap_window(true);
        if done.samples > 0 {
            *self.obs.last_gap.lock().unwrap_or_else(|p| p.into_inner()) = done;
        }
        done
    }

    /// Latency to report: the last completed window, falling back to the
    /// in-progress one before the first roll (so a page opened in the first
    /// few seconds still sees numbers).
    pub fn latency_snapshot(&self) -> LatencySnapshot {
        let last = *self
            .obs
            .last_latency
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if last.samples > 0 {
            last
        } else {
            self.latency_window(false)
        }
    }

    /// The gap window to report, same rule.
    pub fn gap_snapshot(&self) -> GapSnapshot {
        let last = *self.obs.last_gap.lock().unwrap_or_else(|p| p.into_inner());
        if last.samples > 0 {
            last
        } else {
            self.gap_window(false)
        }
    }

    /// The window since the last roll. `reset` advances the baseline and
    /// clears the exact counters, closing the window.
    fn latency_window(&self, reset: bool) -> LatencySnapshot {
        let cur = self.obs.latency.snapshot();
        let mut prev = self
            .obs
            .latency_prev
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let window = cur.delta(&prev);
        if reset {
            *prev = cur;
        }
        drop(prev);
        let (sum_us, max_us, negative) = if reset {
            (
                self.obs.win_sum_us.swap(0, Ordering::Relaxed),
                self.obs.win_max_us.swap(0, Ordering::Relaxed),
                self.obs.win_negative.swap(0, Ordering::Relaxed),
            )
        } else {
            (
                self.obs.win_sum_us.load(Ordering::Relaxed),
                self.obs.win_max_us.load(Ordering::Relaxed),
                self.obs.win_negative.load(Ordering::Relaxed),
            )
        };
        let samples = window.total();
        LatencySnapshot {
            samples,
            // The histogram reports a bucket's upper edge; the exact maximum
            // is known, so never overstate it.
            p50_us: window.percentile_us(0.50).min(max_us),
            p99_us: window.percentile_us(0.99).min(max_us),
            max_us,
            mean_us: sum_us.checked_div(samples).unwrap_or(0),
            negative,
        }
    }

    fn gap_window(&self, reset: bool) -> GapSnapshot {
        let cur = self.obs.gaps.snapshot();
        let mut prev = self.obs.gaps_prev.lock().unwrap_or_else(|p| p.into_inner());
        let window = cur.delta(&prev);
        if reset {
            *prev = cur;
        }
        drop(prev);
        let max_us = if reset {
            self.obs.win_gap_max_us.swap(0, Ordering::Relaxed)
        } else {
            self.obs.win_gap_max_us.load(Ordering::Relaxed)
        };
        GapSnapshot {
            samples: window.total(),
            p99_us: window.percentile_us(0.99).min(max_us),
            max_us,
        }
    }

    /// Publish the interval rates the periodic reporter just computed.
    pub fn set_datagrams_per_s(&self, v: f64) {
        self.obs.rate_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn datagrams_per_s(&self) -> f64 {
        f64::from_bits(self.obs.rate_bits.load(Ordering::Relaxed))
    }

    pub fn set_kb_per_s(&self, v: f64) {
        self.obs.kb_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn kb_per_s(&self) -> f64 {
        f64::from_bits(self.obs.kb_bits.load(Ordering::Relaxed))
    }

    /// The JSON body shared by `/api/stats` and the pushed `viz_stats` frame.
    pub fn payload(&self, session_cached: bool) -> StatsPayload {
        let s = self.snapshot();
        let gaps = self.gap_snapshot();
        StatsPayload {
            kind: VIZ_STATS_TYPE,
            uptime_s: self.uptime_s(),
            datagrams: s.datagrams,
            datagrams_per_s: self.datagrams_per_s(),
            forwarded: s.forwarded,
            parse_errors: s.parse_errors,
            lag_drops: s.lag_drops,
            lag_disconnects: s.lag_disconnects,
            clients: s.clients,
            session_cached,
            latency: self.latency_snapshot(),
            seq_gaps: self.seq_gaps(),
            queue_depth: self.queue_depth(),
            queue_depth_max: self.obs.queue_depth_max.load(Ordering::Relaxed),
            bytes_forwarded: self.bytes_forwarded(),
            kb_per_s: self.kb_per_s(),
            gap_p99_ms: gaps.p99_ms(),
            gap_max_ms: gaps.max_ms(),
        }
    }
}

/// Without `observability` nothing consumes these, so recording one is a call
/// the optimizer deletes rather than a `cfg` at the call site.
#[cfg(not(feature = "observability"))]
impl Stats {
    pub fn record_latency(&self, _us: i64) {}
    pub fn record_gap(&self, _us: i64) {}
    pub fn note_bytes(&self, _n: u64) {}
    pub fn note_queue_depth(&self, _depth: u64) {}
}

/// What `/api/stats` returns and what the page receives once a second over the
/// WebSocket. Identical shape on both paths on purpose: one parser in the page.
#[cfg(feature = "observability")]
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
    /// Envelopes missing from the capture agent's `seq_no` sequence, as the
    /// bridge saw it (the page counts its own, over what reached the socket).
    pub seq_gaps: u64,
    pub queue_depth: u64,
    pub queue_depth_max: u64,
    pub bytes_forwarded: u64,
    pub kb_per_s: f64,
    pub gap_p99_ms: f64,
    pub gap_max_ms: f64,
}

#[cfg(feature = "observability")]
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

#[cfg(feature = "observability")]
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
    fn note_datagram_returns_the_gap_since_the_previous_one() {
        let s = Stats::default();
        let now = 1_756_000_000_000_000;
        assert_eq!(s.note_datagram(now), None, "the first has no predecessor");
        assert_eq!(s.note_datagram(now + 25_000), Some(25_000));
    }

    #[cfg(feature = "observability")]
    #[test]
    fn feed_state_says_never_live_or_stalled() {
        assert_eq!(feed_state(false, None), "never");
        assert_eq!(
            feed_state(false, Some(0.1)),
            "never",
            "no listener, no feed"
        );
        assert_eq!(feed_state(true, None), "never");
        assert_eq!(feed_state(true, Some(0.1)), "live");
        assert_eq!(feed_state(true, Some(FEED_STALL_AFTER_S)), "live");
        assert_eq!(feed_state(true, Some(FEED_STALL_AFTER_S + 0.1)), "stalled");
    }

    #[test]
    fn health_is_not_ok_until_udp_is_bound() {
        let s = Stats::default();
        let now = 1_756_000_000_000_000;
        let h = s.health(now, "127.0.0.1:7878", "127.0.0.1:7879");
        assert!(!h.ok);
        assert!(!h.udp_bound);
        assert!(h.uptime_s >= 0.0);

        s.set_udp_bound(true);
        let h = s.health(now, "127.0.0.1:7878", "127.0.0.1:7879");
        assert!(h.ok && h.udp_bound);

        let v: serde_json::Value = serde_json::to_value(s.health(now, "u", "h")).unwrap();
        for key in ["ok", "udp_bound", "uptime_s"] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
    }

    #[cfg(feature = "observability")]
    #[test]
    fn health_reports_the_feed_and_the_addresses() {
        let s = Stats::default();
        let now = 1_756_000_000_000_000;
        s.set_udp_bound(true);
        let h = s.health(now, "127.0.0.1:7878", "0.0.0.0:7879");
        assert_eq!(h.feed, "never");
        assert!(!h.stalled);
        assert_eq!(h.last_datagram_age_s, None);
        assert_eq!(h.udp_addr, "127.0.0.1:7878");
        assert_eq!(h.http_addr, "0.0.0.0:7879");
        assert_eq!(h.version, env!("CARGO_PKG_VERSION"));

        s.note_datagram(now - 2_500_000);
        s.client_connected();
        let h = s.health(now, "u", "h");
        assert_eq!(h.feed, "live");
        assert!(!h.stalled);
        assert!((h.last_datagram_age_s.unwrap() - 2.5).abs() < 1e-9);
        assert_eq!(h.clients, 1);

        // A datagram stamped in the future (clock skew) reads as age 0.
        s.note_datagram(now + 1_000_000);
        assert_eq!(s.last_datagram_age_s(now), Some(0.0));

        s.note_datagram(now - 30_000_000);
        let h = s.health(now, "u", "h");
        assert_eq!(h.feed, "stalled");
        assert!(h.stalled);
        assert!(h.ok, "a stalled feed is still a bound listener");
    }

    #[cfg(feature = "observability")]
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

    #[cfg(feature = "observability")]
    #[test]
    fn an_empty_latency_window_is_all_zeroes() {
        let s = Stats::default();
        assert_eq!(s.latency_snapshot(), LatencySnapshot::default());
    }

    #[cfg(feature = "observability")]
    #[test]
    fn latency_percentiles_land_in_the_right_bucket() {
        let s = Stats::default();
        // 99 samples at ~1ms, one at ~50ms: p50 must stay near 1ms, p99 must
        // pick up neither (the 99th of 100 is still the fast bucket).
        for _ in 0..99 {
            s.record_latency(1_000);
        }
        s.record_latency(50_000);
        let w = s.latency_snapshot();
        assert_eq!(w.samples, 100);
        // 1000µs lands in bucket 4 (1000..1250), whose upper edge is 1250.
        assert_eq!(w.p50_us, 1_250);
        assert_eq!(w.p99_us, 1_250);
        assert_eq!(w.max_us, 50_000, "the exact outlier, not a bucket edge");
        assert_eq!(w.mean_us, (99 * 1_000 + 50_000) / 100);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn latency_p99_catches_a_two_percent_tail() {
        let s = Stats::default();
        for _ in 0..98 {
            s.record_latency(500);
        }
        for _ in 0..2 {
            s.record_latency(80_000);
        }
        let w = s.latency_snapshot();
        assert!(w.p50_us <= 750, "p50 {} should stay fast", w.p50_us);
        assert!(w.p99_us >= 80_000, "p99 {} should see the tail", w.p99_us);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn a_percentile_never_exceeds_the_observed_max() {
        let s = Stats::default();
        s.record_latency(10);
        let w = s.latency_snapshot();
        assert_eq!(w.max_us, 10);
        assert_eq!(w.p50_us, 10, "the bucket edge is clamped to the real max");
        assert_eq!(w.p99_us, 10);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn negative_latency_is_counted_and_clamped() {
        let s = Stats::default();
        s.record_latency(-5_000);
        s.record_latency(1_000);
        let w = s.latency_snapshot();
        assert_eq!(w.samples, 2);
        assert_eq!(w.negative, 1);
        assert_eq!(w.max_us, 1_000, "the negative sample clamps to 0");
    }

    #[cfg(feature = "observability")]
    #[test]
    fn a_very_large_latency_keeps_its_exact_max() {
        let s = Stats::default();
        s.record_latency(5_000_000); // 5 seconds
        let w = s.latency_snapshot();
        assert_eq!(w.samples, 1);
        assert_eq!(w.max_us, 5_000_000);
        // The shared 250 µs histogram saturates: anything past its range
        // reports as the overflow bucket, never a smaller number.
        assert!(
            w.p99_us >= telemouse_core::histogram::OVERFLOW_FROM_US,
            "{}",
            w.p99_us
        );
    }

    #[cfg(feature = "observability")]
    #[test]
    fn roll_latency_closes_the_window_and_publishes_it() {
        let s = Stats::default();
        assert_eq!(s.latency_snapshot().samples, 0);

        s.record_latency(3_000);
        assert_eq!(s.latency_snapshot().samples, 1, "in-progress window");

        let done = s.roll_latency();
        assert_eq!(done.samples, 1);
        // Rolled: the completed window is reported, and it survives an empty
        // interval rather than blinking back to zero.
        assert_eq!(s.latency_snapshot().samples, 1);
        assert_eq!(s.roll_latency().samples, 0, "the window really did close");
        assert_eq!(s.latency_snapshot().samples, 1);

        s.record_latency(9_000);
        let done = s.roll_latency();
        assert_eq!(done.samples, 1);
        assert_eq!(done.max_us, 9_000, "the max is per-window, not cumulative");
    }

    #[cfg(feature = "observability")]
    #[test]
    fn gaps_are_a_window_of_their_own() {
        let s = Stats::default();
        for _ in 0..99 {
            s.record_gap(25_000);
        }
        s.record_gap(400_000);
        let g = s.gap_snapshot();
        assert_eq!(g.samples, 100);
        assert_eq!(g.max_us, 400_000);
        assert!((g.max_ms() - 400.0).abs() < 1e-9);
        assert!(g.p99_ms() >= 25.0 && g.p99_ms() <= 26.0, "{}", g.p99_ms());
        assert_eq!(s.roll_gaps().samples, 100);
        assert_eq!(s.roll_gaps().samples, 0);
        assert_eq!(s.gap_snapshot().samples, 100, "the last window is kept");
    }

    #[cfg(feature = "observability")]
    #[test]
    fn queue_depth_keeps_a_high_water_mark_until_it_is_read() {
        let s = Stats::default();
        s.note_queue_depth(3);
        s.note_queue_depth(11);
        s.note_queue_depth(1);
        assert_eq!(s.queue_depth(), 1, "the last observation");
        assert_eq!(s.take_queue_depth_max(), 11);
        assert_eq!(s.take_queue_depth_max(), 0, "cleared as it is read");
    }

    #[cfg(feature = "observability")]
    #[test]
    fn payload_has_the_documented_shape() {
        let s = Stats::default();
        s.datagrams.fetch_add(7, Ordering::Relaxed);
        s.parse_errors.fetch_add(1, Ordering::Relaxed);
        s.record_latency(4_000);
        s.record_gap(25_000);
        s.note_bytes(2_048);
        s.note_seq_gap(3);
        s.note_queue_depth(5);
        s.set_datagrams_per_s(40.0);
        s.set_kb_per_s(12.5);
        s.roll_latency();
        s.roll_gaps();

        let p = s.payload(true);
        assert_eq!(p.kind, VIZ_STATS_TYPE);
        assert_eq!(p.datagrams, 7);
        assert_eq!(p.parse_errors, 1);
        assert!(p.session_cached);
        assert_eq!(p.latency.samples, 1);
        assert_eq!(p.seq_gaps, 3);
        assert_eq!(p.bytes_forwarded, 2_048);
        assert_eq!(p.queue_depth, 5);
        assert_eq!(p.queue_depth_max, 5);
        assert!((p.kb_per_s - 12.5).abs() < 1e-9);
        assert!((p.gap_max_ms - 25.0).abs() < 1e-9);

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
            "seq_gaps",
            "queue_depth",
            "queue_depth_max",
            "bytes_forwarded",
            "kb_per_s",
            "gap_p99_ms",
            "gap_max_ms",
        ] {
            assert!(v.get(key).is_some(), "missing {key} in /api/stats payload");
        }
        for key in [
            "samples", "p50_us", "p99_us", "max_us", "mean_us", "negative",
        ] {
            assert!(v["latency"].get(key).is_some(), "missing latency.{key}");
        }
    }
}
