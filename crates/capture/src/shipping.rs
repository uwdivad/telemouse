//! T2 — the shipping thread.
//!
//! Owns the ring consumer, the [`Batcher`], the batch/marker sequence numbers
//! and the drop / absolute-frame accounting, and fans finished envelopes out to
//! the sinks. All of that policy lives in [`ShipperCore`], which is pure and
//! testable; [`run`] is only the thread + timing shell around it.
//!
//! The loop **parks** rather than polling: T1 unparks it when the ring goes
//! non-empty (see [`crate::raw_input::RingWaker`]), and a short park timeout —
//! sized to whatever is left of the current batch window — guarantees the
//! time-based flush still fires on an idle desk.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use telemouse_core::{Batch, Batcher, Envelope, Marker, QpcAnchor, RawEvent, SessionConfig};

use crate::context::SharedContext;
use crate::platform;
use crate::raw_input::RingWaker;
use crate::sinks::{EnvelopeEncoder, Sink, fan_out, tick_all};
use crate::stats::Stats;

/// Longest the loop ever sleeps between drains: the batch window, so a partial
/// batch is never more than one window late.
const MAX_PARK: Duration = Duration::from_millis(25);
/// Shortest park, so a nearly-expired window does not turn into a spin.
const MIN_PARK: Duration = Duration::from_micros(500);
/// Upper bound on events drained per iteration so markers and flushes still get
/// serviced under a flood.
const DRAIN_BUDGET: usize = 8_192;
const TICK_INTERVAL: Duration = Duration::from_secs(1);
/// A misbehaving sink warns at most this often, with a suppressed count.
pub const WARN_INTERVAL: Duration = Duration::from_secs(10);

/// A hotkey press handed over from T1 (or a note from T3). Markers are rare, so
/// a plain channel is the right tool — the SPSC ring stays reserved for the hot
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerSignal {
    pub ts_qpc: u64,
    pub label: String,
}

/// Turns a monotonic total counter into per-batch deltas.
#[derive(Debug, Default, Clone, Copy)]
pub struct DropAccountant {
    last: u32,
}

impl DropAccountant {
    pub fn new() -> Self {
        Self::default()
    }

    /// Events since the previous call. Wrapping so a counter that laps `u32`
    /// still yields a sane delta instead of a huge one.
    pub fn delta(&mut self, total: u32) -> u32 {
        let d = total.wrapping_sub(self.last);
        self.last = total;
        d
    }
}

/// Rate limiter for per-sink failure warnings.
///
/// With no viz running, a broken sink can fail on every batch (~40/s). One
/// warning per sink per [`WARN_INTERVAL`], carrying how many were suppressed,
/// says the same thing without drowning the log.
#[derive(Debug, Default)]
pub struct WarnLimiter {
    entries: Vec<WarnEntry>,
}

#[derive(Debug)]
struct WarnEntry {
    sink: &'static str,
    next_at: Instant,
    suppressed: u64,
}

impl WarnLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Should this failure be logged now? `Some(suppressed)` says yes and
    /// reports how many were swallowed since the last one; `None` says no.
    pub fn allow(&mut self, sink: &'static str, now: Instant) -> Option<u64> {
        match self.entries.iter_mut().find(|e| e.sink == sink) {
            None => {
                self.entries.push(WarnEntry {
                    sink,
                    next_at: now + WARN_INTERVAL,
                    suppressed: 0,
                });
                Some(0)
            }
            Some(entry) if now >= entry.next_at => {
                let suppressed = std::mem::take(&mut entry.suppressed);
                entry.next_at = now + WARN_INTERVAL;
                Some(suppressed)
            }
            Some(entry) => {
                entry.suppressed += 1;
                None
            }
        }
    }
}

/// The shipping policy: accumulate, decide, assemble.
pub struct ShipperCore {
    session_id: String,
    anchor: QpcAnchor,
    batcher: Batcher,
    batch_seq: u64,
    marker_seq: u64,
    drops: DropAccountant,
    abs_frames: DropAccountant,
    window_ticks: u64,
    /// QPC of the batch's first event, mirrored so the loop can work out how
    /// long it may park before the window expires.
    first_qpc: Option<u64>,
}

impl ShipperCore {
    pub fn new(session_id: String, anchor: QpcAnchor, max_events: usize, window_ms: u64) -> Self {
        Self {
            session_id,
            batcher: Batcher::with_window_ms(max_events, window_ms, anchor.qpc_freq),
            anchor,
            batch_seq: 0,
            marker_seq: 0,
            drops: DropAccountant::new(),
            abs_frames: DropAccountant::new(),
            window_ticks: anchor.ms_to_ticks(window_ms),
            first_qpc: None,
        }
    }

