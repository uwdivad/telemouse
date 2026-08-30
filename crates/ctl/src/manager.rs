//! Managed children: the components the panel can start and stop, what they
//! print, and how they are asked to go away.
//!
//! Every component is a fixed binary plus fixed base arguments; a request may
//! add flags only from that component's allow-list and, for the analyzer,
//! name one recording. The panel is a launcher, not a shell.
//!
//! Stopping is two-stage. Children are spawned in their own process group
//! (Windows `CREATE_NEW_PROCESS_GROUP`), so a `CTRL_BREAK` addressed to that
//! group reaches exactly that child — and the capture agent's Ctrl handler
//! then flushes its partial batch and closes its sinks in order. If the child
//! is still alive after the grace period (or the panel has no console to
//! send the event from), it is terminated.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tracing::{info, warn};

/// Lines of child output kept per component (across restarts).
const LOG_CAPACITY: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Runs until stopped (capture agent, viz server).
    Service,
    /// Runs to completion and exits (doctor, analysis).
    Task,
}

#[derive(Debug, Clone, Serialize)]
pub struct Flag {
    pub flag: &'static str,
    pub help: &'static str,
}

#[derive(Debug, Clone)]
pub struct Component {
    pub id: &'static str,
    pub label: &'static str,
    pub summary: &'static str,
    /// Executable name without extension.
    pub bin: &'static str,
    pub base_args: &'static [&'static str],
    pub kind: Kind,
    /// Flags a request may add; anything else is refused.
    pub flags: &'static [Flag],
    /// Takes one `recordings/<session>.jsonl` as a positional argument.
    pub takes_session: bool,
    /// Gets `--config <telemouse.toml>` appended.
    pub passes_config: bool,
}

