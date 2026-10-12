//! The queue bookkeeping the JSONL writer and the Kafka forwarder share.
//!
//! Both sinks put a bounded channel in front of a worker. The shipping thread
//! only ever *tries* to enqueue: a full queue is a counted drop, a closed one
//! is a terminal failure, and every accepted envelope stays `outstanding`
//! until the worker reports it delivered (or not). Shutdown timeouts, worker
//! failures and panics can race each other; whichever reaches
//! [`QueueAccounting::claim_loss`] first moves every outstanding envelope into
//! `abandoned`, exactly once, and a late completion after that changes
//! nothing. One mutex serializes all of it, so the counters are exact.
//!
//! The sinks keep only their transport; this keeps the arithmetic.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use super::SinkKind;
use crate::stats::{QueueCounters, Stats};

/// What a non-blocking send into the worker's channel came back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrySend {
    Sent,
    /// The queue is full: drop and count, never block shipping.
    Full,
    /// The worker is gone.
    Closed,
}

#[derive(Debug, Default)]
struct QueueState {
    /// Terminal: later envelopes are dropped without touching the channel.
    failed: bool,
    /// Accepted envelopes not yet confirmed delivered.
    outstanding: u64,
    /// The one terminal claim has moved `outstanding` into `abandoned`.
    loss_claimed: bool,
    /// The causal error has been counted and logged (exactly once).
    failure_reported: bool,
    /// The queue is overflowing: warn on the edge into it, info on the way out.
    dropping: bool,
}

pub struct QueueAccounting {
    kind: SinkKind,
    capacity: usize,
    stats: Arc<Stats>,
    counters: fn(&Stats) -> QueueCounters<'_>,
    state: Mutex<QueueState>,
}

