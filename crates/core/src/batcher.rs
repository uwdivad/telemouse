use crate::event::RawEvent;

/// Accumulates drained ring-buffer events into flush-ready batches.
///
/// Pure logic, driven entirely by caller-supplied QPC timestamps, so the 25ms
/// window / max-size policy is unit-testable without a clock. The shipping
/// thread owns one of these; the capture hot path never touches it.
#[derive(Debug)]
pub struct Batcher {
    max_events: usize,
    window_ticks: u64,
    events: Vec<RawEvent>,
    first_qpc: Option<u64>,
}

impl Batcher {
    pub fn new(max_events: usize, window_ticks: u64) -> Self {
        assert!(max_events > 0);
        Self {
            max_events,
            window_ticks,
            events: Vec::with_capacity(max_events),
            first_qpc: None,
        }
    }

    /// Convenience: window given in milliseconds at a known QPC frequency.
    pub fn with_window_ms(max_events: usize, window_ms: u64, qpc_freq: u64) -> Self {
        Self::new(
            max_events,
            (window_ms as u128 * qpc_freq as u128 / 1000) as u64,
        )
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

    /// True once the batch is full or the window has elapsed since its first
    /// event. An empty batcher never wants a flush.
    pub fn should_flush(&self, now_qpc: u64) -> bool {
        if self.events.is_empty() {
            return false;
        }
        if self.events.len() >= self.max_events {
            return true;
        }
        match self.first_qpc {
            Some(first) => now_qpc.saturating_sub(first) >= self.window_ticks,
            None => false,
        }
    }

    /// Take the accumulated events, resetting for the next batch.
    pub fn take(&mut self) -> Vec<RawEvent> {
        self.first_qpc = None;
        std::mem::replace(&mut self.events, Vec::with_capacity(self.max_events))
    }

    /// The accumulated events, borrowed. Together with [`Self::reset`] this is
    /// the zero-realloc flush path: serialize a [`crate::batch::BatchView`]
    /// over this slice, then `reset()` — the `Vec` and its capacity are never
    /// surrendered, unlike [`Self::take`].
    pub fn events(&self) -> &[RawEvent] {
        &self.events
    }

    /// QPC of the current batch's first event, if any.
    pub fn first_qpc(&self) -> Option<u64> {
        self.first_qpc
    }

    /// Clear the events and per-batch state for the next batch, keeping the
    /// allocated capacity.
    pub fn reset(&mut self) {
        self.events.clear();
        self.first_qpc = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts_qpc: u64) -> RawEvent {
        RawEvent { ts_qpc, dx: 1, ..Default::default() }
    }

    #[test]
    fn empty_never_flushes() {
        let b = Batcher::new(10, 100);
        assert!(!b.should_flush(u64::MAX));
    }

    #[test]
    fn flushes_on_window_elapsed() {
        let mut b = Batcher::new(1000, 250); // window = 250 ticks
        b.push(ev(1000));
        b.push(ev(1100));
        assert!(!b.should_flush(1249));
        assert!(b.should_flush(1250));
    }

    #[test]
    fn flushes_on_max_events_regardless_of_time() {
        let mut b = Batcher::new(3, u64::MAX);
        b.push(ev(1));
        b.push(ev(2));
        assert!(!b.should_flush(2));
        b.push(ev(3));
        assert!(b.should_flush(3));
    }

    #[test]
    fn take_resets_window_and_contents() {
        let mut b = Batcher::new(100, 250);
        b.push(ev(1000));
        assert!(b.should_flush(2000));
        let taken = b.take();
        assert_eq!(taken.len(), 1);
        assert!(b.is_empty());
        assert!(!b.should_flush(u64::MAX));
        // Next batch's window starts from its own first event.
        b.push(ev(5000));
        assert!(!b.should_flush(5249));
        assert!(b.should_flush(5250));
    }

    #[test]
    fn reset_keeps_capacity_and_clears_batch_state() {
        let mut b = Batcher::new(100, 250);
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
        // Next batch's window starts from its own first event, same as take().
        b.push(ev(5000));
        assert!(!b.should_flush(5249));
        assert!(b.should_flush(5250));
    }

    #[test]
    fn with_window_ms_converts_at_freq() {
        // 25ms at 10MHz = 250_000 ticks.
        let mut b = Batcher::with_window_ms(1000, 25, 10_000_000);
        b.push(ev(0));
        assert!(!b.should_flush(249_999));
        assert!(b.should_flush(250_000));
    }
}