    pub fn anchor(&self) -> QpcAnchor {
        self.anchor
    }

    pub fn push(&mut self, ev: RawEvent) {
        if self.first_qpc.is_none() {
            self.first_qpc = Some(ev.ts_qpc);
        }
        self.batcher.push(ev);
    }

    pub fn should_flush(&self, now_qpc: u64) -> bool {
        self.batcher.should_flush(now_qpc)
    }

    pub fn pending(&self) -> usize {
        self.batcher.len()
    }

    /// How long the loop may park before the current batch window expires.
    /// With nothing pending there is no deadline, so it parks the full window.
    pub fn park_hint(&self, now_qpc: u64) -> Duration {
        let Some(first) = self.first_qpc else {
            return MAX_PARK;
        };
        let elapsed = now_qpc.saturating_sub(first);
        let remaining_ticks = self.window_ticks.saturating_sub(elapsed);
        let us = (remaining_ticks as u128 * 1_000_000 / self.anchor.qpc_freq.max(1) as u128) as u64;
        Duration::from_micros(us).clamp(MIN_PARK, MAX_PARK)
    }

    /// Assemble the accumulated events into a batch. `None` when empty.
    pub fn build_batch(
        &mut self,
        ctx: &crate::context::ContextSnapshot,
        drops_total: u32,
        abs_frames_total: u32,
    ) -> Option<Batch> {
        if self.batcher.is_empty() {
            return None;
        }
        let events = self.batcher.take();
        self.first_qpc = None;
        let ts_anchor_us = self.anchor.qpc_to_utc_us(events[0].ts_qpc);
        let (cursor_x, cursor_y) = ctx.batch_cursor();
        let seq_no = self.batch_seq;
        self.batch_seq += 1;
        Some(Batch {
            session_id: self.session_id.clone(),
            seq_no,
            ts_anchor_us,
            game: ctx.game.clone(),
            pointer_locked: ctx.pointer_locked,
            screen_w: ctx.screen_w,
            screen_h: ctx.screen_h,
            cursor_x,
            cursor_y,
            drops_since_last: self.drops.delta(drops_total),
            abs_frames_since_last: self.abs_frames.delta(abs_frames_total),
            events,
        })
    }

    pub fn build_marker(&mut self, ts_qpc: u64, label: String) -> Marker {
        let seq_no = self.marker_seq;
        self.marker_seq += 1;
        Marker {
            session_id: self.session_id.clone(),
            seq_no,
            ts_qpc,
            ts_utc_us: self.anchor.qpc_to_utc_us(ts_qpc),
            label,
        }
    }
}

/// Deliver one envelope everywhere, isolating and counting sink failures.
///
/// `payload` is the envelope already serialized once for the whole fan-out.
pub fn deliver(
    sinks: &mut [Box<dyn Sink>],
    stats: &Stats,
    limiter: &mut WarnLimiter,
    env: &Envelope,
    payload: &str,
) {
    for failure in fan_out(sinks, env, payload) {
        stats.count_sink_error(failure.sink);
        if let Some(suppressed) = limiter.allow(failure.sink, Instant::now()) {
            tracing::warn!(
                sink = failure.sink,
                error = %failure.error,
                suppressed,
                "sink send failed"
            );
        }
    }
}

/// Serialize then deliver. Returns false if the envelope could not be encoded
/// at all (which is a bug, not a sink failure, so it is logged loudly).
fn encode_and_deliver(
    enc: &mut EnvelopeEncoder,
    sinks: &mut [Box<dyn Sink>],
    stats: &Stats,
    limiter: &mut WarnLimiter,
    env: &Envelope,
) -> bool {
    if let Err(e) = enc.encode(env) {
        tracing::error!(error = %format!("{e:#}"), "could not serialize envelope");
        return false;
    }
    deliver(sinks, stats, limiter, env, enc.payload());
    true
}

pub struct ShippingArgs {
    pub session: SessionConfig,
    pub window_ms: u64,
    pub max_events: usize,
    /// Log a one-line summary per batch (the `--print` flag).
    pub print: bool,
    /// Set once T1 has stopped: the loop drains the ring and returns.
    pub capture_stopped: Arc<AtomicBool>,
    /// T1 unparks this thread through here when the ring goes non-empty.
    pub waker: Arc<RingWaker>,
}

