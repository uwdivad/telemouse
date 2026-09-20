//! The bridge between the tokio side of the panel and the UI thread.
//!
//! State flows one way: a publisher task on the runtime takes a [`Snapshot`]
//! of the manager and the process table, hands it over through a `watch`
//! channel, and calls `wake` — on Windows a `PostMessageW` to the window, so
//! the UI thread never blocks on anything. Actions flow the other way as
//! spawned futures ([`start`], [`stop`]): the UI thread fires them and reads
//! the result off the next snapshot, like the web page does.
//!
//! Nothing in this file touches Win32, so the cadence and the action helpers
//! are tested wherever `cargo test` runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use telemouse_core::hotkey::Hotkey;
use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

use crate::manager::{ComponentState, Manager, StartRequest, StopError};
use crate::places::Places;
use crate::procs::{ProcInfo, Scanner};

/// Log lines per component in a snapshot: the window shows one log tail.
pub const GUI_LOG_LINES: usize = 40;
/// Publisher cadence while the window is visible.
pub const TICK: Duration = Duration::from_secs(1);
/// While the window is hidden only every n-th tick publishes (the tray only
/// needs to know whether capture is running), and no process scan runs.
pub const HIDDEN_EVERY: u32 = 5;
/// Hidden *and* nothing running: the publisher stops ticking altogether and
/// waits to be told something happened, with this as the longest it will
/// sleep through. The cadence is what it always was (`TICK * HIDDEN_EVERY`),
/// but an idle panel now wakes once instead of five times to reach it.
pub const IDLE_WAIT: Duration = Duration::from_secs(5);
/// How old a process scan may be and still be shown. The page uses 4 s; the
/// window refreshes every second, so it gets a fresher one at the same cost
/// per scan (~100 µs, see `procs.rs`).
const SCAN_TTL: Duration = Duration::from_secs(2);

/// The hotkey has not been registered yet (or there is none to register).
pub const HOTKEY_UNKNOWN: u8 = 0;
/// `RegisterHotKey` succeeded: the chord is ours system-wide.
pub const HOTKEY_OK: u8 = 1;
/// `RegisterHotKey` was refused — another program holds the chord.
pub const HOTKEY_FAILED: u8 = 2;

/// `None` while nothing has been attempted, so the window does not accuse a
/// hotkey of failing before the UI thread has had a chance to register it.
pub fn hotkey_registered(status: u8) -> Option<bool> {
    match status {
        HOTKEY_OK => Some(true),
        HOTKEY_FAILED => Some(false),
        _ => None,
    }
}

/// Everything the window and the tray render.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub now_unix_s: u64,
    pub components: Vec<ComponentState>,
    /// Empty while the window is hidden — the tray does not show processes.
    pub processes: Vec<ProcInfo>,
    /// `recording.enabled` in the config the children are launched with.
    pub recording_enabled: bool,
    pub recording_dir: String,
    /// The new-session hotkey as the user sees it (`Ctrl+Alt+R`); empty
    /// when none is configured.
    pub hotkey: String,
    /// Whether that chord is actually registered. `None` until the UI
    /// thread has tried.
    pub hotkey_registered: Option<bool>,
    /// Version, panel URL and the absolute paths the menu can open.
    pub places: Places,
}

/// What the UI thread holds: the shared handles it reads and pokes.
pub struct GuiLink {
    pub handle: tokio::runtime::Handle,
    pub manager: Arc<Manager>,
    pub scanner: Arc<Scanner>,
    pub state: watch::Receiver<Arc<Snapshot>>,
    /// Ask the publisher for a snapshot now (after an action, on show).
    pub poke: Arc<Notify>,
    /// Tell `main` to shut the server down and stop the children.
    pub quit: Arc<Notify>,
    /// Whether the window is on screen — decides the publisher's cadence.
    pub visible: Arc<AtomicBool>,

    /// `[ctl] hotkey`, parsed; the UI thread registers it system-wide.
    pub hotkey: Option<Hotkey>,
    /// Set by the UI thread once it knows whether the chord is ours.
    pub hotkey_status: Arc<AtomicU8>,
    /// A [`new_session`] is in flight (stop, then start). A second press
    /// during the stop's grace period is ignored rather than raced.
    pub restarting: Arc<AtomicBool>,

    /// `http://…/`: what the window's embedded browser navigates to. The
    /// first snapshot in the channel is empty, so it is carried here.
    pub panel_url: String,
    /// Where WebView2 may write its cache; `None` when nowhere is writable,
    /// which is a text-only window.
    pub webview_data_dir: Option<std::path::PathBuf>,
    /// `--no-webview`: text-only window on purpose, no Edge components.
    pub no_webview: bool,
}

