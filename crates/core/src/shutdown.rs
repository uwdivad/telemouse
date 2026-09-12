//! Console control events, handled the way Windows actually works.
//!
//! `Ctrl-C` and `Ctrl-Break` are polite: the handler returns, the process
//! keeps running, and teardown happens on the main thread. The other three —
//! the console window's close button, logoff, shutdown — are not. Windows
//! terminates the process **as soon as the handler returns**, so a handler
//! that only sets a flag (what the `ctrlc` crate does) loses whatever the
//! sinks had not flushed: the tail of a recording, the queued Kafka batches,
//! the `<session>.meta.json` sidecar.
//!
//! So for those events the handler *blocks* until the process says it is
//! done ([`ShutdownGuard::finished`]) or `max_wait` runs out. The few seconds
//! Windows grants before it kills the process anyway are exactly the budget
//! the capture agent's drains are bounded by.
//!
//! Off Windows nothing is registered and the callback is never invoked; the
//! guard still exists so callers need no `cfg`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Console control events, in the vocabulary of the code that handles them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `Ctrl-C`.
    CtrlC,
    /// `Ctrl-Break` — what `telemouse-ctl` sends for a graceful stop.
    CtrlBreak,
    /// The console window was closed.
    Close,
    /// The user is logging off.
    Logoff,
    /// The system is shutting down.
    Shutdown,
}

impl Signal {
    /// Kebab-case name, as it appears in logs and in a session's
    /// `exit` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::CtrlC => "ctrl-c",
            Signal::CtrlBreak => "ctrl-break",
            Signal::Close => "close",
            Signal::Logoff => "logoff",
            Signal::Shutdown => "shutdown",
        }
    }

    /// True when Windows will terminate the process the moment the handler
    /// returns, so teardown has to finish *inside* the handler.
    pub fn is_terminal(self) -> bool {
        matches!(self, Signal::Close | Signal::Logoff | Signal::Shutdown)
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// The `CTRL_*_EVENT` values, spelled out here so the dispatch logic compiles
// and is testable on every platform.
const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

/// Map a raw console control event to a [`Signal`]; `None` for anything
/// Windows may add later, which is then left to the default handler.
fn signal_of(kind: u32) -> Option<Signal> {
    match kind {
        CTRL_C_EVENT => Some(Signal::CtrlC),
        CTRL_BREAK_EVENT => Some(Signal::CtrlBreak),
        CTRL_CLOSE_EVENT => Some(Signal::Close),
        CTRL_LOGOFF_EVENT => Some(Signal::Logoff),
        CTRL_SHUTDOWN_EVENT => Some(Signal::Shutdown),
        _ => None,
    }
}

/// The "teardown is done" flag the handler thread waits on.
#[derive(Debug, Default)]
struct Done {
    finished: Mutex<bool>,
    cv: Condvar,
}

impl Done {
    fn set(&self) {
        let mut f = self.finished.lock().unwrap_or_else(|p| p.into_inner());
        *f = true;
        self.cv.notify_all();
    }

    /// Block until [`Self::set`] or `max_wait`. Returns true if teardown
    /// actually finished, false on timeout.
    fn wait(&self, max_wait: Duration) -> bool {
        let guard = self.finished.lock().unwrap_or_else(|p| p.into_inner());
        match self.cv.wait_timeout_while(guard, max_wait, |f| !*f) {
            Ok((f, _)) => *f,
            // A poisoned mutex means some other thread panicked mid-teardown;
            // there is nothing left to wait for.
            Err(_) => false,
        }
    }
}

/// Handed back by [`install`]: the process's way of telling a blocked
/// handler that its sinks are closed and Windows may proceed.
#[derive(Debug, Clone)]
pub struct ShutdownGuard {
    done: Arc<Done>,
}

impl ShutdownGuard {
    /// Teardown is complete — release a handler blocked on a close/logoff/
    /// shutdown event. Idempotent, and harmless when nothing is waiting.
    pub fn finished(&self) {
        self.done.set();
    }

    /// Whether [`Self::finished`] has been called.
    pub fn is_finished(&self) -> bool {
        *self.done.finished.lock().unwrap_or_else(|p| p.into_inner())
    }
}

struct Installed {
    on_signal: Box<dyn Fn(Signal) + Send + Sync>,
    done: Arc<Done>,
    max_wait: Duration,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// Run the callback for one console control event and decide whether it was
/// handled. Split out from the Win32 entry point so the blocking rule can be
/// tested without a console.
fn handle_event(kind: u32) -> bool {
    let Some(signal) = signal_of(kind) else {
        return false;
    };
    let Some(state) = INSTALLED.get() else {
        return false;
    };
    (state.on_signal)(signal);
    if signal.is_terminal() {
        // Windows kills us when this returns, so hold it here.
        state.done.wait(state.max_wait);
    }
    true
}

/// Install the process's console control handler.
///
/// `on_signal` runs on the handler thread, not the main one, and must not
/// block for long itself — for a terminal signal it should kick off teardown
/// and let the main thread call [`ShutdownGuard::finished`]. `max_wait`
/// bounds how long a terminal signal is held; Windows' own grace period is
/// about five seconds for a console close and longer for shutdown.
///
/// Only one handler may be installed per process: a second call is an error
/// rather than a silently ignored registration.
pub fn install(
    on_signal: impl Fn(Signal) + Send + Sync + 'static,
    max_wait: Duration,
) -> io::Result<ShutdownGuard> {
    if CLAIMED.swap(true, Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a console control handler is already installed for this process",
        ));
    }
    let done = Arc::new(Done::default());
    let state = Installed {
        on_signal: Box::new(on_signal),
        done: Arc::clone(&done),
        max_wait,
    };
    if INSTALLED.set(state).is_err() {
        CLAIMED.store(false, Ordering::SeqCst);
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a console control handler is already installed for this process",
        ));
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: `ctrl_handler` is a plain `extern "system"` function with
        // the signature Windows expects and no state of its own; the state it
        // reads lives in a `OnceLock` that is already set and never cleared.
        unsafe {
            SetConsoleCtrlHandler(Some(ctrl_handler), true)
                .map_err(|e| io::Error::other(format!("SetConsoleCtrlHandler failed: {e}")))?;
        }
    }
    Ok(ShutdownGuard { done })
}