/// The T2 thread body. Returns when `capture_stopped` is set and the ring has
/// been drained; the partial batch is always flushed before returning.
pub fn run(
    mut consumer: rtrb::Consumer<RawEvent>,
    marker_rx: Receiver<MarkerSignal>,
    mut sinks: Vec<Box<dyn Sink>>,
    ctx: Arc<SharedContext>,
    stats: Arc<Stats>,
    args: ShippingArgs,
) {
    let capture_stopped = Arc::clone(&args.capture_stopped);
    let waker = Arc::clone(&args.waker);
    waker.register(std::thread::current());

    let anchor = args.session.anchor;
    let mut core = ShipperCore::new(
        args.session.session_id.clone(),
        anchor,
        args.max_events,
        args.window_ms,
    );
    let mut enc = EnvelopeEncoder::new();
    let mut limiter = WarnLimiter::new();

    // The session record goes out first, so a recording's first line is always
    // the session envelope.
    encode_and_deliver(
        &mut enc,
        &mut sinks,
        &stats,
        &mut limiter,
        &Envelope::Session(args.session.clone()),
    );

    let mut last_tick = Instant::now();
    loop {
        let stopping = capture_stopped.load(Ordering::Acquire);
        stats.observe_ring_slots(consumer.slots() as u64);

        // Drain the ring, flushing whenever the batcher says so. Checking after
        // every push is what keeps a batch from exceeding `max_events` (and so
        // the UDP datagram budget) under a burst.
        let mut drained = 0usize;
        while let Ok(ev) = consumer.pop() {
            let ts = ev.ts_qpc;
            core.push(ev);
            if core.should_flush(ts) {
                flush(
                    &mut core,
                    &mut sinks,
                    &stats,
                    &ctx,
                    &mut enc,
                    &mut limiter,
                    args.print,
                );
            }
            drained += 1;
            if drained >= DRAIN_BUDGET {
                break;
            }
        }

        while let Ok(sig) = marker_rx.try_recv() {
            let marker = core.build_marker(sig.ts_qpc, sig.label);
            stats.markers.fetch_add(1, Ordering::Relaxed);
            tracing::info!(seq = marker.seq_no, label = %marker.label, "marker");
            encode_and_deliver(
                &mut enc,
                &mut sinks,
                &stats,
                &mut limiter,
                &Envelope::Marker(marker),
            );
        }

        // Time-based flush for a partial batch whose window has elapsed.
        if core.should_flush(platform::qpc()) {
            flush(
                &mut core,
                &mut sinks,
                &stats,
                &ctx,
                &mut enc,
                &mut limiter,
                args.print,
            );
        }

        if last_tick.elapsed() >= TICK_INTERVAL {
            for failure in tick_all(&mut sinks) {
                stats.count_sink_error(failure.sink);
                if let Some(suppressed) = limiter.allow(failure.sink, Instant::now()) {
                    tracing::warn!(
                        sink = failure.sink,
                        error = %failure.error,
                        suppressed,
                        "sink tick failed"
                    );
                }
            }
            last_tick = Instant::now();
        }

        if stopping && drained == 0 && core.pending() == 0 {
            break;
        }
        if drained < DRAIN_BUDGET {
            let timeout = core.park_hint(platform::qpc());
            waker.begin_park();
            // Re-check: T1 may have pushed between the drain above and the flag
            // going up. Missing that check would cost one park timeout.
            if consumer.is_empty() {
                std::thread::park_timeout(timeout);
            }
            waker.end_park();
        }
    }

    // Final flush of whatever is left.
    flush(
        &mut core,
        &mut sinks,
        &stats,
        &ctx,
        &mut enc,
        &mut limiter,
        args.print,
    );
    for failure in tick_all(&mut sinks) {
        stats.count_sink_error(failure.sink);
    }
    tracing::info!(
        batches = stats.batches.load(Ordering::Relaxed),
        events = stats.events(),
        drops = stats.ring_drops(),
        "shipping thread finished"
    );
}

