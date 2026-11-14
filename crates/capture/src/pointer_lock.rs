//! Pointer-lock heuristic.
//!
//! When a game takes raw input it typically hides and *pins* the OS cursor, so
//! the desktop cursor position stops moving while HID deltas keep flowing. That
//! combination is the signal: cursor frozen **and** events arriving ⇒ locked.
//!
//! The signal is noisy on a desktop. A cursor is also "frozen while events
//! arrive" when the wheel is scrolled, when a resting hand produces a count
//! or two that the pointer-speed setting rounds away, and when it is pushed
//! against a screen edge. Those runs are short, so the detector asks for
//! [`DEFAULT_CONFIRM_SAMPLES`] consecutive ticks of evidence before it says
//! locked. The two ways out of a lock are not treated alike:
//!
//! * **The cursor moved** — proof, acted on at once. A pinned cursor does not
//!   move, so there is nothing to wait for.
//! * **Nothing arrived** — no evidence either way. It discards a run that has
//!   not been confirmed yet, but an established lock is kept: a player holding
//!   an angle is still in the game, and dropping the lock on every pause would
//!   leave the start of the next flick flagged as desktop motion while the
//!   confirmation ran again.
//!
//! Pure logic driven by caller-supplied samples so it is unit-testable without
//! a mouse or a Win32 message loop; the context thread just feeds it.

/// Number of consecutive frozen-but-active samples before declaring a lock.
/// At the 250ms context tick that is three quarters of a second of evidence.
/// It was 2 until the 2026-11 performance pass, where two samples turned out
/// to be reached by ordinary desktop use (70 times in 20 minutes of browsing),
/// each time for a single tick; one more sample is what removes those.
pub const DEFAULT_CONFIRM_SAMPLES: u32 = 3;

/// What one sample says about the cursor, before any history is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The desktop cursor is somewhere else than last time.
    Moved,
    /// The cursor did not move although raw events arrived.
    FrozenActive,
    /// The cursor did not move and neither did anything else.
    Idle,
}

impl Evidence {
    /// Classify a sample against the previous cursor position.
    pub fn of(previous: (i32, i32), cursor: (i32, i32), events_delta: u64) -> Self {
        if previous != cursor {
            Self::Moved
        } else if events_delta > 0 {
            Self::FrozenActive
        } else {
            Self::Idle
        }
    }
}

/// The detector's whole decision: the verdict so far, the unbroken run of
/// [`Evidence::FrozenActive`] samples behind it, and one more sample in; the
/// new `(locked, frozen_run)` out. Rules in the module docs.
pub fn decide(
    locked: bool,
    frozen_run: u32,
    confirm_samples: u32,
    evidence: Evidence,
) -> (bool, u32) {
    match evidence {
        Evidence::Moved => (false, 0),
        Evidence::FrozenActive => {
            let run = frozen_run.saturating_add(1);
            (locked || run >= confirm_samples.max(1), run)
        }
        // Keeps a lock, never builds towards one.
        Evidence::Idle => (locked, 0),
    }
}

#[derive(Debug, Clone)]
pub struct PointerLockDetector {
    confirm_samples: u32,
    last_cursor: Option<(i32, i32)>,
    frozen_run: u32,
    locked: bool,
}

impl Default for PointerLockDetector {
    fn default() -> Self {
        Self::new(DEFAULT_CONFIRM_SAMPLES)
    }
}