impl GuiLink {
    /// Stop every managed child with the grace clamped, blocking this
    /// thread until it is done. Called from the UI thread while Windows
    /// holds the session open (`WM_QUERYENDSESSION`), which is the one
    /// place a blocking call there is the correct thing to do: the
    /// alternative is the children being killed mid-recording.
    pub fn stop_all_fast_blocking(&self) {
        let m = self.manager.clone();
        self.handle.block_on(async move { m.stop_all_fast().await });
    }
}

/// Everything [`run_publisher`] needs. A struct rather than nine
/// positional arguments.
pub struct Publisher {
    pub manager: Arc<Manager>,
    pub scanner: Arc<Scanner>,
    pub tx: watch::Sender<Arc<Snapshot>>,
    pub poke: Arc<Notify>,
    pub visible: Arc<AtomicBool>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
    /// `[ctl] hotkey` as the user wrote it; empty when there is none.
    pub hotkey: String,
    pub hotkey_status: Arc<AtomicU8>,
    pub places: Places,
}

/// Take snapshots forever: every [`TICK`] while visible, every
/// [`HIDDEN_EVERY`] ticks while hidden, at once on `poke` — and, when the
/// window is hidden *and* nothing is running, only when something happens
/// or [`IDLE_WAIT`] passes. Stops when the receiver is gone.
pub async fn run_publisher(p: Publisher) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick: u32 = 0;
    // Nothing has been observed yet, so start on the normal cadence.
    let mut idle = false;
    loop {
        let poked = if idle && !p.visible.load(Ordering::Relaxed) {
            // An idle panel with no window has nothing to recompute. Wait
            // for a start or a stop (from the tray *or* the web page, which
            // is why the manager's own notify is in here), for the window
            // to come back, or for the long fallback.
            tokio::select! {
                _ = p.poke.notified() => {}
                _ = p.manager.work().notified() => {}
                _ = tokio::time::sleep(IDLE_WAIT) => {}
            }
            true
        } else {
            tokio::select! {
                _ = interval.tick() => false,
                _ = p.poke.notified() => true,
            }
        };
        tick = tick.wrapping_add(1);
        let vis = p.visible.load(Ordering::Relaxed);
        if !poked && !vis && !tick.is_multiple_of(HIDDEN_EVERY) {
            continue;
        }
        let rec = p.manager.recording();
        let (components, processes) = if vis {
            let components = p.manager.snapshot(GUI_LOG_LINES).await;
            let sc = p.scanner.clone();
            let processes = tokio::task::spawn_blocking(move || sc.scan_cached(SCAN_TTL))
                .await
                .unwrap_or_default();
            (components, processes)
        } else {
            (p.manager.snapshot(0).await, Vec::new())
        };
        idle = !components.iter().any(|c| c.running);
        let snap = Snapshot {
            now_unix_s: crate::manager::now_unix(),
            components,
            processes,
            recording_enabled: rec.enabled,
            recording_dir: rec.dir,
            hotkey: p.hotkey.clone(),
            hotkey_registered: hotkey_registered(p.hotkey_status.load(Ordering::Relaxed)),
            places: p.places.clone(),
        };
        debug!(
            visible = vis,
            poked,
            idle,
            processes = snap.processes.len(),
            "gui snapshot"
        );
        if p.tx.send(Arc::new(snap)).is_err() {
            debug!("gui snapshot receiver gone; publisher exiting");
            return;
        }
        (p.wake)();
    }
}

/// Start a component (the tray's quick action); `save` is the capture
/// card's switch, resolved by the manager. The outcome is logged and shows
/// up in the next snapshot.
pub fn start(link: &GuiLink, id: &'static str, save: Option<bool>) {
    let (m, s, poke) = (
        link.manager.clone(),
        link.scanner.clone(),
        link.poke.clone(),
    );
    link.handle.spawn(async move {
        let req = StartRequest {
            save,
            ..Default::default()
        };
        match m.start(id, &req).await {
            Ok(pid) => info!(component = id, pid, save = ?save, "started from the tray"),
            Err(e) => warn!(component = id, error = %e, "tray start refused"),
        }
        s.invalidate();
        poke.notify_one();
    });
}