#[allow(clippy::too_many_arguments)]
fn flush(
    core: &mut ShipperCore,
    sinks: &mut [Box<dyn Sink>],
    stats: &Stats,
    ctx: &SharedContext,
    enc: &mut EnvelopeEncoder,
    limiter: &mut WarnLimiter,
    print: bool,
) {
    // The context is only needed here (~40×/s), never per drained event.
    let snapshot = ctx.get();
    let Some(batch) = core.build_batch(&snapshot, stats.ring_drops(), stats.abs_frames()) else {
        return;
    };
    stats.batches.fetch_add(1, Ordering::Relaxed);
    record_latency(core.anchor(), stats, &batch);
    if print {
        let (dx, dy) = batch.total_counts();
        tracing::info!(
            seq = batch.seq_no,
            events = batch.events.len(),
            dx,
            dy,
            drops = batch.drops_since_last,
            abs_frames = batch.abs_frames_since_last,
            game = batch.game.as_deref().unwrap_or("-"),
            locked = batch.pointer_locked,
            "batch"
        );
    }
    encode_and_deliver(enc, sinks, stats, limiter, &Envelope::Batch(batch));
}

/// Record how long the batch's oldest and newest events waited to be shipped.
fn record_latency(anchor: QpcAnchor, stats: &Stats, batch: &Batch) {
    let now = platform::qpc();
    let (Some(first), Some(last)) = (batch.events.first(), batch.events.last()) else {
        return;
    };
    stats
        .ship_latency_first
        .record(anchor.ticks_to_us(first.ts_qpc, now).max(0) as u64);
    stats
        .ship_latency_last
        .record(anchor.ticks_to_us(last.ts_qpc, now).max(0) as u64);
}

#[cfg(test)]
mod tests {
    use telemouse_core::event::buttons;

    use super::*;
    use crate::context::ContextSnapshot;
    use crate::sinks::mock::{FailingSink, RecordingSink};

    const FREQ: u64 = 10_000_000;

    fn anchor() -> QpcAnchor {
        QpcAnchor {
            qpc: 1_000_000,
            utc_us: 1_756_000_000_000_000,
            qpc_freq: FREQ,
        }
    }

    fn ev(ts_qpc: u64, dx: i32) -> RawEvent {
        RawEvent {
            ts_qpc,
            dx,
            dy: 1,
            ..Default::default()
        }
    }

    fn ctx() -> ContextSnapshot {
        ContextSnapshot {
            game: Some("cs2.exe".into()),
            pointer_locked: false,
            screen_w: 2560,
            screen_h: 1440,
            cursor_x: 7,
            cursor_y: 9,
        }
    }

    fn shared_ctx() -> SharedContext {
        SharedContext::new(ctx())
    }

    #[test]
    fn drop_deltas_are_differences_of_a_monotonic_counter() {
        let mut acc = DropAccountant::new();
        assert_eq!(acc.delta(0), 0);
        assert_eq!(acc.delta(5), 5);
        assert_eq!(acc.delta(5), 0);
        assert_eq!(acc.delta(9), 4);
        assert_eq!(acc.delta(1_000), 991);
    }

    #[test]
    fn drop_deltas_survive_a_wrapping_counter() {
        let mut acc = DropAccountant::new();
        acc.delta(u32::MAX - 1);
        assert_eq!(acc.delta(1), 3); // MAX-1 -> MAX -> 0 -> 1
    }

    #[test]
    fn drops_are_attributed_per_batch() {
        let mut core = ShipperCore::new("s-1".into(), anchor(), 2, 25);
        let mut totals = [0u32, 3, 3, 10].into_iter();
        let mut seen = Vec::new();
        for chunk in 0..4 {
            core.push(ev(anchor().qpc + chunk, 1));
            core.push(ev(anchor().qpc + chunk, 1));
            let b = core
                .build_batch(&ctx(), totals.next().unwrap(), 0)
                .unwrap();
            seen.push(b.drops_since_last);
        }
        assert_eq!(seen, vec![0, 3, 0, 7]);
    }

    #[test]
    fn absolute_frames_are_attributed_per_batch_independently_of_drops() {
        let mut core = ShipperCore::new("s-1".into(), anchor(), 2, 25);
        let cases = [(0u32, 0u32), (2, 5), (2, 5), (7, 6)];
        let mut seen = Vec::new();
        for (drops, abs) in cases {
            core.push(ev(anchor().qpc, 1));
            core.push(ev(anchor().qpc, 1));
            let b = core.build_batch(&ctx(), drops, abs).unwrap();
            seen.push((b.drops_since_last, b.abs_frames_since_last));
        }
        assert_eq!(seen, vec![(0, 0), (2, 5), (0, 0), (5, 1)]);
    }