#[cfg(windows)]
unsafe extern "system" fn ctrl_handler(kind: u32) -> windows::core::BOOL {
    handle_event(kind).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn event_codes_map_to_signals() {
        assert_eq!(signal_of(CTRL_C_EVENT), Some(Signal::CtrlC));
        assert_eq!(signal_of(CTRL_BREAK_EVENT), Some(Signal::CtrlBreak));
        assert_eq!(signal_of(CTRL_CLOSE_EVENT), Some(Signal::Close));
        assert_eq!(signal_of(CTRL_LOGOFF_EVENT), Some(Signal::Logoff));
        assert_eq!(signal_of(CTRL_SHUTDOWN_EVENT), Some(Signal::Shutdown));
        assert_eq!(signal_of(3), None);
    }

    #[test]
    fn only_close_logoff_shutdown_are_terminal() {
        assert!(!Signal::CtrlC.is_terminal());
        assert!(!Signal::CtrlBreak.is_terminal());
        for s in [Signal::Close, Signal::Logoff, Signal::Shutdown] {
            assert!(s.is_terminal(), "{s}");
        }
        assert_eq!(Signal::CtrlBreak.as_str(), "ctrl-break");
        assert_eq!(Signal::Shutdown.to_string(), "shutdown");
    }

    #[test]
    fn waiting_ends_when_teardown_finishes() {
        let done = Arc::new(Done::default());
        let guard = ShutdownGuard {
            done: Arc::clone(&done),
        };
        assert!(!guard.is_finished());
        let t = std::thread::spawn(move || guard.finished());
        // A generous bound: the assertion is that we return on the signal,
        // not on the timeout.
        assert!(done.wait(Duration::from_secs(10)), "should not time out");
        t.join().unwrap();
    }

    #[test]
    fn waiting_gives_up_after_max_wait() {
        let done = Done::default();
        let started = std::time::Instant::now();
        assert!(!done.wait(Duration::from_millis(50)));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn finishing_before_the_wait_is_not_missed() {
        let done = Done::default();
        done.set();
        done.set(); // idempotent
        assert!(done.wait(Duration::from_secs(0)));
    }

    /// One test drives the whole process-global install, because there is
    /// exactly one handler slot per process.
    #[test]
    fn installed_handler_runs_the_callback_and_blocks_only_for_terminal_events() {
        let (tx, rx) = mpsc::channel();
        let guard = install(move |s| tx.send(s).unwrap(), Duration::from_millis(200))
            .expect("first install succeeds");

        // A second install is refused rather than silently replacing the first.
        let err = install(|_| {}, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        // Ctrl-C returns immediately.
        let started = std::time::Instant::now();
        assert!(handle_event(CTRL_C_EVENT));
        assert_eq!(rx.recv().unwrap(), Signal::CtrlC);
        assert!(started.elapsed() < Duration::from_millis(150));

        // An unknown event is not ours.
        assert!(!handle_event(42));

        // A close event blocks until teardown reports back.
        let g = guard.clone();
        let waiter = std::thread::spawn(move || {
            let s = rx.recv().unwrap();
            g.finished();
            s
        });
        let started = std::time::Instant::now();
        assert!(handle_event(CTRL_CLOSE_EVENT));
        assert_eq!(waiter.join().unwrap(), Signal::Close);
        assert!(guard.is_finished());
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "released by finished(), not by the timeout"
        );
    }
}
