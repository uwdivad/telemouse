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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tracing::{debug, info, warn};

use crate::manager::{ComponentState, Manager, StartRequest};
use crate::procs::{ProcInfo, Scanner};

/// Log lines per component in a snapshot: the window shows one log tail.
pub const GUI_LOG_LINES: usize = 40;
/// Publisher cadence while the window is visible.
pub const TICK: Duration = Duration::from_secs(1);
/// While the window is hidden only every n-th tick publishes (the tray only
/// needs to know whether capture is running), and no process scan runs.
pub const HIDDEN_EVERY: u32 = 5;
/// How old a process scan may be and still be shown. The page uses 4 s; the
/// window refreshes every second, so it gets a fresher one at the same cost
/// per scan (~100 µs, see `procs.rs`).
const SCAN_TTL: Duration = Duration::from_secs(2);

/// Everything the window and the tray render.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub now_unix_s: u64,
    pub components: Vec<ComponentState>,
    /// Empty while the window is hidden — the tray does not show processes.
    pub processes: Vec<ProcInfo>,
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
    /// `http://…/` of the web panel, for the "Open web panel" item.
    pub panel_url: String,
}

/// Take snapshots forever: every [`TICK`] while visible, every
/// [`HIDDEN_EVERY`] ticks while hidden, and at once on `poke`. Stops when
/// the receiver is gone.
pub async fn run_publisher(
    manager: Arc<Manager>,
    scanner: Arc<Scanner>,
    tx: watch::Sender<Arc<Snapshot>>,
    poke: Arc<Notify>,
    visible: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick: u32 = 0;
    loop {
        let poked = tokio::select! {
            _ = interval.tick() => false,
            _ = poke.notified() => true,
        };
        tick = tick.wrapping_add(1);
        let vis = visible.load(Ordering::Relaxed);
        if !poked && !vis && !tick.is_multiple_of(HIDDEN_EVERY) {
            continue;
        }
        let snap = if vis {
            let components = manager.snapshot(GUI_LOG_LINES).await;
            let sc = scanner.clone();
            let processes = tokio::task::spawn_blocking(move || sc.scan_cached(SCAN_TTL))
                .await
                .unwrap_or_default();
            Snapshot { now_unix_s: crate::manager::now_unix(), components, processes }
        } else {
            Snapshot {
                now_unix_s: crate::manager::now_unix(),
                components: manager.snapshot(0).await,
                processes: Vec::new(),
            }
        };
        debug!(visible = vis, poked, processes = snap.processes.len(), "gui snapshot");
        if tx.send(Arc::new(snap)).is_err() {
            debug!("gui snapshot receiver gone; publisher exiting");
            return;
        }
        wake();
    }
}

/// Start a component with no extra flags (the tray's quick action). The
/// outcome is logged and shows up in the next snapshot.
pub fn start(link: &GuiLink, id: &'static str) {
    let (m, s, poke) = (link.manager.clone(), link.scanner.clone(), link.poke.clone());
    link.handle.spawn(async move {
        match m.start(id, &StartRequest::default()).await {
            Ok(pid) => info!(component = id, pid, "started from the tray"),
            Err(e) => warn!(component = id, error = %e, "tray start refused"),
        }
        s.invalidate();
        poke.notify_one();
    });
}

/// Graceful stop (Ctrl-Break, then terminate after the grace period).
pub fn stop(link: &GuiLink, id: &'static str) {
    let (m, s, poke) = (link.manager.clone(), link.scanner.clone(), link.poke.clone());
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
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    /// A manager over an empty binary directory: nothing can ever launch.
    fn manager() -> Arc<Manager> {
        Arc::new(Manager::new(
            crate::manager::COMPONENTS,
            Some(std::env::temp_dir().join("telemouse-ctl-gui-no-bins")),
            PathBuf::from("telemouse.toml"),
            PathBuf::from("recordings"),
            Duration::from_secs(1),
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
        let task = tokio::spawn(run_publisher(
            manager(),
            Arc::new(Scanner::new()),
            tx,
            poke.clone(),
            Arc::new(AtomicBool::new(visible)),
            Arc::new(move || {
                w.fetch_add(1, Ordering::Relaxed);
            }),
        ));
        Rig { rx, poke, wakes, task }
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
        assert!(t0.elapsed() < Duration::from_secs(2), "poke must not wait for the cadence");
        assert_eq!(s.components.len(), crate::manager::COMPONENTS.len());
        assert!(s.processes.is_empty(), "no process scan while hidden");
        assert!(s.components.iter().all(|c| c.log.is_empty()), "no log lines while hidden");
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

    #[tokio::test]
    async fn tray_actions_are_refused_cleanly_and_poke_the_publisher() {
        let (_tx, rx) = watch::channel(Arc::new(Snapshot::default()));
        let poke = Arc::new(Notify::new());
        let link = GuiLink {
            handle: tokio::runtime::Handle::current(),
            manager: manager(),
            scanner: Arc::new(Scanner::new()),
            state: rx,
            poke: poke.clone(),
            quit: Arc::new(Notify::new()),
            visible: Arc::new(AtomicBool::new(true)),
            panel_url: "http://127.0.0.1:7880/".into(),
        };
        // No binaries: the start fails at spawn, the stop finds nothing running;
        // both must still poke so the UI refreshes.
        start(&link, "capture");
        tokio::time::timeout(Duration::from_secs(5), poke.notified()).await.unwrap();
        stop(&link, "viz");
        tokio::time::timeout(Duration::from_secs(5), poke.notified()).await.unwrap();
        assert!(link.manager.snapshot(1).await.iter().all(|c| !c.running));
    }
}
