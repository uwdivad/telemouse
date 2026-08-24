//! The shared context snapshot published by T3 and read by T2 when it
//! assembles a batch.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSnapshot {
    /// Foreground process name, lowercased, no path (e.g. `cs2.exe`).
    pub game: Option<String>,
    pub pointer_locked: bool,
    pub screen_w: u32,
    pub screen_h: u32,
    pub cursor_x: i32,
    pub cursor_y: i32,
}

impl ContextSnapshot {
    /// Cursor position for a batch: only meaningful in desktop mode, per the
    /// `Batch` doc comment.
    pub fn batch_cursor(&self) -> (Option<i32>, Option<i32>) {
        if self.pointer_locked {
            (None, None)
        } else {
            (Some(self.cursor_x), Some(self.cursor_y))
        }
    }
}

/// Single-writer (T3) / many-reader (T2) holder.
///
/// The snapshot is behind an `Arc` so a reader clones a pointer, not a `String`
/// — T2 reads this once per flush (~40×/s) and must not pay for the game name's
/// allocation each time. The `Mutex` only guards the pointer swap.
#[derive(Debug, Default)]
pub struct SharedContext {
    inner: Mutex<std::sync::Arc<ContextSnapshot>>,
    /// Set by T1's window procedure on `WM_DISPLAYCHANGE`; T3 consumes it on
    /// its next tick and refreshes the screen metrics. An atomic keeps the
    /// window procedure lock-free.
    display_changed: AtomicBool,
}

impl SharedContext {
    pub fn new(initial: ContextSnapshot) -> Self {
        Self {
            inner: Mutex::new(std::sync::Arc::new(initial)),
            display_changed: AtomicBool::new(false),
        }
    }

    pub fn get(&self) -> std::sync::Arc<ContextSnapshot> {
        match self.inner.lock() {
            Ok(g) => std::sync::Arc::clone(&g),
            // A poisoned context is not worth killing capture over.
            Err(p) => std::sync::Arc::clone(&p.into_inner()),
        }
    }

    pub fn set(&self, snapshot: ContextSnapshot) {
        let next = std::sync::Arc::new(snapshot);
        match self.inner.lock() {
            Ok(mut g) => *g = next,
            Err(p) => *p.into_inner() = next,
        }
    }

    /// Called from T1's window procedure: the desktop geometry changed.
    pub fn mark_display_changed(&self) {
        self.display_changed.store(true, Ordering::Relaxed);
    }

    /// Consume the display-change flag. True at most once per change.
    pub fn take_display_changed(&self) -> bool {
        self.display_changed.swap(false, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_omitted_while_locked() {
        let mut c = ContextSnapshot {
            cursor_x: 10,
            cursor_y: 20,
            ..Default::default()
        };
        assert_eq!(c.batch_cursor(), (Some(10), Some(20)));
        c.pointer_locked = true;
        assert_eq!(c.batch_cursor(), (None, None));
    }

    #[test]
    fn shared_context_round_trips() {
        let shared = SharedContext::default();
        assert_eq!(*shared.get(), ContextSnapshot::default());
        let snap = ContextSnapshot {
            game: Some("cs2.exe".into()),
            pointer_locked: true,
            screen_w: 2560,
            screen_h: 1440,
            cursor_x: 1,
            cursor_y: 2,
        };
        shared.set(snap.clone());
        assert_eq!(*shared.get(), snap);
    }

    #[test]
    fn readers_share_one_allocation_per_publish() {
        let shared = SharedContext::new(ContextSnapshot {
            game: Some("cs2.exe".into()),
            ..Default::default()
        });
        let a = shared.get();
        let b = shared.get();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        shared.set(ContextSnapshot::default());
        assert!(!std::sync::Arc::ptr_eq(&a, &shared.get()));
    }

    #[test]
    fn display_change_is_a_one_shot_flag() {
        let shared = SharedContext::default();
        assert!(!shared.take_display_changed());
        shared.mark_display_changed();
        shared.mark_display_changed();
        assert!(shared.take_display_changed());
        assert!(!shared.take_display_changed());
    }
}