/// The panel's catalogue. Order is display order.
pub const COMPONENTS: &[Component] = &[
    Component {
        id: "capture",
        label: "Capture agent",
        summary: "telemouse run — raw mouse input → UDP / recording / Kafka",
        bin: "telemouse",
        base_args: &["run"],
        kind: Kind::Service,
        flags: &[
            Flag { flag: "--print", help: "log a one-line summary for every batch" },
            Flag { flag: "--no-kafka", help: "disable the Kafka sink" },
            Flag { flag: "--no-udp", help: "disable the localhost UDP sink (live viz)" },
            Flag { flag: "--no-record", help: "disable the JSONL recording" },
        ],
        takes_session: false,
        passes_config: true,
    },
    Component {
        id: "viz",
        label: "Viz server",
        summary: "telemouse-viz serve — live dashboard, replay, OBS overlay",
        bin: "telemouse-viz",
        base_args: &["serve"],
        kind: Kind::Service,
        flags: &[],
        takes_session: false,
        passes_config: true,
    },
    Component {
        id: "doctor",
        label: "Doctor",
        summary: "telemouse doctor — check clock, monitors, devices, UDP, Kafka",
        bin: "telemouse",
        base_args: &["doctor"],
        kind: Kind::Task,
        flags: &[],
        takes_session: false,
        passes_config: true,
    },
    Component {
        id: "trend",
        label: "Analyze: trend",
        summary: "telemouse-analyze trend — one row per recorded session",
        bin: "telemouse-analyze",
        base_args: &["trend"],
        kind: Kind::Task,
        flags: &[],
        takes_session: false,
        passes_config: false,
    },
    Component {
        id: "report",
        label: "Analyze: report",
        summary: "telemouse-analyze report <session> — metrics for one recording",
        bin: "telemouse-analyze",
        base_args: &["report"],
        kind: Kind::Task,
        flags: &[Flag { flag: "--timing", help: "print the per-phase timing table" }],
        takes_session: true,
        passes_config: false,
    },
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartRequest {
    #[serde(default)]
    pub flags: Vec<String>,
    /// File name (not path) of a recording in the recordings directory.
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExitInfo {
    /// `None` when killed by a signal (Unix) — on Windows always a code.
    pub code: Option<i32>,
    pub at_unix_s: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentState {
    pub id: &'static str,
    pub label: &'static str,
    pub summary: &'static str,
    pub kind: Kind,
    pub bin: &'static str,
    pub bin_path: String,
    pub bin_found: bool,
    pub flags: Vec<Flag>,
    pub takes_session: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub since_unix_s: Option<u64>,
    pub last_exit: Option<ExitInfo>,
    pub log: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StartError {
    UnknownComponent,
    AlreadyRunning,
    FlagNotAllowed(String),
    SessionRequired,
    BadSession(String),
    Spawn(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownComponent => f.write_str("no such component"),
            Self::AlreadyRunning => f.write_str("already running"),
            Self::FlagNotAllowed(x) => write!(f, "flag not allowed for this component: {x}"),
            Self::SessionRequired => f.write_str("this component needs a session"),
            Self::BadSession(x) => write!(f, "bad session: {x}"),
            Self::Spawn(x) => write!(f, "could not start: {x}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopError {
    UnknownComponent,
    NotRunning,
}

impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnknownComponent => "no such component",
            Self::NotRunning => "not running",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopOutcome {
    /// Exited on its own after the Ctrl-Break.
    Graceful,
    /// Had to be terminated (grace period elapsed, no console, or `force`).
    Terminated,
}

/// Bounded, shared line buffer for one component's output.
#[derive(Default)]
pub struct LogRing {
    lines: VecDeque<String>,
}

impl LogRing {
    fn push(&mut self, line: String) {
        if self.lines.len() == LOG_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }
    fn tail(&self, n: usize) -> Vec<String> {
        self.lines.iter().rev().take(n).rev().cloned().collect()
    }
}

struct Slot {
    child: Option<Child>,
    pid: Option<u32>,
    since: Option<u64>,
    last_exit: Option<ExitInfo>,
    log: Arc<Mutex<LogRing>>,
}

impl Slot {
    fn new() -> Self {
        Self {
            child: None,
            pid: None,
            since: None,
            last_exit: None,
            log: Arc::new(Mutex::new(LogRing::default())),
        }
    }

    /// Record an exit if the child has one; returns whether it is gone.
    fn reap(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return true;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let exit = ExitInfo { code: status.code(), at_unix_s: now_unix() };
                self.log
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(format!("--- exited: {} ---", describe_exit(exit)));
                self.last_exit = Some(exit);
                self.child = None;
                self.pid = None;
                self.since = None;
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!(error = %e, "try_wait failed; treating child as gone");
                self.child = None;
                self.pid = None;
                self.since = None;
                true
            }
        }
    }
}

pub(crate) fn describe_exit(e: ExitInfo) -> String {
    match e.code {
        // STATUS_CONTROL_C_EXIT: how a Windows process reports "left on Ctrl-Break".
        Some(-1073741510) => "Ctrl-Break".into(),
        Some(c) => format!("code {c}"),
        None => "signal".into(),
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Manager {
    components: Vec<Component>,
    bin_dir: Option<PathBuf>,
    config_path: PathBuf,
    recordings_dir: PathBuf,
    grace: Duration,
    slots: tokio::sync::Mutex<HashMap<&'static str, Slot>>,
}

impl Manager {
    pub fn new(
        components: &[Component],
        bin_dir: Option<PathBuf>,
        config_path: PathBuf,
        recordings_dir: PathBuf,
        grace: Duration,
    ) -> Self {
        let slots = components.iter().map(|c| (c.id, Slot::new())).collect();
        Self {
            components: components.to_vec(),
            bin_dir,
            config_path,
            recordings_dir,
            grace,
            slots: tokio::sync::Mutex::new(slots),
        }
    }

    fn component(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    /// Where a component's binary is expected: `bin_dir` if configured, else
    /// next to this executable, else bare name (PATH lookup).
    pub fn resolve_bin(&self, bin: &str) -> (PathBuf, bool) {
        let file = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
        if let Some(dir) = &self.bin_dir {
            let p = dir.join(&file);
            let found = p.is_file();
            return (p, found);
        }
        if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
            let p = dir.join(&file);
            if p.is_file() {
                return (p, true);
            }
        }
        (PathBuf::from(&file), which(&file))
    }

    /// A recording name must be a plain `*.jsonl` file name inside the
    /// recordings directory — no separators, no traversal.
    pub fn validate_session(&self, name: &str) -> Result<PathBuf, String> {
        if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
            return Err("must be a file name inside the recordings directory".into());
        }
        if !name.ends_with(".jsonl") {
            return Err("must be a .jsonl recording".into());
        }
        let p = self.recordings_dir.join(name);
        if !p.is_file() {
            return Err(format!("no such recording: {name}"));
        }
        Ok(p)
    }

    /// `*.jsonl` files in the recordings directory, newest first.
    pub fn list_sessions(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.recordings_dir) else {
            return Vec::new();
        };
        let mut v: Vec<(SystemTime, String)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".jsonl") || !e.file_type().ok()?.is_file() {
                    return None;
                }
                let modified = e.metadata().ok()?.modified().unwrap_or(UNIX_EPOCH);
                Some((modified, name))
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        v.into_iter().map(|(_, n)| n).collect()
    }

    /// Reap exited children. Called on every state read and from a timer, so
    /// an unwatched panel still records exits.
    pub async fn reap(&self) {
        let mut slots = self.slots.lock().await;
        for s in slots.values_mut() {
            s.reap();
        }
    }

    pub async fn snapshot(&self, log_lines: usize) -> Vec<ComponentState> {
        let mut slots = self.slots.lock().await;
        self.components
            .iter()
            .map(|c| {
                let s = slots.get_mut(c.id).expect("slot per component");
                s.reap();
                let (bin_path, bin_found) = self.resolve_bin(c.bin);
                ComponentState {
                    id: c.id,
                    label: c.label,
                    summary: c.summary,
                    kind: c.kind,
                    bin: c.bin,
                    bin_path: bin_path.display().to_string(),
                    bin_found,
                    flags: c.flags.to_vec(),
                    takes_session: c.takes_session,
                    running: s.child.is_some(),
                    pid: s.pid,
                    since_unix_s: s.since,
                    last_exit: s.last_exit,
                    log: s.log.lock().unwrap_or_else(|p| p.into_inner()).tail(log_lines),
                }
            })
            .collect()
    }

    /// Build the argument vector for a start request, or say why not.
    pub fn arguments(&self, c: &Component, req: &StartRequest) -> Result<Vec<String>, StartError> {
        let mut args: Vec<String> = c.base_args.iter().map(|s| s.to_string()).collect();
        if c.takes_session {
            let name = req.session.as_deref().ok_or(StartError::SessionRequired)?;
            let p = self.validate_session(name).map_err(StartError::BadSession)?;
            args.push(p.display().to_string());
        }
        if c.passes_config {
            args.push("--config".into());
            args.push(self.config_path.display().to_string());
        }
        for f in &req.flags {
            if !c.flags.iter().any(|a| a.flag == f) {
                return Err(StartError::FlagNotAllowed(f.clone()));
            }
            if !args.iter().any(|a| a == f) {
                args.push(f.clone());
            }
        }
        Ok(args)
    }

    pub async fn start(&self, id: &str, req: &StartRequest) -> Result<u32, StartError> {
        let c = self.component(id).ok_or(StartError::UnknownComponent)?;
        let args = self.arguments(c, req)?;
        let (bin, _) = self.resolve_bin(c.bin);

        let mut slots = self.slots.lock().await;
        let slot = slots.get_mut(c.id).expect("slot per component");
        if !slot.reap() {
            return Err(StartError::AlreadyRunning);
        }

        let mut cmd = Command::new(&bin);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);

        let mut child = cmd.spawn().map_err(|e| {
            StartError::Spawn(format!("{} {}: {e}", bin.display(), args.join(" ")))
        })?;
        let pid = child.id().unwrap_or(0);
        {
            let mut log = slot.log.lock().unwrap_or_else(|p| p.into_inner());
            log.push(format!("--- started pid {pid}: {} {} ---", bin.display(), args.join(" ")));
        }
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, slot.log.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, slot.log.clone()));
        }
        slot.child = Some(child);
        slot.pid = Some(pid);
        slot.since = Some(now_unix());
        info!(component = c.id, pid, bin = %bin.display(), "started");
        Ok(pid)
    }

    /// Stop a component: Ctrl-Break, wait up to the grace period, then
    /// terminate. `force` skips straight to terminating.
    pub async fn stop(&self, id: &str, force: bool) -> Result<StopOutcome, StopError> {
        let c = self.component(id).ok_or(StopError::UnknownComponent)?;
        let pid = {
            let mut slots = self.slots.lock().await;
            let slot = slots.get_mut(c.id).expect("slot per component");
            if slot.reap() {
                return Err(StopError::NotRunning);
            }
            slot.pid.unwrap_or(0)
        };

        if !force && send_ctrl_break(pid) {
            let deadline = tokio::time::Instant::now() + self.grace;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut slots = self.slots.lock().await;
                if slots.get_mut(c.id).expect("slot per component").reap() {
                    info!(component = c.id, pid, "stopped gracefully");
                    return Ok(StopOutcome::Graceful);
                }
            }
            warn!(component = c.id, pid, grace_s = self.grace.as_secs(), "did not stop in time; terminating");
        }

        let mut slots = self.slots.lock().await;
        let slot = slots.get_mut(c.id).expect("slot per component");
        if let Some(child) = slot.child.as_mut() {
            if let Err(e) = child.kill().await {
                warn!(component = c.id, pid, error = %e, "kill failed");
            }
        }
        slot.reap();
        info!(component = c.id, pid, "terminated");
        Ok(StopOutcome::Terminated)
    }

    /// Shutdown: stop every running component, gracefully where possible.
    pub async fn stop_all(&self) {
        let ids: Vec<&'static str> = self.components.iter().map(|c| c.id).collect();
        for id in ids {
            let _ = self.stop(id, false).await;
        }
    }
}

