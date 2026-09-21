//! T2 — the shipping thread.
//!
//! Owns the ring consumer, the [`Batcher`], the batch/marker sequence numbers
//! and the drop / absolute-frame accounting, and fans finished envelopes out to
//! the sinks. All of that policy lives in [`ShipperCore`], which is pure and
//! testable; [`run`] is only the thread + timing shell around it.
//!
//! The loop **parks** rather than polling: with no batch open, T1 unparks it
//! on the first event (see [`crate::raw_input::RingWaker`]); once a batch is
//! open it sleeps out the remainder of the batch window and drains whatever
//! accumulated in one go. That makes the wakeup rate ~1/window rather than
//! one per mouse report — at 1kHz the per-event wake/park cycle was most of
//! the agent's CPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use telemouse_core::batch::total_counts;
use telemouse_core::{
    BatchView, Batcher, Envelope, EnvelopeView, Heartbeat, Marker, QpcAnchor, RawEvent,
    SessionConfig,
};

use crate::context::SharedContext;
use crate::platform;
use crate::raw_input::RingWaker;
use crate::sinks::{EnvelopeEncoder, Sink, fan_out, send_to, tick_all};
use crate::stats::Stats;

/// Shortest park, so a nearly-expired window does not turn into a spin.
const MIN_PARK: Duration = Duration::from_micros(500);
/// Upper bound on events drained per iteration so markers and flushes still get
/// serviced under a flood.
const DRAIN_BUDGET: usize = 8_192;
const TICK_INTERVAL: Duration = Duration::from_secs(1);
/// Park length once the loop is *confirmed* idle: only the once-a-second sink
/// tick needs a timer then, so an idle desk costs one wakeup a second instead
/// of forty. Reached via a two-stage descent (see [`next_park_timeout`]): the
/// first empty-ring park is only [`MAX_PARK`], so a wake lost to the
/// [`RingWaker`] race costs at most one batch window, never a full second.
const IDLE_PARK: Duration = TICK_INTERVAL;
/// A misbehaving sink warns at most this often, with a suppressed count.
pub const WARN_INTERVAL: Duration = Duration::from_secs(10);
/// How often the session envelope is repeated to the live viz — and only to
/// it (see [`send_live`]).
pub const SESSION_RESEND_INTERVAL: Duration = Duration::from_secs(5);
/// How often an otherwise silent agent says hello to the live viz.
///
/// A still mouse produces no events and therefore no batches, which on the
/// viz side is indistinguishable from a capture agent that died — the OBS
/// overlay said "no feed" at a player who was holding angle. One second is
/// comfortably under the overlay's smallest useful `stale_secs` (default 3),
/// and it costs no extra wakeup: once the loop is confirmed idle it already
/// parks exactly [`IDLE_PARK`] for the sink tick, so the heartbeat rides a
/// wake that was happening anyway.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Longest the loop ever sleeps between drains: the configured batch window,
/// so a partial batch is never more than one window late.
///
/// It follows `batch.window_ms` rather than being fixed, because both ends of
/// the range were wrong at 25ms: a 50ms window got probed twice per batch for
/// nothing, and a 5ms window had its deadline missed by 20ms — the window is
/// the only deadline this loop has, so it is the only sensible park.
pub fn max_park(window_ms: u64) -> Duration {
    Duration::from_millis(window_ms).clamp(MIN_PARK, IDLE_PARK)
}

/// Park timeout for an empty-ring park: the two-stage idle descent.
///
/// T1's `RingWaker` wake can, rarely, be lost to the relaxed-load race while
/// this thread is on its way into `park` — the park timeout is the backstop.
/// So the FIRST park after the ring goes empty is only the batch window
/// (`max_park`): a lost wake costs at most one window. Only when that probe
/// park times out with the ring *still* empty (`prev_timed_out`, judged by the
/// caller) does the loop descend to [`IDLE_PARK`]; anything arriving — a
/// successful wake or a non-empty ring — resets the descent. Price: one extra
/// wakeup per descent into idle.
pub fn next_park_timeout(prev_timed_out: bool, ring_empty: bool, max_park: Duration) -> Duration {
    if prev_timed_out && ring_empty {
        IDLE_PARK
    } else {
        max_park
    }
}

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