/// A fresh recording — what the hotkey and the *New session* menu item do:
/// stop the capture agent if it is running (gracefully, so the file it was
/// writing is complete), then start one that saves, whatever `[recording]
/// enabled` says. Every capture run is its own session file, so this is
/// how a long sitting gets split. One at a time: a call while the previous
/// one is still stopping is dropped with a log line.
pub fn new_session(link: &GuiLink) {
    if link.restarting.swap(true, Ordering::AcqRel) {
        info!("new session already in progress; ignored");
        return;
    }
    let (m, s, poke, busy) = (
        link.manager.clone(),
        link.scanner.clone(),
        link.poke.clone(),
        link.restarting.clone(),
    );
    link.handle.spawn(async move {
        match m.stop("capture", false).await {
            Ok(outcome) => info!(?outcome, "new session: previous capture stopped"),
            Err(StopError::NotRunning) => {}
            Err(e) => warn!(error = %e, "new session: stop refused"),
        }
        let req = StartRequest {
            save: Some(true),
            ..Default::default()
        };
        match m.start("capture", &req).await {
            Ok(pid) => info!(pid, "new session: capture started, saving"),
            Err(e) => warn!(error = %e, "new session: start refused"),
        }
        busy.store(false, Ordering::Release);
        s.invalidate();
        poke.notify_one();
    });
}

