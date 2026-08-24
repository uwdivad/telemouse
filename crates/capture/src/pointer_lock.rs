//! Pointer-lock heuristic.
//!
//! When a game takes raw input it typically hides and *pins* the OS cursor, so
//! the desktop cursor position stops moving while HID deltas keep flowing. That
//! combination is the signal: cursor frozen **and** events arriving ⇒ locked.
//!
//! Pure logic driven by caller-supplied samples so it is unit-testable without
//! a mouse or a Win32 message loop; the context thread just feeds it.

/// Number of consecutive frozen-but-active samples before declaring a lock.
/// At the 250ms context tick that is half a second of evidence.
pub const DEFAULT_CONFIRM_SAMPLES: u32 = 2;

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

        if prev != cursor {
            // The desktop cursor is tracking the mouse: definitely not locked.
            self.frozen_run = 0;
            self.locked = false;
        } else if events_delta > 0 {
            // Deltas flowed but the cursor did not budge.
            self.frozen_run = self.frozen_run.saturating_add(1);
            if self.frozen_run >= self.confirm_samples {
                self.locked = true;
            }
        } else {
            // Nothing happened at all — an idle desk is not evidence of a lock.
            self.frozen_run = 0;
            self.locked = false;
        }
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

    #[test]
    fn idle_sample_clears_an_existing_lock() {
        let mut d = PointerLockDetector::new(1);
        d.observe((5, 5), 1);
        assert!(d.observe((5, 5), 1));
        assert!(!d.observe((5, 5), 0));
    }

    #[test]
    fn confirm_samples_of_one_locks_on_first_evidence() {
        let mut d = PointerLockDetector::new(1);
        assert!(!d.observe((0, 0), 10)); // no previous cursor to compare
        assert!(d.observe((0, 0), 10));
    }
}
