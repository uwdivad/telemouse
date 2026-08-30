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
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tracing::{info, warn};

/// Lines of child output kept per component (across restarts).
const LOG_CAPACITY: usize = 400;

/// A log file this large at startup is rotated to `<name>.1` (the previous
/// `.1` is dropped). One panel session of chatty children is well under this.
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Open `<dir>/<name>.log` for appending, rotating it first if it has grown
/// past [`LOG_ROTATE_BYTES`]. Creates the directory.
pub fn open_log(dir: &Path, name: &str) -> std::io::Result<File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.log"));
    if let Ok(md) = std::fs::metadata(&path)
        && md.len() > LOG_ROTATE_BYTES
    {
        let _ = std::fs::rename(&path, dir.join(format!("{name}.log.1")));
    }
    File::options().create(true).append(true).open(path)
}

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
// rustfmt would spread every `Flag` over four lines; one row per flag reads
// as the table it is.
#[rustfmt::skip]
pub const COMPONENTS: &[Component] = &[
    Component {
        id: "capture",
        label: "Capture agent",
        summary: "telemouse run — raw mouse input → UDP / recording / Kafka",
        bin: "telemouse",
        base_args: &["run"],
        kind: Kind::Service,
        flags: &[
            Flag { flag: "--print",     help: "log a one-line summary for every batch" },
            Flag { flag: "--no-kafka",  help: "disable the Kafka sink" },
            Flag { flag: "--no-udp",    help: "disable the localhost UDP sink (live viz)" },
            Flag { flag: "--no-record", help: "do not save the JSONL recording, whatever telemouse.toml says" },
            Flag { flag: "--record",    help: "save the JSONL recording, whatever telemouse.toml says" },
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
        flags: &[Flag {
            flag: "--timing",
            help: "print the per-phase timing table",
        }],
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
    /// Capture only: save a recording this run? `None` = whatever
    /// `recording.enabled` says. The panel turns this into `--record` /
    /// `--no-record` itself (see [`recording_flags`]) so the page and the
    /// tray cannot disagree with it, or with a config that changed since
    /// the page loaded.
    #[serde(default)]
    pub save: Option<bool>,
}

/// How a Windows process reports "left on Ctrl-Break / Ctrl-C": the exit
/// code is `STATUS_CONTROL_C_EXIT`, which is what a gracefully stopped
/// service shows.
pub const STATUS_CONTROL_C_EXIT: i32 = -1073741510; // 0xC000013A

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExitInfo {
    /// `None` when killed by a signal (Unix) — on Windows always a code.
    pub code: Option<i32>,
    pub at_unix_s: u64,
    /// `code == STATUS_CONTROL_C_EXIT`, decided here so the page does not
    /// carry the number.
    pub ctrl_break: bool,
}

impl ExitInfo {
    pub fn new(code: Option<i32>, at_unix_s: u64) -> Self {
        Self {
            code,
            at_unix_s,
            ctrl_break: code == Some(STATUS_CONTROL_C_EXIT),
        }
    }
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
    /// Exits seen since the panel started.
    pub exits: u32,
    /// Of those, the ones nobody asked for: a service that died without a
    /// stop, or a task that finished with a non-zero code.
    pub unexpected_exits: u32,
    /// Arguments of the current (or last) run, after the base arguments.
    pub args: Vec<String>,
    /// Capture only: is this run writing a JSONL recording? False for
    /// everything else and whenever not running.
    pub saving: bool,
    pub log: Vec<String>,
}

/// Whether recording is on by default (`recording.enabled`) and where it
/// goes — the panel's single "save data" switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingInfo {
    pub enabled: bool,
    pub dir: String,
}

/// Does a capture run with these arguments save a recording? The CLI
/// switches beat the config; they are mutually exclusive on the agent.
pub fn recording_saves(config_enabled: bool, args: &[String]) -> bool {
    if args.iter().any(|a| a == "--no-record") {
        false
    } else if args.iter().any(|a| a == "--record") {
        true
    } else {
        config_enabled
    }
}

/// The flags that make a capture run save (or not), given the config
/// default: nothing when the default already agrees.
pub fn recording_flags(config_enabled: bool, save: bool) -> Vec<String> {
    match (config_enabled, save) {
        (true, false) => vec!["--no-record".into()],
        (false, true) => vec!["--record".into()],
        _ => Vec::new(),
    }
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

/// Bounded line buffer for one component's output.
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

/// Where one component's output goes: the ring the page and tray read, and
/// — when a log directory is configured — `<log_dir>/<id>.log`, so what a
/// child printed survives the panel being restarted.
#[derive(Default)]
pub struct LogSink {
    ring: Mutex<LogRing>,
    file: Mutex<Option<File>>,
    /// A failed write is reported once, not once per line.
    write_failed: AtomicBool,
}

impl LogSink {
    fn push(&self, line: String) {
        if let Some(f) = self.file.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
            && let Err(e) = writeln!(f, "{line}")
            && !self.write_failed.swap(true, Ordering::Relaxed)
        {
            warn!(error = %e, "component log file write failed; further failures are not reported");
        }
        self.ring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(line);
    }

    fn tail(&self, n: usize) -> Vec<String> {
        self.ring.lock().unwrap_or_else(|p| p.into_inner()).tail(n)
    }

    /// Attach the file on first use. Opening lazily means an unwritable log
    /// directory costs a warning at the first start, not a refusal to serve.
    fn ensure_file(&self, dir: &Path, id: &str) {
        let mut file = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if file.is_some() {
            return;
        }
        match open_log(dir, id) {
            Ok(f) => *file = Some(f),
            Err(e) => {
                if !self.write_failed.swap(true, Ordering::Relaxed) {
                    warn!(component = id, dir = %dir.display(), error = %e, "cannot open component log file; output is kept in memory only");
                }
            }
        }
    }
}

struct Slot {
    child: Option<Child>,
    pid: Option<u32>,
    since: Option<u64>,
    last_exit: Option<ExitInfo>,
    /// Set by `stop`: the next exit was asked for.
    stopping: bool,
    exits: u32,
    unexpected_exits: u32,
    args: Vec<String>,
    log: Arc<LogSink>,
}

impl Slot {
    fn new() -> Self {
        Self {
            child: None,
            pid: None,
            since: None,
            last_exit: None,
            stopping: false,
            exits: 0,
            unexpected_exits: 0,
            args: Vec::new(),
            log: Arc::new(LogSink::default()),
        }
    }

    /// Record an exit if the child has one; returns whether it is gone.
    ///
    /// An exit nobody asked for is the one event an operator most needs to
    /// hear about and the one the page cannot show if it is not open, so it
    /// is a `warn!` with everything needed to go and look: which component,
    /// what it was running, how long it lasted, and how it ended.
    fn reap(&mut self, c: &Component) -> bool {
        let Some(child) = self.child.as_mut() else {
            return true;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                let exit = ExitInfo::new(status.code(), now_unix());
                let uptime_s = self
                    .since
                    .map(|s| exit.at_unix_s.saturating_sub(s))
                    .unwrap_or(0);
                let expected = self.stopping || (c.kind == Kind::Task && exit.code == Some(0));
                self.exits += 1;
                if expected {
                    info!(component = c.id, pid = self.pid, exit = %describe_exit(exit), uptime_s, "exited");
                } else {
                    self.unexpected_exits += 1;
                    warn!(
                        component = c.id,
                        pid = self.pid,
                        exit = %describe_exit(exit),
                        uptime_s,
                        args = %self.args.join(" "),
                        unexpected_exits = self.unexpected_exits,
                        "exited without being stopped"
                    );
                }
                self.log
                    .push(format!("--- exited: {} ---", describe_exit(exit)));
                self.last_exit = Some(exit);
                self.clear();
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!(component = c.id, pid = self.pid, error = %e, "try_wait failed; treating child as gone");
                self.clear();
                true
            }
        }
    }

    fn clear(&mut self) {
        self.child = None;
        self.pid = None;
        self.since = None;
        self.stopping = false;
    }
}

pub(crate) fn describe_exit(e: ExitInfo) -> String {
    match e.code {
        _ if e.ctrl_break => "Ctrl-Break".into(),
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

/// Everything a [`Manager`] needs to know about its surroundings.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Where the binaries are; `None` = next to this executable, then `PATH`.
    pub bin_dir: Option<PathBuf>,
    /// The `telemouse.toml` handed to every child that takes one.
    pub config_path: PathBuf,
    pub recordings_dir: PathBuf,
    /// `recording.enabled` from that config.
    pub recording_enabled: bool,
    /// How long a graceful stop may take before the child is terminated.
    pub grace: Duration,
    /// Where child output is also written, one `<id>.log` per component.
    /// `None` keeps it in memory only.
    pub log_dir: Option<PathBuf>,
}

pub struct Manager {
    components: Vec<Component>,
    cfg: ManagerConfig,
    slots: tokio::sync::Mutex<HashMap<&'static str, Slot>>,
}

impl Manager {
    pub fn new(components: &[Component], cfg: ManagerConfig) -> Self {
        let slots = components.iter().map(|c| (c.id, Slot::new())).collect();
        Self {
            components: components.to_vec(),
            cfg,
            slots: tokio::sync::Mutex::new(slots),
        }
    }

    pub fn recording(&self) -> RecordingInfo {
        RecordingInfo {
            enabled: self.cfg.recording_enabled,
            dir: self.cfg.recordings_dir.display().to_string(),
        }
    }

    fn component(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.id == id)
    }

    /// Where a component's binary is expected: `bin_dir` if configured, else
    /// next to this executable, else bare name (PATH lookup).
    pub fn resolve_bin(&self, bin: &str) -> (PathBuf, bool) {
        let file = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
        if let Some(dir) = &self.cfg.bin_dir {
            let p = dir.join(&file);
            let found = p.is_file();
            return (p, found);
        }
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf))
        {
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
        let p = self.cfg.recordings_dir.join(name);
        if !p.is_file() {
            return Err(format!("no such recording: {name}"));
        }
        Ok(p)
    }

    /// `*.jsonl` files in the recordings directory, newest first.
    pub fn list_sessions(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.cfg.recordings_dir) else {
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
        for c in &self.components {
            slots.get_mut(c.id).expect("slot per component").reap(c);
        }
    }

    pub async fn snapshot(&self, log_lines: usize) -> Vec<ComponentState> {
        let mut slots = self.slots.lock().await;
        self.components
            .iter()
            .map(|c| {
                let s = slots.get_mut(c.id).expect("slot per component");
                s.reap(c);
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
                    exits: s.exits,
                    unexpected_exits: s.unexpected_exits,
                    args: s.args.clone(),
                    saving: s.child.is_some()
                        && c.id == "capture"
                        && recording_saves(self.cfg.recording_enabled, &s.args),
                    log: s.log.tail(log_lines),
                }
            })
            .collect()
    }

    /// Build the argument vector for a start request, or say why not.
    pub fn arguments(&self, c: &Component, req: &StartRequest) -> Result<Vec<String>, StartError> {
        let mut args: Vec<String> = c.base_args.iter().map(|s| s.to_string()).collect();
        if c.takes_session {
            let name = req.session.as_deref().ok_or(StartError::SessionRequired)?;
            let p = self
                .validate_session(name)
                .map_err(StartError::BadSession)?;
            args.push(p.display().to_string());
        }
        if c.passes_config {
            args.push("--config".into());
            args.push(self.cfg.config_path.display().to_string());
        }
        // `save` becomes a flag here and then goes through the same allow-list
        // as any other, so a component without the recording switch refuses it
        // the way it refuses the raw flag.
        let save_flags = req
            .save
            .map(|save| recording_flags(self.cfg.recording_enabled, save))
            .unwrap_or_default();
        for f in req.flags.iter().chain(&save_flags) {
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
        if !slot.reap(c) {
            return Err(StartError::AlreadyRunning);
        }
        if let Some(dir) = &self.cfg.log_dir {
            slot.log.ensure_file(dir, c.id);
        }

        let mut cmd = Command::new(&bin);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);

        let mut child = cmd
            .spawn()
            .map_err(|e| StartError::Spawn(format!("{} {}: {e}", bin.display(), args.join(" "))))?;
        let pid = child.id().unwrap_or(0);
        slot.log.push(format!(
            "--- started pid {pid}: {} {} ---",
            bin.display(),
            args.join(" ")
        ));
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, slot.log.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, slot.log.clone()));
        }
        slot.child = Some(child);
        slot.pid = Some(pid);
        slot.since = Some(now_unix());
        slot.args = args;
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
            if slot.reap(c) {
                return Err(StopError::NotRunning);
            }
            slot.stopping = true;
            slot.pid
        };

        // A pid of 0 would address the whole console group — this panel
        // included — so a child whose pid was never known is only terminated.
        if !force && pid.is_some_and(send_ctrl_break) {
            let deadline = tokio::time::Instant::now() + self.cfg.grace;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut slots = self.slots.lock().await;
                if slots.get_mut(c.id).expect("slot per component").reap(c) {
                    info!(component = c.id, pid, "stopped gracefully");
                    return Ok(StopOutcome::Graceful);
                }
            }
            warn!(
                component = c.id,
                pid,
                grace_s = self.cfg.grace.as_secs(),
                "did not stop in time; terminating"
            );
        }

        let mut slots = self.slots.lock().await;
        let slot = slots.get_mut(c.id).expect("slot per component");
        if let Some(child) = slot.child.as_mut()
            && let Err(e) = child.kill().await
        {
            warn!(component = c.id, pid, error = %e, "kill failed");
        }
        slot.reap(c);
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
async fn pump<R: AsyncRead + Unpin>(r: R, log: Arc<LogSink>) {
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        log.push(line);
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
        flags: &[Flag {
            flag: "--ok",
            help: "allowed",
        }],
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

    /// A task that fails: exits 3 without being asked to.
    const FAILER: Component = Component {
        id: "failer",
        label: "Failer",
        summary: "test task that exits 3",
        #[cfg(windows)]
        bin: "cmd",
        #[cfg(not(windows))]
        bin: "sh",
        #[cfg(windows)]
        base_args: &["/C", "echo about to fail && exit 3"],
        #[cfg(not(windows))]
        base_args: &["-c", "echo about to fail; exit 3"],
        kind: Kind::Task,
        flags: &[],
        takes_session: false,
        passes_config: false,
    };

    fn config(dir: &Path) -> ManagerConfig {
        ManagerConfig {
            bin_dir: None,
            config_path: PathBuf::from("telemouse.toml"),
            recordings_dir: dir.to_path_buf(),
            recording_enabled: true,
            grace: Duration::from_secs(2),
            log_dir: None,
        }
    }

    fn manager(dir: &Path) -> Manager {
        Manager::new(&[SLEEPER, REPORTER, FAILER], config(dir))
    }

    /// Poll until the component is no longer running (or the deadline).
    async fn wait_exit(m: &Manager, id: &str) -> ComponentState {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let st = m.snapshot(50).await;
            let s = st.into_iter().find(|s| s.id == id).unwrap();
            if !s.running {
                return s;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{id} still running: {:?}",
                s.log
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn exits_are_counted_and_classified() {
        let m = manager(Path::new("."));
        // A task that fails on its own: unexpected.
        m.start("failer", &StartRequest::default()).await.unwrap();
        let s = wait_exit(&m, "failer").await;
        assert_eq!(s.last_exit.map(|e| e.code), Some(Some(3)));
        assert_eq!((s.exits, s.unexpected_exits), (1, 1));
        assert!(s.log.iter().any(|l| l.contains("about to fail")));

        // A service that is stopped: expected, whatever the exit code.
        m.start("sleeper", &StartRequest::default()).await.unwrap();
        m.stop("sleeper", true).await.unwrap();
        let s = wait_exit(&m, "sleeper").await;
        assert_eq!((s.exits, s.unexpected_exits), (1, 0));

        // Counts accumulate across runs and `stopping` does not leak into
        // the next run.
        m.start("failer", &StartRequest::default()).await.unwrap();
        let s = wait_exit(&m, "failer").await;
        assert_eq!((s.exits, s.unexpected_exits), (2, 2));
    }

    #[tokio::test]
    async fn child_output_is_also_written_to_a_log_file() {
        let dir = tmpdir("logs");
        let mut cfg = config(Path::new("."));
        cfg.log_dir = Some(dir.join("nested"));
        let m = Manager::new(&[FAILER], cfg);
        m.start("failer", &StartRequest::default()).await.unwrap();
        wait_exit(&m, "failer").await;
        // The pumps finish a beat after try_wait sees the exit.
        let path = dir.join("nested").join("failer.log");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let text = loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains("about to fail") && text.contains("--- exited: code 3 ---") {
                break text;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "log file incomplete: {text:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(text.starts_with("--- started pid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_log_rotates_a_large_file() {
        let dir = tmpdir("rotate");
        let path = dir.join("x.log");
        let f = File::create(&path).unwrap();
        f.set_len(LOG_ROTATE_BYTES + 1).unwrap();
        drop(f);
        let mut f = open_log(&dir, "x").unwrap();
        writeln!(f, "fresh").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        assert_eq!(
            std::fs::metadata(dir.join("x.log.1")).unwrap().len(),
            LOG_ROTATE_BYTES + 1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_switch_is_cli_over_config() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(recording_saves(true, &s(&["run", "--print"])));
        assert!(!recording_saves(false, &s(&["run"])));
        assert!(!recording_saves(true, &s(&["run", "--no-record"])));
        assert!(recording_saves(false, &s(&["run", "--record"])));
        assert_eq!(recording_flags(true, true), Vec::<String>::new());
        assert_eq!(recording_flags(false, false), Vec::<String>::new());
        assert_eq!(recording_flags(true, false), s(&["--no-record"]));
        assert_eq!(recording_flags(false, true), s(&["--record"]));
        let m = manager(Path::new("."));
        assert_eq!(
            m.recording(),
            RecordingInfo {
                enabled: true,
                dir: ".".into()
            }
        );
    }

    #[tokio::test]
    async fn snapshot_reports_the_arguments_of_a_run() {
        let m = manager(Path::new("."));
        m.start(
            "sleeper",
            &StartRequest {
                flags: vec!["--ok".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let st = m.snapshot(1).await;
        let s = st.iter().find(|s| s.id == "sleeper").unwrap();
        assert_eq!(s.args.last().map(String::as_str), Some("--ok"));
        assert!(!s.saving, "only the capture component saves");
        m.stop("sleeper", true).await.unwrap();
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
            m.start("sleeper", &StartRequest::default())
                .await
                .unwrap_err(),
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
            assert!(
                tokio::time::Instant::now() < deadline,
                "no output captured: {:?}",
                s.log
            );
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
        assert_eq!(
            m.stop("sleeper", true).await.unwrap_err(),
            StopError::NotRunning
        );
    }

    #[tokio::test]
    async fn graceful_stop_always_ends_the_child() {
        let m = manager(Path::new("."));
        m.start("sleeper", &StartRequest::default()).await.unwrap();
        let t0 = tokio::time::Instant::now();
        // Whether the Ctrl-Break is honoured or the grace period runs out,
        // the child is gone afterwards and it took no longer than grace+slack.
        let outcome = m.stop("sleeper", false).await.unwrap();
        assert!(matches!(
            outcome,
            StopOutcome::Graceful | StopOutcome::Terminated
        ));
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
            .arguments(
                &SLEEPER,
                &StartRequest {
                    flags: vec!["--ok".into(), "--ok".into()],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ok.last(), Some(&"--ok".to_string()));
        assert_eq!(
            ok.iter().filter(|a| *a == "--ok").count(),
            1,
            "flags are deduplicated"
        );

        assert_eq!(
            m.arguments(
                &SLEEPER,
                &StartRequest {
                    flags: vec!["--evil".into()],
                    ..Default::default()
                }
            )
            .unwrap_err(),
            StartError::FlagNotAllowed("--evil".into())
        );
        assert_eq!(
            m.arguments(&REPORTER, &StartRequest::default())
                .unwrap_err(),
            StartError::SessionRequired
        );
        for bad in [
            "../x.jsonl",
            "sub/x.jsonl",
            "sub\\x.jsonl",
            "notes.txt",
            "missing.jsonl",
            "",
        ] {
            let r = m.arguments(
                &REPORTER,
                &StartRequest {
                    session: Some(bad.into()),
                    ..Default::default()
                },
            );
            assert!(
                matches!(r, Err(StartError::BadSession(_))),
                "{bad:?} → {r:?}"
            );
        }
        let ok = m
            .arguments(
                &REPORTER,
                &StartRequest {
                    session: Some("s-1.jsonl".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(ok[0], "report");
        assert!(ok[1].ends_with("s-1.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The save switch is resolved against the config the panel holds, not
    /// whatever the page believed when it loaded.
    #[test]
    fn save_is_resolved_server_side() {
        let capture = &COMPONENTS[0];
        assert_eq!(capture.id, "capture");
        let has = |args: &[String], f: &str| args.iter().any(|a| a == f);

        let on = Manager::new(std::slice::from_ref(capture), config(Path::new(".")));
        let a = on
            .arguments(
                capture,
                &StartRequest {
                    save: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(has(&a, "--no-record") && !has(&a, "--record"));
        let a = on
            .arguments(
                capture,
                &StartRequest {
                    save: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            !has(&a, "--no-record") && !has(&a, "--record"),
            "agrees with the default: no flag"
        );
        let a = on.arguments(capture, &StartRequest::default()).unwrap();
        assert!(!has(&a, "--no-record") && !has(&a, "--record"));

        let off = Manager::new(
            std::slice::from_ref(capture),
            ManagerConfig {
                recording_enabled: false,
                ..config(Path::new("."))
            },
        );
        let a = off
            .arguments(
                capture,
                &StartRequest {
                    save: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(has(&a, "--record") && !has(&a, "--no-record"));
        assert!(recording_saves(false, &a));

        // A raw flag and the switch that says the same thing do not double up.
        let a = on
            .arguments(
                capture,
                &StartRequest {
                    flags: vec!["--no-record".into()],
                    save: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(a.iter().filter(|x| *x == "--no-record").count(), 1);

        // Components without the switch refuse it like any other flag.
        assert_eq!(
            on.arguments(
                &SLEEPER,
                &StartRequest {
                    save: Some(false),
                    ..Default::default()
                }
            )
            .unwrap_err(),
            StartError::FlagNotAllowed("--no-record".into())
        );
    }

    #[tokio::test]
    async fn unknown_component_and_missing_binary_are_errors_not_panics() {
        let m = manager(Path::new("."));
        assert_eq!(
            m.start("nope", &StartRequest::default()).await.unwrap_err(),
            StartError::UnknownComponent
        );
        assert_eq!(
            m.stop("nope", true).await.unwrap_err(),
            StopError::UnknownComponent
        );

        let dir = tmpdir("missing");
        std::fs::write(dir.join("s.jsonl"), "{}\n").unwrap();
        let m = manager(&dir);
        let r = m
            .start(
                "rep",
                &StartRequest {
                    session: Some("s.jsonl".into()),
                    ..Default::default()
                },
            )
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
        assert_eq!(
            m.list_sessions(),
            vec!["new.jsonl".to_string(), "old.jsonl".to_string()]
        );
        assert!(manager(&dir.join("absent")).list_sessions().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_bin_prefers_the_configured_dir() {
        let dir = tmpdir("bin");
        let m = Manager::new(
            &[SLEEPER],
            ManagerConfig {
                bin_dir: Some(dir.clone()),
                ..config(Path::new("."))
            },
        );
        let (p, found) = m.resolve_bin("telemouse-viz");
        assert_eq!(
            p,
            dir.join(format!("telemouse-viz{}", std::env::consts::EXE_SUFFIX))
        );
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
        assert_eq!(
            r.tail(2),
            vec![
                (LOG_CAPACITY + 8).to_string(),
                (LOG_CAPACITY + 9).to_string()
            ]
        );
    }

    #[test]
    fn catalogue_ids_are_unique_and_binaries_are_workspace_names() {
        let mut ids: Vec<&str> = COMPONENTS.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), COMPONENTS.len());
        for c in COMPONENTS {
            assert!(
                crate::procs::classify(c.bin, "").is_some(),
                "{} is not a telemouse binary",
                c.bin
            );
        }
    }
}