impl PointerLockDetector {
    pub fn new(confirm_samples: u32) -> Self {
        Self {
            confirm_samples: confirm_samples.max(1),
            last_cursor: None,
            frozen_run: 0,
            locked: false,
        }
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Feed one sample: the current cursor position and how many raw events
    /// were captured since the previous sample. Returns the new lock state.
    pub fn observe(&mut self, cursor: (i32, i32), events_delta: u64) -> bool {
        let previous = self.last_cursor.replace(cursor);
        let Some(prev) = previous else {
            // First sample: nothing to compare against, so it is not evidence.
            self.frozen_run = 0;
            self.locked = false;
            return false;
        };

        (self.locked, self.frozen_run) = decide(
            self.locked,
            self.frozen_run,
            self.confirm_samples,
            Evidence::of(prev, cursor, events_delta),
        );
        self.locked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_when_cursor_frozen_while_deltas_flow() {
        let mut d = PointerLockDetector::new(2);
        assert!(!d.observe((100, 100), 40)); // first sample: no history yet
        assert!(!d.observe((100, 100), 40)); // one frozen+active sample
        assert!(d.observe((100, 100), 40)); // confirmed
        assert!(d.locked());
    }

    #[test]
    fn unlocks_as_soon_as_the_cursor_moves() {
        let mut d = PointerLockDetector::new(2);
        for _ in 0..5 {
            d.observe((100, 100), 40);
        }
        assert!(d.locked());
        assert!(!d.observe((101, 100), 40));
        assert!(!d.locked());
    }

    #[test]
    fn never_locks_without_deltas() {
        let mut d = PointerLockDetector::new(2);
        for _ in 0..10 {
            assert!(!d.observe((100, 100), 0));
        }
        assert!(!d.locked());
    }

    /// A pause is not an unlock: the player is holding an angle. The flick
    /// that follows is locked from its first sample, and the cursor moving
    /// still ends the lock at once.
    #[test]
    fn an_idle_sample_keeps_an_existing_lock() {
        let mut d = PointerLockDetector::new(1);
        d.observe((5, 5), 1);
        assert!(d.observe((5, 5), 1));
        for _ in 0..40 {
            assert!(d.observe((5, 5), 0));
        }
        assert!(d.observe((5, 5), 30));
        assert!(!d.observe((6, 5), 30));
        assert!(!d.observe((6, 5), 0));
    }

    /// ...but idle never counts towards one, and breaks a run in progress.
    #[test]
    fn an_idle_sample_discards_an_unconfirmed_run() {
        let mut d = PointerLockDetector::default();
        d.observe((5, 5), 1);
        for _ in 0..20 {
            assert!(!d.observe((5, 5), 4));
            assert!(!d.observe((5, 5), 4));
            assert!(!d.observe((5, 5), 0));
        }
        assert!(!d.locked());
    }

    /// C2: what the two-sample rule reported as a one-tick lock (frozen for
    /// two ticks, then the cursor moves) is no transition at all now.
    #[test]
    fn a_single_tick_positive_is_not_a_lock() {
        assert_eq!(DEFAULT_CONFIRM_SAMPLES, 3);
        let mut old_rule = PointerLockDetector::new(2);
        let mut d = PointerLockDetector::default();
        let mut old_locks = 0;
        let mut x = 0;
        old_rule.observe((x, 0), 250);
        d.observe((x, 0), 250);
        for _ in 0..50 {
            // Two frozen ticks with events (a scroll, a resting hand)...
            for _ in 0..2 {
                old_locks += u32::from(old_rule.observe((x, 0), 250));
                assert!(!d.observe((x, 0), 250));
            }
            // ...then ordinary motion.
            x += 7;
            assert!(!old_rule.observe((x, 0), 250));
            assert!(!d.observe((x, 0), 250));
        }
        assert_eq!(old_locks, 50, "the trace is the old false positive");
    }

    #[test]
    fn the_default_locks_on_the_third_frozen_sample_and_stays() {
        let mut d = PointerLockDetector::default();
        assert!(!d.observe((960, 540), 250)); // no history
        assert!(!d.observe((960, 540), 250));
        assert!(!d.observe((960, 540), 250));
        for _ in 0..100 {
            assert!(d.observe((960, 540), 250));
        }
    }

    #[test]
    fn the_decision_table() {
        use Evidence::{FrozenActive, Idle, Moved};
        assert_eq!(Evidence::of((1, 2), (1, 3), 0), Moved);
        assert_eq!(Evidence::of((1, 2), (1, 3), 9), Moved);
        assert_eq!(Evidence::of((1, 2), (1, 2), 9), FrozenActive);
        assert_eq!(Evidence::of((1, 2), (1, 2), 0), Idle);

        // Not locked: only an unbroken run reaches the threshold.
        assert_eq!(decide(false, 0, 3, FrozenActive), (false, 1));
        assert_eq!(decide(false, 1, 3, FrozenActive), (false, 2));
        assert_eq!(decide(false, 2, 3, FrozenActive), (true, 3));
        assert_eq!(decide(false, 2, 3, Idle), (false, 0));
        assert_eq!(decide(false, 2, 3, Moved), (false, 0));
        // Locked: only the cursor moving ends it.
        assert_eq!(decide(true, 3, 3, FrozenActive), (true, 4));
        assert_eq!(decide(true, 3, 3, Idle), (true, 0));
        assert_eq!(decide(true, 0, 3, FrozenActive), (true, 1));
        assert_eq!(decide(true, 7, 3, Moved), (false, 0));
        // Degenerate inputs neither panic nor lock on nothing.
        assert_eq!(decide(false, 0, 0, FrozenActive), (true, 1));
        assert_eq!(decide(false, 0, 0, Idle), (false, 0));
        assert_eq!(decide(true, u32::MAX, 3, FrozenActive), (true, u32::MAX));
    }

    #[test]
    fn confirm_samples_of_one_locks_on_first_evidence() {
        let mut d = PointerLockDetector::new(1);
        assert!(!d.observe((0, 0), 10)); // no previous cursor to compare
        assert!(d.observe((0, 0), 10));
    }
}
