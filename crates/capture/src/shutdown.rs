//! One shutdown signal shared by the main thread and T3, with a condvar so an
//! idle agent parks instead of polling.
//!
//! The flag is the source of truth (`Ordering::Acquire`/`Release`), the condvar
//! only wakes waiters early. [`Shutdown::notify`] wakes waiters *without*
//! setting the flag, which is how T1 tells the main thread "look at me" when it
//! exits unexpectedly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Shutdown {
    flag: AtomicBool,
    guard: Mutex<()>,
    cv: Condvar,
}

impl Shutdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    /// Request shutdown and wake everyone waiting.
    pub fn set(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify();
    }

    /// Wake waiters without requesting shutdown (used to surface a state change
    /// the waiter should re-check, e.g. the capture thread dying).
    pub fn notify(&self) {
        // Taking the lock ensures a waiter that has just re-checked the flag but
        // not yet parked cannot miss this notification.
        drop(self.guard.lock());
        self.cv.notify_all();
    }

    /// Block until shutdown is requested, someone calls [`Self::notify`], or
    /// `timeout` elapses. Returns the flag's value.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.is_set() {
            return true;
        }
        let Ok(guard) = self.guard.lock() else {
            // A poisoned lock is not worth killing capture over; degrade to a
            // plain sleep so the caller's loop keeps its cadence.
            std::thread::sleep(timeout.min(Duration::from_millis(50)));
            return self.is_set();
        };
        let _unused = self.cv.wait_timeout(guard, timeout);
        self.is_set()
    }

    /// Wait until `deadline`, returning early on shutdown or notification.
    /// Returns the flag's value.
    pub fn wait_until(&self, deadline: Instant) -> bool {
        let now = Instant::now();
        if deadline <= now {
            return self.is_set();
        }
        self.wait_timeout(deadline - now)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn wait_returns_immediately_once_set() {
        let s = Shutdown::new();
        assert!(!s.is_set());
        s.set();
        let start = Instant::now();
        assert!(s.wait_timeout(Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    /// A wait that nobody ends runs to its deadline and reports "not set".
    /// The condvar may wake spuriously (an early return is legitimate — it
    /// is how `notify` works), so the caller's pattern of re-waiting until
    /// the deadline is what is exercised here.
    #[test]
    fn wait_times_out_when_nothing_happens() {
        let s = Shutdown::new();
        let start = Instant::now();
        let deadline = start + Duration::from_millis(40);
        let mut set = false;
        while Instant::now() < deadline {
            set = s.wait_until(deadline);
        }
        assert!(!set);
        assert!(
            start.elapsed() >= Duration::from_millis(30),
            "returned too early"
        );
    }

    #[test]
    fn a_waiter_wakes_promptly_when_another_thread_sets_it() {
        let s = Arc::new(Shutdown::new());
        let setter = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                s.set();
            })
        };
        let start = Instant::now();
        assert!(s.wait_timeout(Duration::from_secs(30)));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        setter.join().unwrap();
    }

    #[test]
    fn notify_wakes_without_requesting_shutdown() {
        let s = Arc::new(Shutdown::new());
        let notifier = {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                s.notify();
            })
        };
        let start = Instant::now();
        // Returns false: woken, but shutdown was never requested.
        assert!(!s.wait_timeout(Duration::from_secs(30)));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert!(!s.is_set());
        notifier.join().unwrap();
    }

    #[test]
    fn wait_until_a_past_deadline_does_not_block() {
        let s = Shutdown::new();
        let start = Instant::now();
        assert!(!s.wait_until(Instant::now() - Duration::from_secs(1)));
        assert!(start.elapsed() < Duration::from_millis(100));
    }
}