/// Copy one child stream, line by line, into its component's log.
async fn pump<R: AsyncRead + Unpin>(r: R, log: Arc<Mutex<LogRing>>) {
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log.lock().unwrap_or_else(|p| p.into_inner()).push(line);
    }
}

fn which(file: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(file).is_file()))
        .unwrap_or(false)
}

#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[cfg(windows)]
unsafe extern "system" {
    fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
}

/// Ask a child to shut down the way Ctrl-C would. Returns false when the
/// event could not be delivered (no console, or the child is gone), in which
/// case the caller falls back to terminating.
#[cfg(windows)]
fn send_ctrl_break(pid: u32) -> bool {
    const CTRL_BREAK_EVENT: u32 = 1;
    // SAFETY: plain FFI call with two integers; no pointers, no lifetimes.
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
}

#[cfg(not(windows))]
fn send_ctrl_break(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long-running, dependency-free stand-in for a service.
    const SLEEPER: Component = Component {
        id: "sleeper",
        label: "Sleeper",
        summary: "test service",
        #[cfg(windows)]
        bin: "cmd",
        #[cfg(not(windows))]
        bin: "sh",
        #[cfg(windows)]
        base_args: &["/C", "echo hello from child && ping -n 60 127.0.0.1 > NUL"],
        #[cfg(not(windows))]
        base_args: &["-c", "echo hello from child; sleep 60"],
        kind: Kind::Service,
        flags: &[Flag { flag: "--ok", help: "allowed" }],
        takes_session: false,
        passes_config: false,
    };

    const REPORTER: Component = Component {
        id: "rep",
        label: "Reporter",
        summary: "needs a session",
        bin: "does-not-matter",
        base_args: &["report"],
        kind: Kind::Task,
        flags: &[],
        takes_session: true,
        passes_config: false,
    };

    fn manager(dir: &Path) -> Manager {
        Manager::new(
            &[SLEEPER, REPORTER],
            None,
            PathBuf::from("telemouse.toml"),
            dir.to_path_buf(),
            Duration::from_secs(2),
        )
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("telemouse-ctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn start_reports_running_captures_output_and_stop_terminates() {
        let m = manager(Path::new("."));
        let pid = m.start("sleeper", &StartRequest::default()).await.unwrap();
        assert!(pid > 0);

        let st = m.snapshot(50).await;
        let s = st.iter().find(|s| s.id == "sleeper").unwrap();
        assert!(s.running);
        assert_eq!(s.pid, Some(pid));
        assert!(s.since_unix_s.is_some());
        assert!(s.log[0].starts_with("--- started pid"));

        assert_eq!(
            m.start("sleeper", &StartRequest::default()).await.unwrap_err(),
            StartError::AlreadyRunning
        );

        // The child's stdout lands in the log.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let st = m.snapshot(50).await;
            let s = st.iter().find(|s| s.id == "sleeper").unwrap();
            if s.log.iter().any(|l| l.contains("hello from child")) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no output captured: {:?}", s.log);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let outcome = m.stop("sleeper", true).await.unwrap();
        assert_eq!(outcome, StopOutcome::Terminated);
        let st = m.snapshot(50).await;
        let s = st.iter().find(|s| s.id == "sleeper").unwrap();
        assert!(!s.running);
        assert!(s.pid.is_none());
        assert!(s.last_exit.is_some());
        assert!(s.log.iter().any(|l| l.starts_with("--- exited:")));
        assert_eq!(m.stop("sleeper", true).await.unwrap_err(), StopError::NotRunning);
    }

    #[tokio::test]
    async fn graceful_stop_always_ends_the_child() {
        let m = manager(Path::new("."));
        m.start("sleeper", &StartRequest::default()).await.unwrap();
        let t0 = tokio::time::Instant::now();
        // Whether the Ctrl-Break is honoured or the grace period runs out,
        // the child is gone afterwards and it took no longer than grace+slack.
        let outcome = m.stop("sleeper", false).await.unwrap();
        assert!(matches!(outcome, StopOutcome::Graceful | StopOutcome::Terminated));
        assert!(t0.elapsed() < Duration::from_secs(4));
        let st = m.snapshot(5).await;
        assert!(!st.iter().find(|s| s.id == "sleeper").unwrap().running);
    }

    #[tokio::test]
    async fn stop_all_is_quiet_on_an_idle_manager() {
        let m = manager(Path::new("."));
        m.stop_all().await;
        assert!(m.snapshot(1).await.iter().all(|s| !s.running));
    }

    #[test]
    fn arguments_enforce_the_allow_list_and_the_session_rules() {
        let dir = tmpdir("args");
        std::fs::write(dir.join("s-1.jsonl"), "{}\n").unwrap();
        let m = manager(&dir);

        let ok = m
            .arguments(&SLEEPER, &StartRequest { flags: vec!["--ok".into(), "--ok".into()], session: None })
            .unwrap();
        assert_eq!(ok.last(), Some(&"--ok".to_string()));
        assert_eq!(ok.iter().filter(|a| *a == "--ok").count(), 1, "flags are deduplicated");

        assert_eq!(
            m.arguments(&SLEEPER, &StartRequest { flags: vec!["--evil".into()], session: None })
                .unwrap_err(),
            StartError::FlagNotAllowed("--evil".into())
        );
        assert_eq!(
            m.arguments(&REPORTER, &StartRequest::default()).unwrap_err(),
            StartError::SessionRequired
        );
        for bad in ["../x.jsonl", "sub/x.jsonl", "sub\\x.jsonl", "notes.txt", "missing.jsonl", ""] {
            let r = m.arguments(&REPORTER, &StartRequest { flags: vec![], session: Some(bad.into()) });
            assert!(matches!(r, Err(StartError::BadSession(_))), "{bad:?} → {r:?}");
        }
        let ok = m
            .arguments(&REPORTER, &StartRequest { flags: vec![], session: Some("s-1.jsonl".into()) })
            .unwrap();
        assert_eq!(ok[0], "report");
        assert!(ok[1].ends_with("s-1.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_component_and_missing_binary_are_errors_not_panics() {
        let m = manager(Path::new("."));
        assert_eq!(
            m.start("nope", &StartRequest::default()).await.unwrap_err(),
            StartError::UnknownComponent
        );
        assert_eq!(m.stop("nope", true).await.unwrap_err(), StopError::UnknownComponent);

        let dir = tmpdir("missing");
        std::fs::write(dir.join("s.jsonl"), "{}\n").unwrap();
        let m = manager(&dir);
        let r = m
            .start("rep", &StartRequest { flags: vec![], session: Some("s.jsonl".into()) })
            .await;
        assert!(matches!(r, Err(StartError::Spawn(_))), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_are_listed_newest_first_and_only_jsonl() {
        let dir = tmpdir("list");
        std::fs::write(dir.join("old.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(dir.join("new.jsonl"), "{}\n").unwrap();
        let m = manager(&dir);
        assert_eq!(m.list_sessions(), vec!["new.jsonl".to_string(), "old.jsonl".to_string()]);
        assert!(manager(&dir.join("absent")).list_sessions().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_bin_prefers_the_configured_dir() {
        let dir = tmpdir("bin");
        let m = Manager::new(
            &[SLEEPER],
            Some(dir.clone()),
            PathBuf::from("telemouse.toml"),
            PathBuf::from("."),
            Duration::from_secs(1),
        );
        let (p, found) = m.resolve_bin("telemouse-viz");
        assert_eq!(p, dir.join(format!("telemouse-viz{}", std::env::consts::EXE_SUFFIX)));
        assert!(!found);
        std::fs::write(&p, "").unwrap();
        assert!(m.resolve_bin("telemouse-viz").1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_ring_is_bounded() {
        let mut r = LogRing::default();
        for i in 0..(LOG_CAPACITY + 10) {
            r.push(i.to_string());
        }
        assert_eq!(r.lines.len(), LOG_CAPACITY);
        assert_eq!(r.tail(2), vec![(LOG_CAPACITY + 8).to_string(), (LOG_CAPACITY + 9).to_string()]);
    }

    #[test]
    fn catalogue_ids_are_unique_and_binaries_are_workspace_names() {
        let mut ids: Vec<&str> = COMPONENTS.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), COMPONENTS.len());
        for c in COMPONENTS {
            assert!(crate::procs::classify(c.bin, "").is_some(), "{} is not a telemouse binary", c.bin);
        }
    }
}