/// Decides when the live viz needs an idle heartbeat.
///
/// The batch counter is the whole input: any progress at all means the viz is
/// already hearing from us and needs nothing extra, and the quiet clock only
/// starts once that counter stops moving. Pure, so "no heartbeat while
/// batches flow" is a test rather than a claim.
///
/// [`due`](Self::due) is meant to be called once per shipping-loop iteration
/// rather than once per tick: while events are flowing the loop turns over
/// every batch window, so the quiet clock starts within a window of the last
/// batch instead of up to a second late.
#[derive(Debug)]
pub struct HeartbeatGate {
    interval: Duration,
    batches: u64,
    quiet_since: Instant,
}

impl HeartbeatGate {
    pub fn new(batches: u64, now: Instant) -> Self {
        Self::with_interval(HEARTBEAT_INTERVAL, batches, now)
    }

    /// Same, at a different cadence — for the tests.
    pub fn with_interval(interval: Duration, batches: u64, now: Instant) -> Self {
        Self {
            interval,
            batches,
            quiet_since: now,
        }
    }

    /// True when a heartbeat is due now: the batch counter has not moved
    /// since the last call, and it has been quiet for a full interval.
    pub fn due(&mut self, batches: u64, now: Instant) -> bool {
        if batches != self.batches {
            self.batches = batches;
            self.quiet_since = now;
            return false;
        }
        if now.duration_since(self.quiet_since) < self.interval {
            return false;
        }
        self.quiet_since = now;
        true
    }
}

/// Rate limiter for per-sink failure warnings.
///
/// With no viz running, a broken sink can fail on every batch (~40/s). One
/// warning per sink per [`WARN_INTERVAL`], carrying how many were suppressed,
/// says the same thing without drowning the log.
#[derive(Debug)]
pub struct WarnLimiter {
    interval: Duration,
    entries: Vec<WarnEntry>,
}

impl Default for WarnLimiter {
    fn default() -> Self {
        Self::with_interval(WARN_INTERVAL)
    }
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

    /// Same, at a different cadence — for a caller whose failures are rarer
    /// or noisier than a per-batch sink send.
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            entries: Vec::new(),
        }
    }

    /// Should this failure be logged now? `Some(suppressed)` says yes and
    /// reports how many were swallowed since the last one; `None` says no.
    pub fn allow(&mut self, sink: &'static str, now: Instant) -> Option<u64> {
        let interval = self.interval;
        match self.entries.iter_mut().find(|e| e.sink == sink) {
            None => {
                self.entries.push(WarnEntry {
                    sink,
                    next_at: now + interval,
                    suppressed: 0,
                });
                Some(0)
            }
            Some(entry) if now >= entry.next_at => {
                let suppressed = std::mem::take(&mut entry.suppressed);
                entry.next_at = now + interval;
                Some(suppressed)
            }
            Some(entry) => {
                entry.suppressed += 1;
                None
            }
        }
    }
}