/// Graceful stop (Ctrl-Break, then terminate after the grace period).
pub fn stop(link: &GuiLink, id: &'static str) {
    let (m, s, poke) = (
        link.manager.clone(),
        link.scanner.clone(),
        link.poke.clone(),
    );
    link.handle.spawn(async move {
        match m.stop(id, false).await {
            Ok(outcome) => info!(component = id, ?outcome, "stopped from the tray"),
            Err(e) => warn!(component = id, error = %e, "tray stop refused"),
        }
        s.invalidate();
        poke.notify_one();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::{ConfigStatus, ManagerConfig};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    /// A manager over an empty binary directory: nothing can ever launch.
    fn manager() -> Arc<Manager> {
        Arc::new(Manager::new(
            crate::manager::COMPONENTS,
            ManagerConfig {
                bin_dir: Some(std::env::temp_dir().join("telemouse-ctl-gui-no-bins")),
                config_path: PathBuf::from("telemouse.toml"),
                recordings_dir: PathBuf::from("recordings"),
                recording_enabled: true,
                config_status: ConfigStatus::Defaults,
                grace: Duration::from_secs(1),
                log_dir: None,
            },
        ))
    }

    struct Rig {
        rx: watch::Receiver<Arc<Snapshot>>,
        poke: Arc<Notify>,
        wakes: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    fn rig(visible: bool) -> Rig {
        let (tx, rx) = watch::channel(Arc::new(Snapshot::default()));
        let poke = Arc::new(Notify::new());
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let task = tokio::spawn(run_publisher(Publisher {
            manager: manager(),
            scanner: Arc::new(Scanner::new()),
            tx,
            poke: poke.clone(),
            visible: Arc::new(AtomicBool::new(visible)),
            wake: Arc::new(move || {
                w.fetch_add(1, Ordering::Relaxed);
            }),
            hotkey: "Ctrl+Alt+R".into(),
            hotkey_status: Arc::new(AtomicU8::new(HOTKEY_OK)),
            places: Places {
                version: "9.9.9".into(),
                panel_url: "http://127.0.0.1:7880/".into(),
                ..Default::default()
            },
        }));
        Rig {
            rx,
            poke,
            wakes,
            task,
        }
    }

    async fn next(rx: &mut watch::Receiver<Arc<Snapshot>>) -> Arc<Snapshot> {
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("a snapshot within 5 s")
            .expect("publisher alive");
        rx.borrow_and_update().clone()
    }

    #[tokio::test]
    async fn visible_publishes_components_processes_and_wakes() {
        let mut r = rig(true);
        let s = next(&mut r.rx).await;
        assert_eq!(s.components.len(), crate::manager::COMPONENTS.len());
        assert!(s.components.iter().all(|c| !c.running));
        // This test binary is itself a telemouse process, so the scan is never empty.
        assert!(!s.processes.is_empty());
        assert!(s.now_unix_s > 1_700_000_000);
        assert!(s.recording_enabled);
        assert_eq!(s.recording_dir, "recordings");
        assert_eq!(s.hotkey, "Ctrl+Alt+R");
        assert_eq!(s.hotkey_registered, Some(true));
        assert_eq!(s.places.version, "9.9.9");
        assert!(r.wakes.load(Ordering::Relaxed) >= 1);
        r.task.abort();
    }

    #[tokio::test]
    async fn hidden_skips_the_process_scan_and_answers_a_poke_at_once() {
        let mut r = rig(false);
        // The first tick is skipped while hidden (tick 1 of 5); a poke is not.
        let t0 = std::time::Instant::now();
        r.poke.notify_one();
        let s = next(&mut r.rx).await;
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "poke must not wait for the cadence"
        );
        assert_eq!(s.components.len(), crate::manager::COMPONENTS.len());
        assert!(s.processes.is_empty(), "no process scan while hidden");
        assert!(
            s.components.iter().all(|c| c.log.is_empty()),
            "no log lines while hidden"
        );
        r.task.abort();
    }

    /// With the window hidden and nothing running, the publisher parks: it
    /// must still answer a poke immediately, and must not have spun in the
    /// meantime.
    #[tokio::test]
    async fn a_hidden_idle_publisher_parks_but_still_answers() {
        let mut r = rig(false);
        r.poke.notify_one();
        let _ = next(&mut r.rx).await;
        let after_first = r.wakes.load(Ordering::Relaxed);
        // Well under IDLE_WAIT: a parked publisher publishes nothing.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            r.wakes.load(Ordering::Relaxed),
            after_first,
            "a parked publisher must not tick"
        );
        let t0 = std::time::Instant::now();
        r.poke.notify_one();
        let _ = next(&mut r.rx).await;
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "a poke still wakes it"
        );
        r.task.abort();
    }

    #[tokio::test]
    async fn publisher_exits_when_the_receiver_is_dropped() {
        let r = rig(true);
        drop(r.rx);
        tokio::time::timeout(Duration::from_secs(5), r.task)
            .await
            .expect("publisher must stop once nobody listens")
            .unwrap();
    }

    #[test]
    fn a_hotkey_is_only_accused_once_it_has_been_tried() {
        assert_eq!(hotkey_registered(HOTKEY_UNKNOWN), None);
        assert_eq!(hotkey_registered(HOTKEY_OK), Some(true));
        assert_eq!(hotkey_registered(HOTKEY_FAILED), Some(false));
    }

    fn link(poke: Arc<Notify>) -> GuiLink {
        let (_tx, rx) = watch::channel(Arc::new(Snapshot::default()));
        GuiLink {
            handle: tokio::runtime::Handle::current(),
            manager: manager(),
            scanner: Arc::new(Scanner::new()),
            state: rx,
            poke,
            quit: Arc::new(Notify::new()),
            visible: Arc::new(AtomicBool::new(true)),
            hotkey: Hotkey::parse("ctrl+alt+r").unwrap(),
            hotkey_status: Arc::new(AtomicU8::new(HOTKEY_UNKNOWN)),
            restarting: Arc::new(AtomicBool::new(false)),
            panel_url: "http://127.0.0.1:7880/".into(),
            webview_data_dir: None,
            no_webview: true,
        }
    }

    #[tokio::test]
    async fn tray_actions_are_refused_cleanly_and_poke_the_publisher() {
        let poke = Arc::new(Notify::new());
        let link = link(poke.clone());
        // No binaries: the start fails at spawn, the stop finds nothing running;
        // both must still poke so the UI refreshes.
        start(&link, "capture", Some(false));
        tokio::time::timeout(Duration::from_secs(5), poke.notified())
            .await
            .unwrap();
        stop(&link, "viz");
        tokio::time::timeout(Duration::from_secs(5), poke.notified())
            .await
            .unwrap();
        assert!(link.manager.snapshot(1).await.iter().all(|c| !c.running));
    }

    /// With nothing running the hotkey's action is a plain saving start; it
    /// still pokes when that start is refused, and it releases its guard.
    #[tokio::test]
    async fn new_session_is_one_at_a_time_and_pokes_when_done() {
        let poke = Arc::new(Notify::new());
        let link = link(poke.clone());
        new_session(&link);
        assert!(
            link.restarting.load(Ordering::Acquire),
            "guard held while in flight"
        );
        tokio::time::timeout(Duration::from_secs(5), poke.notified())
            .await
            .unwrap();
        assert!(!link.restarting.load(Ordering::Acquire), "guard released");
        assert!(link.manager.snapshot(1).await.iter().all(|c| !c.running));

        // A press while one is in flight is dropped, not queued: no second poke.
        link.restarting.store(true, Ordering::Release);
        new_session(&link);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), poke.notified())
                .await
                .is_err(),
            "the ignored press must not spawn anything"
        );
        assert!(
            link.restarting.load(Ordering::Acquire),
            "the ignored press does not clear the guard"
        );
    }
}