impl QueueAccounting {
    pub fn new(
        kind: SinkKind,
        capacity: usize,
        stats: Arc<Stats>,
        counters: fn(&Stats) -> QueueCounters<'_>,
    ) -> Self {
        Self {
            kind,
            capacity,
            stats,
            counters,
            state: Mutex::new(QueueState::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn counters(&self) -> QueueCounters<'_> {
        (self.counters)(&self.stats)
    }

    /// The sink has failed for good; later envelopes are dropped.
    pub fn is_failed(&self) -> bool {
        self.lock().failed
    }

    /// Count one envelope refused because the sink already failed.
    pub fn count_dropped(&self) {
        self.counters().dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one sink error (the causal failure is the caller's to log).
    pub fn count_error(&self) {
        self.counters().errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Offer one envelope. `send` is the non-blocking channel send; it runs
    /// under the state lock, after the envelope is already counted as
    /// outstanding, so a worker that consumes it at once (or a terminal claim
    /// that races it) always sees consistent counts. Never fails: overflow and
    /// a closed channel are counted, logged on their edges, and swallowed so
    /// they stay off fan-out's allocating error path.
    pub fn enqueue(&self, send: impl FnOnce() -> TrySend) {
        let mut st = self.lock();
        let c = self.counters();
        if st.failed {
            // The causal error was already logged; this is just a drop.
            c.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        st.outstanding += 1;
        c.queued.fetch_add(1, Ordering::Relaxed);
        match send() {
            TrySend::Sent => {
                if st.dropping {
                    st.dropping = false;
                    tracing::info!(
                        sink = self.kind.as_str(),
                        dropped_total = c.dropped.load(Ordering::Relaxed),
                        "sink queue recovered"
                    );
                }
            }
            TrySend::Full => {
                st.outstanding -= 1;
                c.queued.fetch_sub(1, Ordering::Relaxed);
                let total = c.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if !st.dropping {
                    st.dropping = true;
                    tracing::warn!(
                        sink = self.kind.as_str(),
                        queue_capacity = self.capacity,
                        dropped_total = total,
                        "sink queue full; dropping envelopes until it drains"
                    );
                }
            }
            TrySend::Closed => {
                st.outstanding -= 1;
                c.queued.fetch_sub(1, Ordering::Relaxed);
                c.dropped.fetch_add(1, Ordering::Relaxed);
                let abandoned = self.claim_loss_locked(&mut st);
                if !std::mem::replace(&mut st.failure_reported, true) {
                    c.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        sink = self.kind.as_str(),
                        abandoned,
                        "sink worker channel closed unexpectedly"
                    );
                }
            }
        }
    }

    /// The worker settled `n` accepted envelopes: delivered, or (when
    /// `delivered` is false) lost after being accepted. A no-op once the loss
    /// has been claimed — those envelopes were already counted as abandoned.
    pub fn finish(&self, n: u64, delivered: bool) {
        if n == 0 {
            return;
        }
        // The guard is held until the counters moved, so a racing claim never
        // sees them half-updated.
        let mut st = self.lock();
        if st.loss_claimed {
            return;
        }
        debug_assert!(st.outstanding >= n);
        let n = n.min(st.outstanding);
        st.outstanding -= n;
        let c = self.counters();
        c.queued.fetch_sub(n, Ordering::Relaxed);
        if !delivered {
            c.abandoned.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Mark the sink failed. True on the first transition, so the caller
    /// counts the causal error once. (Kafka's worker-panic paths.)
    #[cfg_attr(not(feature = "kafka"), allow(dead_code))]
    pub fn mark_failed(&self) -> bool {
        !std::mem::replace(&mut self.lock().failed, true)
    }

    /// True the first time it is called: the one place that gets to count and
    /// log the causal failure.
    pub fn mark_failure_reported(&self) -> bool {
        !std::mem::replace(&mut self.lock().failure_reported, true)
    }

    /// Terminal: move every outstanding envelope into `abandoned`, exactly
    /// once however many paths race to it. Returns how many moved.
    pub fn claim_loss(&self) -> u64 {
        let mut st = self.lock();
        self.claim_loss_locked(&mut st)
    }

    fn claim_loss_locked(&self, st: &mut QueueState) -> u64 {
        st.failed = true;
        if std::mem::replace(&mut st.loss_claimed, true) {
            return 0;
        }
        let abandoned = std::mem::take(&mut st.outstanding);
        let c = self.counters();
        c.queued.fetch_sub(abandoned, Ordering::Relaxed);
        c.abandoned.fetch_add(abandoned, Ordering::Relaxed);
        abandoned
    }

    /// Pretend `n` envelopes were accepted, for tests that start mid-flight.
    #[cfg(test)]
    pub fn preload(&self, n: u64) {
        self.lock().outstanding += n;
        self.counters().queued.fetch_add(n, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> (Arc<Stats>, QueueAccounting) {
        let stats = Arc::new(Stats::default());
        let q = QueueAccounting::new(SinkKind::Jsonl, 4, Arc::clone(&stats), Stats::jsonl_queue);
        (stats, q)
    }

    #[test]
    fn a_full_queue_is_a_drop_not_an_outstanding_envelope() {
        let (stats, q) = queue();
        q.enqueue(|| TrySend::Sent);
        q.enqueue(|| TrySend::Full);
        q.enqueue(|| TrySend::Full);
        q.enqueue(|| TrySend::Sent);
        let s = stats.snapshot();
        assert_eq!((s.jsonl_queued, s.jsonl_dropped), (2, 2));
        q.finish(2, true);
        assert_eq!(stats.snapshot().jsonl_queued, 0);
        assert_eq!(stats.snapshot().jsonl_errors, 0);
    }

    #[test]
    fn a_closed_channel_claims_the_loss_and_reports_once() {
        let (stats, q) = queue();
        q.enqueue(|| TrySend::Sent);
        q.enqueue(|| TrySend::Closed);
        let s = stats.snapshot();
        assert_eq!(s.jsonl_queued, 0);
        assert_eq!(s.jsonl_abandoned, 1, "the accepted one is lost");
        assert_eq!(s.jsonl_dropped, 1, "the refused one is a drop");
        assert_eq!(s.jsonl_errors, 1);
        // Failed now: later envelopes are quiet drops.
        q.enqueue(|| unreachable!("a failed sink never touches its channel"));
        assert_eq!(stats.snapshot().jsonl_dropped, 2);
        assert!(!q.mark_failure_reported(), "already reported");
        assert!(!q.mark_failed(), "already failed");
    }

    #[test]
    fn the_terminal_claim_is_exact_once_and_late_completion_is_harmless() {
        let (stats, q) = queue();
        q.preload(3);
        assert_eq!(q.claim_loss(), 3);
        assert_eq!(q.claim_loss(), 0);
        q.finish(1, true);
        q.finish(1, false);
        let s = stats.snapshot();
        assert_eq!((s.jsonl_queued, s.jsonl_abandoned), (0, 3));
    }

    #[test]
    fn an_undelivered_completion_is_abandoned() {
        let (stats, q) = queue();
        q.preload(2);
        q.finish(1, false);
        q.finish(1, true);
        let s = stats.snapshot();
        assert_eq!((s.jsonl_queued, s.jsonl_abandoned), (0, 1));
    }
}
