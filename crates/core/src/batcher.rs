use crate::event::RawEvent;

/// Accumulates drained ring-buffer events into flush-ready batches.
///
/// Pure logic, driven entirely by caller-supplied QPC timestamps, so the 25ms
/// window / max-size policy is unit-testable without a clock. The shipping
/// thread owns one of these; the capture hot path never touches it.
///
/// ## Two clocks, and why they must not be mixed
///
/// A batch carries two different kinds of QPC value:
///
/// * [`Self::first_qpc`] — the stamp of the batch's first *event*, which is
///   what a consumer reads as `ts_anchor_us`. It comes from the raw-input
///   drain, and that drain is deliberately coalesced: T1 reads a whole
///   period's worth of reports at once and spreads their stamps back over the
///   period since the previous read. So an event's stamp is **back-dated** by
///   up to one coalescing period (`coalesce_ms + 1`, ~9 ms at the defaults)
///   relative to the moment the shipping thread first saw it.
/// * [`Self::deadline`] — a wall-clock QPC: the instant the open window
///   closes. It is set by the owner from *its own* reading of the clock
///   ([`Self::open_window`]), never from an event stamp.
///
/// Until 2026-09-21 there was only `first_qpc`: the window was "flush once
/// `window_ticks` have elapsed since the first event", tested against the
/// shipping thread's wall clock. Mixing the two clocks made every window
/// short by exactly the back-dating, so at `window_ms = 25` a live stream
/// shipped a batch roughly every 19 ms — about 52/s instead of the documented
/// 40/s (measured in `docs/PERFORMANCE-2026-09-20.md`), and every consumer
/// downstream paid the extra 30%.
///
/// The deadline now runs on a **fixed grid**: each window closes one
/// `window_ticks` after the previous one did, for as long as the stream stays
/// live. That makes the cadence exactly `1000 / window_ms` batches per second
/// no matter where inside a window a drain happens to land, at the price of
/// up to one coalescing period of extra wait for the *oldest* event in a
/// batch (worst case `window_ms + coalesce_ms + 1`; it was exactly
/// `window_ms`, because the old window was anchored on that very event).
#[derive(Debug)]
pub struct Batcher {
    max_events: usize,
    window_ticks: u64,
    events: Vec<RawEvent>,
    first_qpc: Option<u64>,
    /// Wall-clock QPC at which the open window closes. `None` means no window
    /// is open at all — the stream is idle and nothing is on a timer.
    deadline: Option<u64>,
}

impl Batcher {
    /// `window_ticks` is clamped to at least one tick: a zero-length window
    /// would expire the instant it opened and spin the owner's loop. The
    /// config layer already refuses `window_ms = 0`; this is the belt.
    pub fn new(max_events: usize, window_ticks: u64) -> Self {
        assert!(max_events > 0);
        Self {
            max_events,
            window_ticks: window_ticks.max(1),
            events: Vec::with_capacity(max_events),
            first_qpc: None,
            deadline: None,
        }
    }

    /// Convenience: window given in milliseconds at a known QPC frequency.
    pub fn with_window_ms(max_events: usize, window_ms: u64, qpc_freq: u64) -> Self {
        Self::new(
            max_events,
            (window_ms as u128 * qpc_freq as u128 / 1000) as u64,
        )
    }

    /// The configured window, in QPC ticks.
    pub fn window_ticks(&self) -> u64 {
        self.window_ticks
    }

