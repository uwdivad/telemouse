//! The native status window and tray icon (Windows only).
//!
//! * [`feed`] — the runtime side: a publisher task that snapshots the manager
//!   and the process table, and the spawned start/stop actions. Portable.
//! * [`model`] — what to show: text, tooltip, icon state, menu, icon pixels.
//!   Pure, tested anywhere.
//! * `win` — the Win32 glue: one window, one `EDIT`, `Shell_NotifyIcon`, a
//!   popup menu, and the message loop on its own OS thread.
//!
//! Off Windows [`spawn`] returns `None` and the panel is what it always was:
//! a web page. On Windows `--no-gui` does the same.

pub mod feed;
pub mod model;
#[cfg(windows)]
mod win;

use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicU32};

use telemouse_core::hotkey::Hotkey;
use tokio::sync::Notify;

use crate::manager::Manager;
use crate::places::Places;
use crate::procs::Scanner;

pub struct GuiDeps {
    pub handle: tokio::runtime::Handle,
    pub manager: Arc<Manager>,
    pub scanner: Arc<Scanner>,
    /// Notified once when the user picks Exit; `main` stops the server.
    pub quit: Arc<Notify>,
    /// `[ctl] hotkey`: the system-wide new-session chord, if any.
    pub hotkey: Option<Hotkey>,
    /// Version, config, logs and docs: what the header and the menu open.
    pub places: Places,
}

/// The running UI thread. [`GuiHandle::shutdown`] is the only way to end it.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct GuiHandle {
    thread: std::thread::JoinHandle<()>,
    /// The window, once created (0 until then, and again after teardown).
    hwnd: Arc<AtomicIsize>,
    /// The UI thread's id: the fallback quit path if no window ever existed.
    thread_id: Arc<AtomicU32>,
}

impl GuiHandle {
    /// Remove the tray icon, close the window and join the thread. Call it
    /// after the server has shut down, so the icon outlives the children it
    /// represents by nothing.
    pub fn shutdown(self) {
        #[cfg(windows)]
        {
            use std::sync::atomic::Ordering;
            win::post_quit(self.hwnd.load(Ordering::Acquire));
            win::post_thread_quit(self.thread_id.load(Ordering::Acquire));
        }
        if self.thread.join().is_err() {
            tracing::warn!("gui thread panicked");
        }
    }
}

/// A modal message box for the moments the console is already gone: a
/// config that cannot be read, a port that is taken. Off Windows (and in
/// tests) it is a line on stderr.
pub fn alert(title: &str, text: &str) {
    #[cfg(windows)]
    win::alert(title, text);
    #[cfg(not(windows))]
    eprintln!("{title}: {text}");
}

/// Open a URL or a path with the shell's default handler. `false` when the
/// shell refused (or off Windows, where nothing is opened).
pub fn open_url(target: &str) -> bool {
    #[cfg(windows)]
    {
        win::shell_open(target)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        false
    }
}

/// Start the publisher task and the UI thread. `None` off Windows, or when
/// the thread could not be spawned (the server keeps running headless).
pub fn spawn(deps: GuiDeps) -> Option<GuiHandle> {
    #[cfg(not(windows))]
    {
        let _ = deps;
        None
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
        use tokio::sync::watch;

        let hwnd = Arc::new(AtomicIsize::new(0));
        let thread_id = Arc::new(AtomicU32::new(0));
        let (tx, rx) = watch::channel(Arc::new(feed::Snapshot::default()));
        let poke = Arc::new(Notify::new());
        let visible = Arc::new(AtomicBool::new(true));
        // Written by the UI thread once RegisterHotKey has answered, read by
        // the publisher so the window can say when the chord is not ours.
        let hotkey_status = Arc::new(AtomicU8::new(feed::HOTKEY_UNKNOWN));

        let wake_hwnd = hwnd.clone();
        deps.handle.spawn(feed::run_publisher(feed::Publisher {
            manager: deps.manager.clone(),
            scanner: deps.scanner.clone(),
            tx,
            poke: poke.clone(),
            visible: visible.clone(),
            wake: Arc::new(move || win::post_refresh(wake_hwnd.load(Ordering::Acquire))),
            hotkey: deps.hotkey.map(|h| h.to_string()).unwrap_or_default(),
            hotkey_status: hotkey_status.clone(),
            places: deps.places.clone(),
        }));

        let link = feed::GuiLink {
            handle: deps.handle,
            manager: deps.manager,
            scanner: deps.scanner,
            state: rx,
            poke,
            quit: deps.quit,
            visible,
            hotkey: deps.hotkey,
            hotkey_status,
            restarting: Arc::new(AtomicBool::new(false)),
        };
        let (h, t) = (hwnd.clone(), thread_id.clone());
        let thread = std::thread::Builder::new()
            .name("ctl-gui".into())
            .spawn(move || {
                if let Err(e) = win::run(link, h, t) {
                    tracing::warn!(error = %e, "gui unavailable; the web panel keeps serving");
                }
            });
        match thread {
            Ok(thread) => Some(GuiHandle {
                thread,
                hwnd,
                thread_id,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "could not start the gui thread");
                None
            }
        }
    }
}