    #[test]
    fn batches_break_on_max_events_and_number_sequentially() {
        let mut core = ShipperCore::new("s-1".into(), anchor(), 3, 25);
        let mut batches = Vec::new();
        for i in 0..7u64 {
            let e = ev(anchor().qpc + i, 2);
            core.push(e);
            if core.should_flush(e.ts_qpc) {
                batches.push(core.build_batch(&ctx(), 0, 0).unwrap());
            }
        }
        // 7 events, max 3 => two full batches, one event still pending.
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].seq_no, 0);
        assert_eq!(batches[1].seq_no, 1);
        assert!(batches.iter().all(|b| b.events.len() == 3));
        assert_eq!(core.pending(), 1);
        let last = core.build_batch(&ctx(), 0, 0).unwrap();
        assert_eq!(last.seq_no, 2);
        assert_eq!(last.events.len(), 1);
        assert!(core.build_batch(&ctx(), 0, 0).is_none());
    }

    #[test]
    fn batches_break_on_the_window_and_map_ts_through_the_anchor() {
        let a = anchor();
        let mut core = ShipperCore::new("s-1".into(), a, 1_000, 25);
        core.push(ev(a.qpc, 1));
        // 24ms later: still inside the 25ms window.
        assert!(!core.should_flush(a.qpc + FREQ * 24 / 1000));
        core.push(ev(a.qpc + FREQ * 24 / 1000, 1));
        // 25ms after the first event: flush.
        assert!(core.should_flush(a.qpc + FREQ * 25 / 1000));
        let b = core.build_batch(&ctx(), 0, 0).unwrap();
        assert_eq!(b.events.len(), 2);
        // ts_anchor_us is the *first* event mapped onto UTC.
        assert_eq!(b.ts_anchor_us, a.utc_us);

        // A batch starting 1s after the anchor maps 1s later.
        core.push(ev(a.qpc + FREQ, 1));
        let b2 = core.build_batch(&ctx(), 0, 0).unwrap();
        assert_eq!(b2.ts_anchor_us, a.utc_us + 1_000_000);
        assert_eq!(b2.seq_no, 1);
    }

    #[test]
    fn the_park_hint_tracks_the_remaining_window() {
        let a = anchor();
        let mut core = ShipperCore::new("s-1".into(), a, 1_000, 25);
        // Nothing pending: park the whole window.
        assert_eq!(core.park_hint(a.qpc), MAX_PARK);

        core.push(ev(a.qpc, 1));
        // 10ms in: 15ms of window left.
        let hint = core.park_hint(a.qpc + FREQ * 10 / 1000);
        assert_eq!(hint, Duration::from_millis(15));
        // Past the window: clamped to the floor, never zero (no spin).
        assert_eq!(core.park_hint(a.qpc + FREQ), MIN_PARK);

        // Taking the batch clears the deadline again.
        core.build_batch(&ctx(), 0, 0).unwrap();
        assert_eq!(core.park_hint(a.qpc + FREQ), MAX_PARK);
    }

    #[test]
    fn batch_carries_the_context_snapshot() {
        let mut core = ShipperCore::new("s-1".into(), anchor(), 10, 25);
        core.push(ev(anchor().qpc, 1));
        let b = core.build_batch(&ctx(), 0, 0).unwrap();
        assert_eq!(b.game.as_deref(), Some("cs2.exe"));
        assert_eq!((b.screen_w, b.screen_h), (2560, 1440));
        assert_eq!((b.cursor_x, b.cursor_y), (Some(7), Some(9)));

        let locked = ContextSnapshot {
            pointer_locked: true,
            ..ctx()
        };
        core.push(ev(anchor().qpc, 1));
        let b = core.build_batch(&locked, 0, 0).unwrap();
        assert!(b.pointer_locked);
        assert_eq!((b.cursor_x, b.cursor_y), (None, None));
    }

    #[test]
    fn markers_get_their_own_sequence_and_utc_mapping() {
        let a = anchor();
        let mut core = ShipperCore::new("s-1".into(), a, 10, 25);
        core.push(ev(a.qpc, 1));
        core.build_batch(&ctx(), 0, 0).unwrap();

        let m0 = core.build_marker(a.qpc + FREQ / 2, "hotkey".into());
        let m1 = core.build_marker(a.qpc + FREQ, "anchor_drift_us=42".into());
        assert_eq!((m0.seq_no, m1.seq_no), (0, 1)); // independent of batch seq
        assert_eq!(m0.ts_utc_us, a.utc_us + 500_000);
        assert_eq!(m1.ts_utc_us, a.utc_us + 1_000_000);
        assert_eq!(m0.session_id, "s-1");
        assert_eq!(m0.label, "hotkey");
    }

    #[test]
    fn warnings_are_rate_limited_per_sink_with_a_suppressed_count() {
        let mut l = WarnLimiter::new();
        let t0 = Instant::now();
        // First failure for a sink always warns.
        assert_eq!(l.allow("udp", t0), Some(0));
        // Everything inside the window is swallowed...
        for i in 1..=5 {
            assert_eq!(l.allow("udp", t0 + Duration::from_millis(i * 100)), None);
        }
        // ...and the next one past it reports the backlog.
        assert_eq!(l.allow("udp", t0 + WARN_INTERVAL), Some(5));
        // The counter resets after being reported.
        assert_eq!(l.allow("udp", t0 + WARN_INTERVAL + Duration::from_secs(1)), None);
        assert_eq!(l.allow("udp", t0 + WARN_INTERVAL * 2), Some(1));
    }

    #[test]
    fn each_sink_gets_its_own_warn_budget() {
        let mut l = WarnLimiter::new();
        let t0 = Instant::now();
        assert_eq!(l.allow("udp", t0), Some(0));
        // A different sink is not silenced by udp's warning.
        assert_eq!(l.allow("kafka", t0), Some(0));
        assert_eq!(l.allow("udp", t0), None);
        assert_eq!(l.allow("kafka", t0), None);
        assert_eq!(l.allow("jsonl", t0), Some(0));
    }

    /// The shipping loop's core, exercised end to end without threads: feed
    /// synthetic events through the batcher and the sink fan-out.
    #[test]
    fn shipping_core_drives_sinks_with_correct_boundaries() {
        let a = anchor();
        let stats = Stats::default();
        let good = RecordingSink::new("udp");
        let other = RecordingSink::new("jsonl");
        let mut sinks: Vec<Box<dyn Sink>> = vec![
            Box::new(good.clone()),
            Box::new(FailingSink::new("kafka")),
            Box::new(other.clone()),
        ];
        let mut core = ShipperCore::new("s-1".into(), a, 4, 25);
        let context = shared_ctx();
        let mut enc = EnvelopeEncoder::new();
        let mut limiter = WarnLimiter::new();

        // 10 events, one per ms: max_events (4) decides the boundaries.
        for i in 0..10u64 {
            let e = RawEvent {
                ts_qpc: a.qpc + i * (FREQ / 1000),
                dx: 1,
                dy: -1,
                buttons: if i == 3 { buttons::LEFT_DOWN } else { 0 },
                ..Default::default()
            };
            core.push(e);
            if core.should_flush(e.ts_qpc) {
                flush(
                    &mut core,
                    &mut sinks,
                    &stats,
                    &context,
                    &mut enc,
                    &mut limiter,
                    false,
                );
            }
        }
        // Final partial.
        flush(
            &mut core,
            &mut sinks,
            &stats,
            &context,
            &mut enc,
            &mut limiter,
            false,
        );

        assert_eq!(good.count(), 3); // 4 + 4 + 2
        assert_eq!(other.count(), 3); // the failing sink starved nobody
        assert_eq!(stats.snapshot().kafka_errors, 3);
        assert_eq!(stats.snapshot().batches, 3);
        // Three failures, one warning: the rest were rate-limited away.
        assert_eq!(limiter.allow("kafka", Instant::now()), None);

        let received = good.received.lock().unwrap().clone();
        let batches: Vec<Batch> = received
            .into_iter()
            .map(|e| match e {
                Envelope::Batch(b) => b,
                other => panic!("unexpected envelope {other:?}"),
            })
            .collect();
        assert_eq!(
            batches.iter().map(|b| b.events.len()).collect::<Vec<_>>(),
            vec![4, 4, 2]
        );
        assert_eq!(
            batches.iter().map(|b| b.seq_no).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Each batch is anchored on its own first event: 0ms, 4ms, 8ms.
        assert_eq!(
            batches.iter().map(|b| b.ts_anchor_us).collect::<Vec<_>>(),
            vec![a.utc_us, a.utc_us + 4_000, a.utc_us + 8_000]
        );
        assert_eq!(batches[0].total_counts(), (4, -4));
        assert!(batches[0].events[3].is_click_down());

        // Every sink got the same JSON, and it parses back to the same batch.
        let payloads = good.payloads.lock().unwrap().clone();
        assert_eq!(payloads, *other.payloads.lock().unwrap());
        assert!(payloads[0].contains(r#""type":"batch""#));

        // The flush path recorded shipping latency for every batch.
        let hist = stats.snapshot().ship_latency_first;
        assert_eq!(hist.total(), 3);
    }
}