    pub fn push(&mut self, ev: RawEvent) {
        if self.first_qpc.is_none() {
            self.first_qpc = Some(ev.ts_qpc);
        }
        self.events.push(ev);
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The batch has reached `max_events` and must go out now, whatever the
    /// clock says. This is the only flush condition the drain loop may test
    /// per pushed event: it is what keeps a batch inside the UDP datagram
    /// budget under a burst, and unlike the window it needs no clock reading.
    pub fn is_full(&self) -> bool {
        self.events.len() >= self.max_events
    }

    /// Open the next window.
    ///
    /// With a window already open the deadline advances on the **grid**
    /// (`deadline + window_ticks`). That is what keeps a live stream at
    /// exactly one batch per window even though the drain that opens a batch
    /// lands at an arbitrary offset inside it.
    ///
    /// With no window open — the stream was idle — the window is anchored on
    /// the batch's **first event** instead, falling back to `now_qpc` when
    /// there is none yet. That is safe precisely here and nowhere else: after
    /// a pause the producer is blocked on its input queue, so the first
    /// report of the burst has an *observed* arrival and its stamp is not
    /// back-dated. Anchoring on it keeps the idle → first-batch latency at
    /// exactly one window, which is what the live view's responsiveness is
    /// judged on.
    ///
    /// A loop that fell a whole window or more behind (a scheduling stall, a
    /// `DRAIN_BUDGET` backlog) restarts the grid at `now_qpc` rather than
    /// working through a queue of already-expired windows.
    pub fn open_window(&mut self, now_qpc: u64) {
        let next = match self.deadline {
            Some(d) => d.saturating_add(self.window_ticks),
            None => self
                .first_qpc
                .unwrap_or(now_qpc)
                .min(now_qpc)
                .saturating_add(self.window_ticks),
        };
        self.deadline = Some(if next <= now_qpc {
            now_qpc.saturating_add(self.window_ticks)
        } else {
            next
        });
    }

    /// No window open: nothing is due, so the owner may park on its producer
    /// instead of on a timer. Called when a whole window passed with no
    /// events at all.
    pub fn close_window(&mut self) {
        self.deadline = None;
    }

    /// Wall-clock QPC the open window closes at, if one is open.
    pub fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    pub fn is_window_open(&self) -> bool {
        self.deadline.is_some()
    }

    /// True once the open window's deadline has passed. Independent of
    /// whether anything accumulated: a window that closes empty is how the
    /// owner learns the stream went quiet.
    pub fn window_expired(&self, now_qpc: u64) -> bool {
        self.deadline.is_some_and(|d| now_qpc >= d)
    }

    /// True once the batch is full or the open window has closed. An empty
    /// batcher never wants a flush. `now_qpc` is a **wall-clock** reading,
    /// not an event stamp (see the type docs).
    pub fn should_flush(&self, now_qpc: u64) -> bool {
        if self.events.is_empty() {
            return false;
        }
        self.is_full() || self.window_expired(now_qpc)
    }

    /// The accumulated events, borrowed. Together with [`Self::reset`] this is
    /// the flush path, and the only one: serialize a
    /// [`crate::batch::BatchView`] over this slice, then `reset()` — the
    /// `Vec` and its capacity are never surrendered, so a steady-state flush
    /// allocates nothing.
    pub fn events(&self) -> &[RawEvent] {
        &self.events
    }

    /// QPC of the current batch's first event, if any.
    pub fn first_qpc(&self) -> Option<u64> {
        self.first_qpc
    }

    /// Clear the events and per-batch state for the next batch, keeping the
    /// allocated capacity.
    ///
    /// Deliberately leaves the window alone. A flush forced by
    /// [`Self::is_full`] happens partway through a window and must not move
    /// the grid: the rest of that window still belongs to it, and only
    /// [`Self::open_window`] advances the deadline.
    pub fn reset(&mut self) {
        self.events.clear();
        self.first_qpc = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts_qpc: u64) -> RawEvent {
        RawEvent {
            ts_qpc,
            dx: 1,
            ..Default::default()
        }
    }

    #[test]
    fn empty_never_flushes() {
        let mut b = Batcher::new(10, 100);
        b.open_window(0);
        assert!(!b.should_flush(u64::MAX));
    }

    #[test]
    fn no_window_open_means_nothing_is_ever_due() {
        let mut b = Batcher::new(1000, 250);
        b.push(ev(1000));
        // A closed window has no deadline: only the size rule can fire, and
        // an event stamp can never be mistaken for one.
        assert!(!b.is_window_open());
        assert!(!b.window_expired(u64::MAX));
        assert!(!b.should_flush(u64::MAX));
    }

    #[test]
    fn flushes_when_the_open_window_closes() {
        let mut b = Batcher::new(1000, 250); // window = 250 ticks
        b.open_window(1_000);
        assert_eq!(b.deadline(), Some(1_250));
        // Events stamped well before the window opened (raw input back-dates
        // them) do not shorten it: the deadline is wall clock only.
        b.push(ev(900));
        b.push(ev(1_100));
        assert!(!b.should_flush(1_249));
        assert!(b.should_flush(1_250));
    }

    #[test]
    fn flushes_on_max_events_regardless_of_time() {
        let mut b = Batcher::new(3, u64::MAX);
        b.push(ev(1));
        b.push(ev(2));
        assert!(!b.is_full());
        assert!(!b.should_flush(2));
        b.push(ev(3));
        assert!(b.is_full());
        assert!(b.should_flush(3));
    }

    #[test]
    fn the_window_runs_on_a_grid_while_the_stream_is_live() {
        let mut b = Batcher::new(1000, 250);
        // Idle -> live: the first window runs from now.
        b.open_window(1_000);
        assert_eq!(b.deadline(), Some(1_250));
        // Every window after it closes exactly one window after the last, so
        // the cadence does not drift with when the loop happened to wake.
        b.open_window(1_260); // woken 10 ticks late
        assert_eq!(b.deadline(), Some(1_500));
        b.open_window(1_505);
        assert_eq!(b.deadline(), Some(1_750));
    }

    #[test]
    fn the_first_window_of_a_burst_is_anchored_on_its_first_event() {
        let mut b = Batcher::new(1000, 250);
        // After a pause the producer was blocked on its queue, so this stamp
        // is an observed arrival: the window runs one window from *it*, not
        // from the moment the loop got round to looking.
        b.push(ev(1_000));
        b.open_window(1_030);
        assert_eq!(b.deadline(), Some(1_250));
        // The grid takes over from there — no more event stamps.
        b.reset();
        b.push(ev(1_240));
        b.open_window(1_255);
        assert_eq!(b.deadline(), Some(1_500));
    }

    #[test]
    fn an_anchor_already_older_than_a_window_restarts_from_now() {
        let mut b = Batcher::new(1000, 250);
        // The loop was starved: anchoring on this stamp would open a window
        // that is already over and flush every pass.
        b.push(ev(1_000));
        b.open_window(9_000);
        assert_eq!(b.deadline(), Some(9_250));
    }

    #[test]
    fn a_stalled_loop_restarts_the_grid_instead_of_catching_up() {
        let mut b = Batcher::new(1000, 250);
        b.open_window(1_000);
        // The loop lost 10 windows to a scheduling stall. Advancing the grid
        // would queue up ten already-expired deadlines and flush ten times in
        // a row; restart from now instead.
        b.open_window(4_000);
        assert_eq!(b.deadline(), Some(4_250));
    }

    #[test]
    fn closing_the_window_stands_the_owner_down() {
        let mut b = Batcher::new(1000, 250);
        b.open_window(1_000);
        b.close_window();
        assert!(!b.is_window_open());
        assert_eq!(b.deadline(), None);
        assert!(!b.window_expired(u64::MAX));
        // Re-opening after silence runs from the new now, not from the stale
        // grid: the first event after a pause still ships within one window.
        b.open_window(9_000);
        assert_eq!(b.deadline(), Some(9_250));
    }

    #[test]
    fn reset_keeps_capacity_and_the_window() {
        let mut b = Batcher::new(100, 250);
        b.open_window(1_000);
        b.push(ev(1000));
        b.push(ev(1100));
        assert_eq!(b.events().len(), 2);
        assert_eq!(b.first_qpc(), Some(1000));
        let cap = b.events.capacity();

        b.reset();
        assert!(b.is_empty());
        assert_eq!(b.first_qpc(), None);
        assert!(!b.should_flush(u64::MAX));
        assert_eq!(b.events.capacity(), cap, "reset must not shrink or realloc");
        // A size-forced flush mid-window must not move the grid: the rest of
        // this window still belongs to it.
        assert_eq!(b.deadline(), Some(1_250));
        b.push(ev(5000));
        assert!(!b.should_flush(1_249));
        assert!(b.should_flush(1_250));
    }

    #[test]
    fn with_window_ms_converts_at_freq() {
        // 25ms at 10MHz = 250_000 ticks.
        let mut b = Batcher::with_window_ms(1000, 25, 10_000_000);
        assert_eq!(b.window_ticks(), 250_000);
        b.open_window(0);
        b.push(ev(0));
        assert!(!b.should_flush(249_999));
        assert!(b.should_flush(250_000));
    }

    #[test]
    fn a_zero_length_window_is_clamped_so_it_cannot_spin() {
        let mut b = Batcher::with_window_ms(10, 0, 10_000_000);
        assert_eq!(b.window_ticks(), 1);
        b.open_window(100);
        assert_eq!(b.deadline(), Some(101));
    }
}