/// The per-batch header values [`ShipperCore::next_batch_meta`] hands out
/// alongside the borrowed events when a batch is flushed.
#[derive(Debug, Clone, Copy)]
pub struct BatchMeta {
    pub seq_no: u64,
    pub ts_anchor_us: i64,
    pub drops_since_last: u32,
    pub abs_frames_since_last: u32,
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
    max_park: Duration,
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
            max_park: max_park(window_ms),
        }
    }

    #[cfg_attr(not(feature = "observability"), allow(dead_code))]
    pub fn anchor(&self) -> QpcAnchor {
        self.anchor
    }

    /// The longest park this core's window justifies (see [`max_park`]).
    pub fn max_park(&self) -> Duration {
        self.max_park
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn push(&mut self, ev: RawEvent) {
        self.batcher.push(ev);
    }

    pub fn should_flush(&self, now_qpc: u64) -> bool {
        self.batcher.should_flush(now_qpc)
    }

    pub fn pending(&self) -> usize {
        self.batcher.len()
    }

    /// The accumulated events, borrowed — a flushed `BatchView` serializes
    /// straight over this slice.
    pub fn events(&self) -> &[RawEvent] {
        self.batcher.events()
    }

    /// How long the loop may park before the current batch window expires.
    /// With nothing pending there is no deadline, so it parks the full window.
    pub fn park_hint(&self, now_qpc: u64) -> Duration {
        let Some(first) = self.batcher.first_qpc() else {
            return self.max_park;
        };
        let elapsed = now_qpc.saturating_sub(first);
        let remaining_ticks = self.window_ticks.saturating_sub(elapsed);
        let us = (remaining_ticks as u128 * 1_000_000 / self.anchor.qpc_freq.max(1) as u128) as u64;
        Duration::from_micros(us).clamp(MIN_PARK, self.max_park)
    }

    /// Claim the header for the accumulated batch — sequence number, anchor
    /// timestamp and the per-batch deltas — leaving the events in place.
    /// `None` when empty. The caller serializes a [`BatchView`] over
    /// [`Self::events`] and then calls [`Self::finish_batch`]; nothing on that
    /// path clones a `String` or surrenders the event `Vec`.
    pub fn next_batch_meta(
        &mut self,
        drops_total: u32,
        abs_frames_total: u32,
    ) -> Option<BatchMeta> {
        let first = self.batcher.first_qpc()?;
        let seq_no = self.batch_seq;
        self.batch_seq += 1;
        Some(BatchMeta {
            seq_no,
            ts_anchor_us: self.anchor.qpc_to_utc_us(first),
            drops_since_last: self.drops.delta(drops_total),
            abs_frames_since_last: self.abs_frames.delta(abs_frames_total),
        })
    }

    /// Done with the flushed batch: clear it, keeping the event capacity.
    pub fn finish_batch(&mut self) {
        self.batcher.reset();
    }

    /// Assemble the accumulated events into an owned batch. `None` when empty.
    /// Test convenience only — the production flush path serializes a
    /// borrowing [`BatchView`] instead (see [`flush`]).
    #[cfg(test)]
    pub fn build_batch(
        &mut self,
        ctx: &crate::context::ContextSnapshot,
        drops_total: u32,
        abs_frames_total: u32,
    ) -> Option<telemouse_core::Batch> {
        let meta = self.next_batch_meta(drops_total, abs_frames_total)?;
        let (cursor_x, cursor_y) = ctx.batch_cursor();
        let batch = telemouse_core::Batch {
            session_id: self.session_id.clone(),
            seq_no: meta.seq_no,
            ts_anchor_us: meta.ts_anchor_us,
            game: ctx.game.clone(),
            pointer_locked: ctx.pointer_locked,
            screen_w: ctx.screen_w,
            screen_h: ctx.screen_h,
            cursor_x,
            cursor_y,
            drops_since_last: meta.drops_since_last,
            abs_frames_since_last: meta.abs_frames_since_last,
            events: self.batcher.events().to_vec(),
        };
        self.finish_batch();
        Some(batch)
    }

    /// The "still here, hand still" datagram for the live viz. Carries no
    /// sequence number by design (see [`telemouse_core::Heartbeat`]), so it
    /// takes `&self` and has no counter to advance.
    pub fn build_heartbeat(&self, now_qpc: u64) -> Heartbeat {
        Heartbeat {
            session_id: self.session_id.clone(),
            ts_utc_us: self.anchor.qpc_to_utc_us(now_qpc),
        }
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
/// `payload` is the envelope already serialized once for the whole fan-out;
/// `topic`/`key` carry its routing.
pub fn deliver(
    sinks: &mut [Box<dyn Sink>],
    stats: &Stats,
    limiter: &mut WarnLimiter,
    topic: &'static str,
    key: &str,
    payload: &str,
) {
    for failure in fan_out(sinks, topic, key, payload) {
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

/// Serialize then deliver an owned envelope (sessions and markers — batches go
/// through the borrowing view in [`flush`]). Returns false if the envelope
/// could not be encoded at all (which is a bug, not a sink failure, so it is
/// logged loudly).
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
    deliver(sinks, stats, limiter, env.topic(), env.key(), enc.payload());
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
    /// Run once at shutdown, after every sink but Kafka has been closed —
    /// i.e. after the recording's final flush and before Kafka's bounded
    /// drain. The sidecar is written here so a drain that runs out of time
    /// still leaves a document whose counters match the recording on disk.
    pub on_recording_closed: Option<Box<dyn FnOnce() + Send>>,
}

/// The T2 thread body. Returns when `capture_stopped` is set and the ring has
/// been drained; the partial batch is always flushed before returning.
pub fn run(
    mut consumer: rtrb::Consumer<RawEvent>,
    marker_rx: Receiver<MarkerSignal>,
    mut sinks: Vec<Box<dyn Sink>>,
    ctx: Arc<SharedContext>,
    stats: Arc<Stats>,
    mut args: ShippingArgs,
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
    // the session envelope. It is kept, because the live viz gets it again
    // every few seconds (see `resend_session`).
    let session_env = Envelope::Session(args.session.clone());
    encode_and_deliver(&mut enc, &mut sinks, &stats, &mut limiter, &session_env);

    let mut last_tick = Instant::now();
    let mut last_resend = Instant::now();
    let mut heartbeat = HeartbeatGate::new(stats.batches.load(Ordering::Relaxed), Instant::now());
    // Two-stage idle descent: true once an empty-ring park has already timed
    // out with the ring still empty, i.e. the loop is confirmed idle.
    let mut idle_probe_expired = false;
    loop {
        stats.t2_iters.fetch_add(1, Ordering::Relaxed);
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

        // A viz started after the agent has no session record and cannot
        // convert anything to cm or degrees until it gets one.
        if last_resend.elapsed() >= SESSION_RESEND_INTERVAL {
            last_resend = Instant::now();
            send_live(
                &mut enc,
                &mut sinks,
                &stats,
                &mut limiter,
                &session_env,
                "session",
            );
        }

        // Nothing moved for a second: tell the live viz we are still here, so
        // a still hand does not read as a dead agent. Polled every iteration
        // (it is two loads and a compare) so the quiet clock starts within a
        // batch window of the last batch, but it can only *fire* on a wake,
        // and once idle those are the once-a-second parks the tick already
        // needs. Never while shutting down: the last thing on the wire should
        // be the final batch.
        if !stopping && heartbeat.due(stats.batches.load(Ordering::Relaxed), Instant::now()) {
            let env = Envelope::Heartbeat(core.build_heartbeat(platform::qpc()));
            send_live(
                &mut enc,
                &mut sinks,
                &stats,
                &mut limiter,
                &env,
                "heartbeat",
            );
        }

        if stopping && drained == 0 && core.pending() == 0 {
            break;
        }
        if drained < DRAIN_BUDGET {
            let timeout = core.park_hint(platform::qpc());
            if core.pending() == 0 {
                // Nothing in flight: T1 wakes us on the first event (so the
                // batch window starts promptly), markers and shutdown wake us
                // explicitly, and the only timed work left is the sink tick.
                waker.begin_park();
                // Re-check: T1 may have pushed between the drain above and the
                // flag going up. Missing that check would cost one park timeout.
                // A marker handed over in this tiny window rides the next
                // wake or the tick, at worst a second late — markers are rare
                // and every sender also calls `wake()` after sending.
                if consumer.is_empty() {
                    // Descend to the long idle park only once a batch-window
                    // park has confirmed the ring is really idle: a wake lost
                    // to the RingWaker race (see its docs) then costs at most
                    // MAX_PARK, not IDLE_PARK, for one extra wakeup per
                    // descent into idle.
                    let idle_timeout = next_park_timeout(idle_probe_expired, true, core.max_park());
                    let parked_at = Instant::now();
                    std::thread::park_timeout(idle_timeout);
                    idle_probe_expired = parked_at.elapsed() >= idle_timeout && consumer.is_empty();
                } else {
                    idle_probe_expired = false;
                }
                waker.end_park();
            } else {
                // A batch is open: its window is the only deadline that
                // matters, so sleep it out and let events pile up in the ring.
                // Waking per event here cost a syscall on T1 and a full
                // wake→pop→park cycle on this thread for every mouse report.
                idle_probe_expired = false;
                std::thread::park_timeout(timeout);
            }
        } else {
            idle_probe_expired = false;
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

    // Close the sinks in two stages. Everything but Kafka goes first, because
    // the JSONL writer's final flush is what makes the recording complete;
    // the callback then writes the sidecar with the counters as they stand.
    // Only after that does Kafka's *bounded* drain run — it can time out and
    // abandon envelopes, and if it does, the sidecar on disk is already
    // accurate about everything else.
    let (kafka, rest): (Vec<_>, Vec<_>) = sinks.into_iter().partition(|s| s.name() == "kafka");
    drop(rest);
    if let Some(closed) = args.on_recording_closed.take() {
        closed();
    }
    drop(kafka);

    tracing::info!(
        batches = stats.batches.load(Ordering::Relaxed),
        events = stats.events(),
        drops = stats.ring_drops(),
        "shipping thread finished"
    );
}

/// Deliver an envelope to the live viz, and *only* to it.
///
/// Two things ride this path, and neither belongs anywhere else:
///
/// * the periodic **session re-send** — a viz launched mid-session otherwise
///   shows raw counts forever, since the session record carries the CPI, the
///   device names, the monitor list and the anchor. A second `session` line
///   in a recording would change what the file means, and consumers take the
///   first session envelope of a stream anyway.
/// * the idle **heartbeat** — liveness, not telemetry. A line a second on
///   disk and a message a second on the broker, both saying nothing, is
///   exactly what the recording and Kafka do not need.
///
/// `what` only names the thing in the failure warning.
fn send_live(
    enc: &mut EnvelopeEncoder,
    sinks: &mut [Box<dyn Sink>],
    stats: &Stats,
    limiter: &mut WarnLimiter,
    env: &Envelope,
    what: &'static str,
) {
    if let Err(e) = enc.encode(env) {
        tracing::error!(error = %format!("{e:#}"), what, "could not serialize envelope");
        return;
    }
    let Some(failure) = send_to(sinks, "udp", env.topic(), env.key(), enc.payload()) else {
        return;
    };
    stats.count_sink_error(failure.sink);
    if let Some(suppressed) = limiter.allow(failure.sink, Instant::now()) {
        tracing::warn!(
            sink = failure.sink,
            error = %failure.error,
            suppressed,
            what,
            "live-only send failed"
        );
    }
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
    let Some(meta) = core.next_batch_meta(stats.ring_drops(), stats.abs_frames()) else {
        return;
    };
    stats.batches.fetch_add(1, Ordering::Relaxed);
    if print {
        let (dx, dy) = total_counts(core.events());
        tracing::info!(
            seq = meta.seq_no,
            events = core.events().len(),
            dx,
            dy,
            drops = meta.drops_since_last,
            abs_frames = meta.abs_frames_since_last,
            game = snapshot.game.as_deref().unwrap_or("-"),
            locked = snapshot.pointer_locked,
            "batch"
        );
    }
    // Steady-state allocation-free: the view borrows the session id, the
    // context snapshot and the batcher's events in place — no `String` clones,
    // no fresh `Vec` — and serializes into the encoder's reused buffer.
    let (cursor_x, cursor_y) = snapshot.batch_cursor();
    let view = EnvelopeView::Batch(BatchView {
        session_id: core.session_id(),
        seq_no: meta.seq_no,
        ts_anchor_us: meta.ts_anchor_us,
        game: snapshot.game.as_deref(),
        pointer_locked: snapshot.pointer_locked,
        screen_w: snapshot.screen_w,
        screen_h: snapshot.screen_h,
        cursor_x,
        cursor_y,
        drops_since_last: meta.drops_since_last,
        abs_frames_since_last: meta.abs_frames_since_last,
        events: core.events(),
    });
    if let Err(e) = enc.encode(&view) {
        tracing::error!(error = %format!("{e:#}"), "could not serialize envelope");
    } else {
        deliver(
            sinks,
            stats,
            limiter,
            view.topic(),
            view.key(),
            enc.payload(),
        );
    }
    // Measured *after* the fan-out, not before it: capture→ship is supposed
    // to include the ship. Taken before, it left out the one part of the path
    // that can actually stall (a slow JSONL flush, a Kafka queue backing up)
    // and reported a flattering number precisely when things were going
    // wrong.
    #[cfg(feature = "observability")]
    record_latency(core.anchor(), stats, core.events());
    core.finish_batch();
}

/// Record how long the batch's oldest and newest events waited to be shipped.
#[cfg(feature = "observability")]
fn record_latency(anchor: QpcAnchor, stats: &Stats, events: &[RawEvent]) {
    let now = platform::qpc();
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        return;
    };
    stats
        .ship_latency_first
        .record_clamped(anchor.ticks_to_us(first.ts_qpc, now));
    stats
        .ship_latency_last
        .record_clamped(anchor.ticks_to_us(last.ts_qpc, now));
}

#[cfg(test)]
mod tests {
    use telemouse_core::Batch;
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
            let b = core.build_batch(&ctx(), totals.next().unwrap(), 0).unwrap();
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
    fn the_longest_park_follows_the_configured_window() {
        assert_eq!(max_park(25), Duration::from_millis(25));
        assert_eq!(max_park(50), Duration::from_millis(50));
        assert_eq!(max_park(5), Duration::from_millis(5));
        // Never a spin, and never longer than the idle park is anyway.
        assert_eq!(max_park(0), MIN_PARK);
        assert_eq!(max_park(u64::MAX), IDLE_PARK);
        // And the core carries the one its window justifies.
        assert_eq!(
            ShipperCore::new("s".into(), anchor(), 10, 50).max_park(),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn the_park_hint_tracks_the_remaining_window() {
        let a = anchor();
        let mut core = ShipperCore::new("s-1".into(), a, 1_000, 25);
        // Nothing pending: park the whole window.
        assert_eq!(core.park_hint(a.qpc), Duration::from_millis(25));

        core.push(ev(a.qpc, 1));
        // 10ms in: 15ms of window left.
        let hint = core.park_hint(a.qpc + FREQ * 10 / 1000);
        assert_eq!(hint, Duration::from_millis(15));
        // Past the window: clamped to the floor, never zero (no spin).
        assert_eq!(core.park_hint(a.qpc + FREQ), MIN_PARK);

        // Taking the batch clears the deadline again.
        core.build_batch(&ctx(), 0, 0).unwrap();
        assert_eq!(core.park_hint(a.qpc + FREQ), Duration::from_millis(25));

        // A wider window parks wider: 40ms in, 10ms of a 50ms window left.
        let mut wide = ShipperCore::new("s-1".into(), a, 1_000, 50);
        assert_eq!(wide.park_hint(a.qpc), Duration::from_millis(50));
        wide.push(ev(a.qpc, 1));
        assert_eq!(
            wide.park_hint(a.qpc + FREQ * 40 / 1000),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn the_idle_descent_takes_two_stages() {
        let window = max_park(25);
        // First empty-ring park after activity: only the batch window, so a
        // wake lost to the RingWaker race costs at most one window.
        assert_eq!(next_park_timeout(false, true, window), window);
        // A probe that timed out with the ring still empty: confirmed idle.
        assert_eq!(next_park_timeout(true, true, window), IDLE_PARK);
        // Anything in the ring resets the descent, whatever the probe said.
        assert_eq!(next_park_timeout(true, false, window), window);
        assert_eq!(next_park_timeout(false, false, window), window);
        // The probe is always the caller's window, not a fixed 25ms.
        let wide = max_park(50);
        assert_eq!(next_park_timeout(false, true, wide), wide);
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
        assert_eq!(
            l.allow("udp", t0 + WARN_INTERVAL + Duration::from_secs(1)),
            None
        );
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
        #[cfg(feature = "observability")]
        {
            let snap = stats.snapshot();
            assert_eq!(snap.ship_latency_first.total(), 3);
            assert_eq!(snap.ship_latency_last.total(), 3);
        }
    }

    #[test]
    fn the_session_re_send_reaches_the_viz_and_nothing_else() {
        let udp = RecordingSink::new("udp");
        let jsonl = RecordingSink::new("jsonl");
        let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(udp.clone()), Box::new(jsonl.clone())];
        let stats = Stats::default();
        let mut enc = EnvelopeEncoder::new();
        let mut limiter = WarnLimiter::new();
        let env = Envelope::Session(SessionConfig {
            session_id: "s-1".into(),
            mouse_cpi: 1600.0,
            ..Default::default()
        });

        // The startup delivery goes everywhere...
        encode_and_deliver(&mut enc, &mut sinks, &stats, &mut limiter, &env);
        assert_eq!((udp.count(), jsonl.count()), (1, 1));

        // ...every repeat after it goes to the live viz alone, so the
        // recording keeps exactly one session line.
        for _ in 0..3 {
            send_live(&mut enc, &mut sinks, &stats, &mut limiter, &env, "session");
        }
        assert_eq!(udp.count(), 4);
        assert_eq!(jsonl.count(), 1, "the recording gained no session lines");
        assert!(matches!(
            udp.received.lock().unwrap().last(),
            Some(Envelope::Session(_))
        ));
        assert_eq!(stats.snapshot().udp_errors, 0);
    }

    #[test]
    fn a_failing_viz_re_send_is_counted_but_rate_limited() {
        let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(FailingSink::new("udp"))];
        let stats = Stats::default();
        let mut enc = EnvelopeEncoder::new();
        let mut limiter = WarnLimiter::new();
        let env = Envelope::Session(SessionConfig::default());
        for _ in 0..4 {
            send_live(&mut enc, &mut sinks, &stats, &mut limiter, &env, "session");
        }
        assert_eq!(stats.snapshot().udp_errors, 4);
        assert_eq!(limiter.allow("udp", Instant::now()), None);
    }

    #[test]
    fn no_heartbeat_while_batches_flow() {
        let t0 = Instant::now();
        let mut gate = HeartbeatGate::new(0, t0);
        // Batches every 25ms for two seconds: the viz is hearing from us, so
        // there is nothing for a heartbeat to add.
        let mut batches = 0u64;
        for step in 1..=80u64 {
            batches += 1;
            assert!(
                !gate.due(batches, t0 + Duration::from_millis(25 * step)),
                "a heartbeat rode along with batch {batches}"
            );
        }
    }

    #[test]
    fn the_first_heartbeat_comes_one_interval_after_the_last_batch() {
        let t0 = Instant::now();
        let mut gate = HeartbeatGate::new(0, t0);
        // Last batch at 500ms, polled by the loop right after.
        assert!(!gate.due(1, t0 + Duration::from_millis(500)));
        // The loop keeps turning over; nothing is due until a full interval
        // of silence has passed, measured from that batch and not from the
        // start of the session.
        assert!(!gate.due(1, t0 + Duration::from_millis(525)));
        assert!(!gate.due(1, t0 + Duration::from_millis(1_499)));
        assert!(gate.due(1, t0 + Duration::from_millis(1_500)));
        // ...then once per interval, no faster however often it is polled.
        for ms in [1_600, 2_000, 2_400] {
            assert!(!gate.due(1, t0 + Duration::from_millis(ms)));
        }
        assert!(gate.due(1, t0 + Duration::from_millis(2_500)));
    }

    #[test]
    fn an_agent_that_never_sees_an_event_still_says_hello() {
        // The case the whole thing exists for: capture is up, the hand has
        // not moved once, and the viz must not read that as a dead agent.
        let t0 = Instant::now();
        let mut gate = HeartbeatGate::new(0, t0);
        assert!(!gate.due(0, t0 + Duration::from_millis(999)));
        assert!(gate.due(0, t0 + HEARTBEAT_INTERVAL));
        assert!(gate.due(0, t0 + HEARTBEAT_INTERVAL * 2));
    }

    #[test]
    fn a_resumed_feed_stops_the_heartbeat_again() {
        let t0 = Instant::now();
        let mut gate = HeartbeatGate::with_interval(Duration::from_secs(1), 0, t0);
        assert!(gate.due(0, t0 + Duration::from_secs(1)));
        assert!(gate.due(0, t0 + Duration::from_secs(2)));
        // The hand moves: the batch counter advances and the clock restarts.
        assert!(!gate.due(1, t0 + Duration::from_millis(2_100)));
        assert!(!gate.due(2, t0 + Duration::from_millis(2_900)));
        assert!(!gate.due(2, t0 + Duration::from_millis(3_800)));
        assert!(gate.due(2, t0 + Duration::from_millis(3_900)));
    }

    /// A heartbeat is liveness for the live viz and nothing else: it must not
    /// reach the recording or Kafka, and it must carry neither a sequence
    /// number nor an anchor timestamp (the bridge reads both off every
    /// datagram, for seq gaps and for latency).
    #[test]
    fn the_heartbeat_reaches_the_viz_alone_and_stays_out_of_the_stats() {
        let a = anchor();
        let udp = RecordingSink::new("udp");
        let jsonl = RecordingSink::new("jsonl");
        let kafka = RecordingSink::new("kafka");
        let mut sinks: Vec<Box<dyn Sink>> = vec![
            Box::new(udp.clone()),
            Box::new(jsonl.clone()),
            Box::new(kafka.clone()),
        ];
        let stats = Stats::default();
        let mut enc = EnvelopeEncoder::new();
        let mut limiter = WarnLimiter::new();

        let core = ShipperCore::new("s-1".into(), a, 10, 25);
        let hb = core.build_heartbeat(a.qpc + FREQ * 3);
        assert_eq!(hb.session_id, "s-1");
        assert_eq!(hb.ts_utc_us, a.utc_us + 3_000_000);

        let env = Envelope::Heartbeat(hb);
        send_live(
            &mut enc,
            &mut sinks,
            &stats,
            &mut limiter,
            &env,
            "heartbeat",
        );
        assert_eq!(udp.count(), 1);
        assert_eq!(jsonl.count(), 0, "a recording gains no heartbeat lines");
        assert_eq!(kafka.count(), 0, "the broker gains no heartbeat messages");

        let payload = udp.payloads.lock().unwrap()[0].clone();
        assert!(payload.starts_with(r#"{"type":"heartbeat","#));
        assert!(!payload.contains("seq_no"));
        assert!(!payload.contains("ts_anchor_us"));
        // It is not a batch: nothing about the batch accounting moves.
        assert_eq!(stats.snapshot().batches, 0);
        assert_eq!(stats.snapshot().udp_errors, 0);
    }

    #[test]
    fn warn_limiters_can_run_at_a_different_cadence() {
        let mut l = WarnLimiter::with_interval(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(l.allow("kafka", t0), Some(0));
        assert_eq!(
            l.allow("kafka", t0 + WARN_INTERVAL),
            None,
            "still inside 60s"
        );
        assert_eq!(l.allow("kafka", t0 + Duration::from_secs(60)), Some(1));
    }
}
