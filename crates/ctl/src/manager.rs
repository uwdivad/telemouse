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
//! send the event from), it is terminated. A shutdown Windows itself is
//! waiting on gets the same sequence with the grace clamped to
//! [`FAST_STOP_GRACE`], because the budget there is a handful of seconds and
//! a terminated child still beats a killed panel.
//!
//! Child output goes three ways: an in-memory ring the page and the tray
//! read (always), `<log_dir>/<id>.log` written by a dedicated thread per
//! component (`logging`), and — for the capture agent — the last `capture
//! stats` line parsed into numbers the card and the tray can show
//! (`observability`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use telemouse_core::config::AppConfig;
use telemouse_core::recordings::id_from_file_name;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tracing::{info, warn};

/// Lines of child output kept per component (across restarts).
const LOG_CAPACITY: usize = 400;

/// How long a binary lookup is reused. Resolving five components means five
/// `is_file` calls (and a PATH walk per missing one) on every snapshot,
/// twice a second; a build finishing while the panel runs is visible within
/// this, and anything the panel does itself drops the cache at once.
const BIN_TTL: Duration = Duration::from_secs(5);

/// Ceiling on the grace period when Windows is waiting on us (a console
/// close, logoff, shutdown, or the tray window's `WM_QUERYENDSESSION`).
/// The system grants a handful of seconds before it kills the process, so a
/// child that has not left by then is terminated rather than gambling with
/// the panel's own teardown.
pub const FAST_STOP_GRACE: Duration = Duration::from_secs(3);

/// Below this, a recording is one long session away from filling the disk.
#[cfg(feature = "observability")]
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How long a disk-free / recording-size reading is reused.
#[cfg(feature = "observability")]
const DISK_TTL: Duration = Duration::from_secs(5);

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExitInfo {
    /// `None` when killed by a signal (Unix) — on Windows always a code.
    pub code: Option<i32>,
    pub at_unix_s: u64,
    /// `code == STATUS_CONTROL_C_EXIT`, decided here so the page does not
    /// carry the number.
    pub ctrl_break: bool,
    /// What an unexpected exit most likely means, when the child's last
    /// lines say so (see [`exit_hint`]). Shown next to the exit code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The child's last non-empty line, kept whenever it ended on a
    /// non-zero code: the cause is usually written there and nowhere else
    /// the operator will look.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_line: Option<String>,
}

impl ExitInfo {
    pub fn new(code: Option<i32>, at_unix_s: u64) -> Self {
        Self {
            code,
            at_unix_s,
            ctrl_break: code == Some(STATUS_CONTROL_C_EXIT),
            hint: None,
            last_line: None,
        }
    }

    pub fn with_hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    pub fn with_last_line(mut self, line: Option<String>) -> Self {
        self.last_line = line;
        self
    }

    /// Did this exit fail? Ctrl-Break is how a graceful stop looks.
    pub fn failed(&self) -> bool {
        !self.ctrl_break && self.code != Some(0)
    }
}

/// A binary built before a key was added to `telemouse.toml`: every config
/// struct rejects unknown keys, so it exits the moment it loads the file.
/// Without this the symptom is "capture exited: code 1" and the cause is one
/// line in a log nobody may open.
pub const HINT_STALE_BINARY: &str = "telemouse.toml has a key this binary does not know: rebuild the workspace (cargo build --release --workspace)";

/// `WSAEADDRINUSE`. The viz server and the panel both bind a port; the
/// usual cause is a second instance, the other is a port something else
/// took.
pub const HINT_PORT_IN_USE: &str =
    "port already in use: another instance is running, or change the address in telemouse.toml";

/// `ERROR_ACCESS_DENIED`. Raw input and process queries against an elevated
/// foreground application need the same elevation.
pub const HINT_ACCESS_DENIED: &str =
    "access denied: the target runs elevated; run the panel as administrator";

/// A hint for an unexpected exit, read off the child's last lines. The
/// order is most-specific first: a config rejection explains itself, so it
/// is echoed verbatim rather than summarised.
pub fn exit_hint(log_tail: &[String]) -> Option<String> {
    if let Some(line) = log_tail
        .iter()
        .rev()
        .find(|l| l.contains("invalid config:"))
    {
        return Some(line.trim().to_string());
    }
    if log_tail.iter().any(|l| l.contains("unknown field")) {
        return Some(HINT_STALE_BINARY.to_string());
    }
    if log_tail.iter().any(|l| {
        l.contains("os error 10048") || l.contains("Only one usage of each socket address")
    }) {
        return Some(HINT_PORT_IN_USE.to_string());
    }
    if log_tail.iter().any(|l| l.contains("os error 5")) {
        return Some(HINT_ACCESS_DENIED.to_string());
    }
    None
}

/// `s` cut to `max` characters, with an ellipsis when something was
/// dropped. Used wherever a hint has to share a fixed-width column or a
/// 127-character tooltip with everything else.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    s.chars().take(keep).collect::<String>() + "…"
}

/// The one-glance form of a hint: its first clause, clipped. The long form
/// stays on the card, where there is room for the fix.
pub fn short_hint(hint: &str, max: usize) -> String {
    clip(hint.split(':').next().unwrap_or(hint).trim(), max)
}

/// Where a component's live numbers come from: the last `capture stats`
/// line, plus what the recording it names costs on disk.
#[cfg(feature = "observability")]
pub use crate::stats::{ChildStats, RecordingLive};

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
    /// Lines this component has ever printed. A client that passes
    /// `?log_since=<this>` is sent only what arrived since.
    pub log_seq: u64,
    /// The last `capture stats` line, parsed.
    #[cfg(feature = "observability")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<ChildStats>,
    /// The recording this run is writing, and what the disk has left.
    #[cfg(feature = "observability")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingLive>,
}

/// Whether recording is on by default (`recording.enabled`) and where it
/// goes — the panel's single "save data" switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordingInfo {
    pub enabled: bool,
    pub dir: String,
}

/// Where the config the panel runs on came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigStatus {
    /// Nothing was there; the panel wrote the shipped sample.
    Seeded,
    /// A file on disk was read.
    Loaded,
    /// No file, and none could be written: built-in defaults.
    Defaults,
}

/// The config file as `/api/state` reports it: where it is, whether it is
/// there, whether this start created it, and when it last changed — a
/// config edited while the panel runs is picked up, so the page has to be
/// able to say which one is in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigInfo {
    pub path: String,
    pub found: bool,
    pub seeded: bool,
    pub status: ConfigStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_unix_s: Option<u64>,
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

/// How long a stop may take: the configured grace, clamped to
/// [`FAST_STOP_GRACE`] when the operating system is the one waiting.
pub fn stop_grace(configured: Duration, fast: bool) -> Duration {
    if fast {
        configured.min(FAST_STOP_GRACE)
    } else {
        configured
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

/// Bounded line buffer for one component's output, plus the count of every
/// line it has ever held — the cursor `?log_since=` is expressed in.
#[derive(Default)]
pub struct LogRing {
    lines: VecDeque<String>,
    /// Lines ever pushed. The line at index `i` has ordinal
    /// `seq - lines.len() + i`.
    seq: u64,
}

impl LogRing {
    fn push(&mut self, line: String) {
        if self.lines.len() == LOG_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
        self.seq += 1;
    }

    fn tail(&self, n: usize) -> Vec<String> {
        self.lines.iter().rev().take(n).rev().cloned().collect()
    }

    /// The lines a client that has seen `since` of them has not: at most
    /// `max`, and everything still held when it has fallen too far behind.
    fn since(&self, since: u64, max: usize) -> Vec<String> {
        let first = self.seq - self.lines.len() as u64;
        let skip = since.saturating_sub(first) as usize;
        if skip >= self.lines.len() {
            return Vec::new();
        }
        let start = skip.max(self.lines.len().saturating_sub(max));
        self.lines.iter().skip(start).cloned().collect()
    }

    fn last_non_empty(&self) -> Option<String> {
        self.lines
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty() && !l.starts_with("--- "))
            .cloned()
    }
}

/// Where one component's output goes: the ring the page and tray read
/// (always), `<log_dir>/<id>.log` (`logging`), and the parsed tail of the
/// capture agent's stats line (`observability`).
///
/// The file is written by a thread of its own. A pump is a tokio task on a
/// two-worker runtime, and an unbuffered `writeln!` under a mutex per line
/// is exactly the blocking call a worker must not make: a chatty child
/// (`--print`) prints thousands of lines a second.
#[derive(Default)]
pub struct LogSink {
    ring: Mutex<LogRing>,
    #[cfg(feature = "logging")]
    file: Mutex<Option<std::sync::mpsc::Sender<String>>>,
    #[cfg(feature = "observability")]
    stats: Mutex<Option<ChildStats>>,
}

impl LogSink {
    fn push(&self, line: String) {
        // Old release binaries colourise even into a pipe (`NO_COLOR` is set
        // for the ones we launch, but a 0.1.0 exe predates it), and escape
        // bytes in the page and the log file are noise.
        let line = match telemouse_core::logging::strip_ansi(&line) {
            std::borrow::Cow::Borrowed(_) => line,
            std::borrow::Cow::Owned(clean) => clean,
        };

        #[cfg(feature = "observability")]
        if let Some(s) = crate::stats::parse_stats_line(&line) {
            *self.stats.lock().unwrap_or_else(|p| p.into_inner()) = Some(s);
        }

        #[cfg(feature = "logging")]
        {
            let slot = self.file.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(tx) = slot.as_ref() {
                // The writer thread only ends when this sender is dropped,
                // so a send error cannot happen in practice; if it ever
                // does, the ring still has the line.
                let _ = tx.send(line.clone());
            }
        }

        self.ring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(line);
    }

    fn tail(&self, n: usize) -> Vec<String> {
        self.ring.lock().unwrap_or_else(|p| p.into_inner()).tail(n)
    }

    /// `(lines, seq)`: everything since `since` (or the last `n` lines when
    /// the client has no cursor), and the cursor to send back next time.
    fn read(&self, n: usize, since: Option<u64>) -> (Vec<String>, u64) {
        let ring = self.ring.lock().unwrap_or_else(|p| p.into_inner());
        let lines = match since {
            Some(s) => ring.since(s, n),
            None => ring.tail(n),
        };
        (lines, ring.seq)
    }

    fn last_non_empty(&self) -> Option<String> {
        self.ring
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last_non_empty()
    }

    #[cfg(feature = "observability")]
    fn stats(&self) -> Option<ChildStats> {
        self.stats.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Attach the file on first use. Opening lazily means an unwritable log
    /// directory costs a warning at the first start, not a refusal to serve.
    #[cfg(feature = "logging")]
    fn ensure_file(&self, dir: &Path, id: &str) {
        use std::io::Write;

        let mut slot = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return;
        }
        let mut log = match telemouse_core::logging::LogFile::open(dir, id) {
            Ok(f) => f,
            Err(e) => {
                warn!(component = id, dir = %dir.display(), error = %e, "cannot open component log file; output is kept in memory only");
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let name = id.to_string();
        let spawned = std::thread::Builder::new()
            .name(format!("ctl-log-{id}"))
            .spawn(move || {
                let mut failed = false;
                // Coalesce whatever is already queued into one write, then
                // flush, so a burst costs one syscall and a quiet child's
                // line is on disk immediately.
                while let Ok(first) = rx.recv() {
                    let mut buf = first;
                    buf.push('\n');
                    while let Ok(next) = rx.try_recv() {
                        buf.push_str(&next);
                        buf.push('\n');
                    }
                    if let Err(e) = log.write_all(buf.as_bytes()).and_then(|()| log.flush())
                        && !std::mem::replace(&mut failed, true)
                    {
                        warn!(component = %name, error = %e, "component log file write failed; further failures are not reported");
                    }
                }
            });
        match spawned {
            Ok(_) => *slot = Some(tx),
            Err(e) => {
                warn!(component = id, error = %e, "could not start the component log writer; output is kept in memory only")
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
                let mut exit = ExitInfo::new(status.code(), now_unix());
                let uptime_s = self
                    .since
                    .map(|s| exit.at_unix_s.saturating_sub(s))
                    .unwrap_or(0);
                let expected = self.stopping || (c.kind == Kind::Task && exit.code == Some(0));
                self.exits += 1;
                if exit.failed() {
                    // The pumps may still be a beat behind the exit, but a
                    // config rejection is printed before anything else.
                    exit = exit
                        .with_hint(exit_hint(&self.log.tail(40)))
                        .with_last_line(self.log.last_non_empty());
                }
                if expected {
                    info!(component = c.id, pid = self.pid, exit = %describe_exit(&exit), uptime_s, "exited");
                } else {
                    self.unexpected_exits += 1;
                    warn!(
                        component = c.id,
                        pid = self.pid,
                        exit = %describe_exit(&exit),
                        uptime_s,
                        args = %self.args.join(" "),
                        unexpected_exits = self.unexpected_exits,
                        "exited without being stopped"
                    );
                }
                self.log
                    .push(format!("--- exited: {} ---", describe_exit(&exit)));
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

pub(crate) fn describe_exit(e: &ExitInfo) -> String {
    let base = match e.code {
        _ if e.ctrl_break => "Ctrl-Break".to_string(),
        Some(c) => format!("code {c}"),
        None => "signal".to_string(),
    };
    match &e.hint {
        Some(hint) => format!("{base} ({hint})"),
        None => base,
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `path`'s modification time as a Unix second, or `None` when it has none
/// (it does not exist, or the filesystem does not say).
fn mtime_unix(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Everything a [`Manager`] needs to know about its surroundings.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Where the binaries are; `None` = next to this executable, then `PATH`.
    pub bin_dir: Option<PathBuf>,
    /// The `telemouse.toml` handed to every child that takes one. Absolute:
    /// a child started from the tray has whatever working directory Explorer
    /// felt like, so a relative path would mean something else to it.
    pub config_path: PathBuf,
    pub recordings_dir: PathBuf,
    /// `recording.enabled` from that config.
    pub recording_enabled: bool,
    /// Where that config came from.
    pub config_status: ConfigStatus,
    /// How long a graceful stop may take before the child is terminated.
    pub grace: Duration,
    /// Where child output is also written, one `<id>.log` per component.
    /// `None` keeps it in memory only (always the case without `logging`).
    #[cfg_attr(not(feature = "logging"), allow(dead_code))]
    pub log_dir: Option<PathBuf>,
}

impl ManagerConfig {
    /// A config for tests and for the headless paths: everything relative to
    /// `dir`, nothing seeded.
    #[cfg(test)]
    fn for_test(dir: &Path) -> Self {
        Self {
            bin_dir: None,
            config_path: PathBuf::from("telemouse.toml"),
            recordings_dir: dir.to_path_buf(),
            recording_enabled: true,
            config_status: ConfigStatus::Defaults,
            grace: Duration::from_secs(2),
            log_dir: None,
        }
    }
}

/// The parts of the config that a `telemouse.toml` edited while the panel
/// runs can change under it.
struct Live {
    recording_enabled: bool,
    recordings_dir: PathBuf,
    mtime_unix_s: Option<u64>,
    status: ConfigStatus,
    seeded: bool,
}

/// The binary lookups a snapshot needs, and when they were made.
type BinCache = Option<(Instant, Vec<(PathBuf, bool)>)>;

pub struct Manager {
    components: Vec<Component>,
    cfg: ManagerConfig,
    live: Mutex<Live>,
    slots: tokio::sync::Mutex<HashMap<&'static str, Slot>>,
    /// Binary lookups, good for [`BIN_TTL`]; dropped on every start and stop.
    bins: Mutex<BinCache>,
    /// Fired whenever something starts or stops, so the reaper and the tray
    /// publisher can sleep instead of polling an idle panel.
    work: Notify,
    /// Disk-free / recording-size reading, good for [`DISK_TTL`].
    #[cfg(feature = "observability")]
    disk: Mutex<Option<(Instant, RecordingLive)>>,
    #[cfg(feature = "observability")]
    low_disk_warned: std::sync::atomic::AtomicBool,
}

impl Manager {
    pub fn new(components: &[Component], cfg: ManagerConfig) -> Self {
        let slots = components.iter().map(|c| (c.id, Slot::new())).collect();
        let live = Live {
            recording_enabled: cfg.recording_enabled,
            recordings_dir: cfg.recordings_dir.clone(),
            mtime_unix_s: mtime_unix(&cfg.config_path),
            status: cfg.config_status,
            seeded: cfg.config_status == ConfigStatus::Seeded,
        };
        Self {
            components: components.to_vec(),
            cfg,
            live: Mutex::new(live),
            slots: tokio::sync::Mutex::new(slots),
            bins: Mutex::new(None),
            work: Notify::new(),
            #[cfg(feature = "observability")]
            disk: Mutex::new(None),
            #[cfg(feature = "observability")]
            low_disk_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Notified on every start and stop. The reaper waits on it rather than
    /// ticking at an idle panel, and the tray publisher joins it so the icon
    /// still follows a start made from the web page.
    pub fn work(&self) -> &Notify {
        &self.work
    }

    /// The config as the page reports it.
    pub fn config_info(&self) -> ConfigInfo {
        let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        ConfigInfo {
            path: self.cfg.config_path.display().to_string(),
            found: live.mtime_unix_s.is_some(),
            seeded: live.seeded,
            status: live.status,
            mtime_unix_s: live.mtime_unix_s,
        }
    }

    pub fn recording(&self) -> RecordingInfo {
        let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        RecordingInfo {
            enabled: live.recording_enabled,
            dir: live.recordings_dir.display().to_string(),
        }
    }

    fn recordings_dir(&self) -> PathBuf {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recordings_dir
            .clone()
    }

    fn recording_enabled(&self) -> bool {
        self.live
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .recording_enabled
    }

    /// Re-read `telemouse.toml` if it changed since we last looked, and take
    /// the two fields a running panel can honour: whether capture saves by
    /// default, and where recordings go. Everything else (ports, the bin
    /// directory, the hotkey) is bound to children already launched or to
    /// the window, and is left for a restart.
    ///
    /// One `metadata` call per snapshot, so a config edited mid-session is
    /// in force within a poll instead of after a restart nobody knew to do.
    pub fn refresh_config(&self) {
        let current = mtime_unix(&self.cfg.config_path);
        {
            let live = self.live.lock().unwrap_or_else(|p| p.into_inner());
            if live.mtime_unix_s == current {
                return;
            }
        }
        let base = telemouse_core::paths::config_base(&self.cfg.config_path);
        let loaded = match AppConfig::load_or_default(&self.cfg.config_path) {
            Ok(mut c) => {
                c.resolve_paths(&base);
                Some(c)
            }
            Err(e) => {
                warn!(path = %self.cfg.config_path.display(), error = %e, "telemouse.toml changed but cannot be read; keeping the settings the panel started with");
                None
            }
        };
        let mut live = self.live.lock().unwrap_or_else(|p| p.into_inner());
        live.mtime_unix_s = current;
        let Some(c) = loaded else { return };
        live.status = if current.is_some() {
            ConfigStatus::Loaded
        } else {
            ConfigStatus::Defaults
        };
        if live.recording_enabled != c.recording.enabled || live.recordings_dir != c.recording.dir {
            info!(
                enabled = c.recording.enabled,
                dir = %c.recording.dir.display(),
                "telemouse.toml changed; recording settings reloaded"
            );
        }
        live.recording_enabled = c.recording.enabled;
        live.recordings_dir = c.recording.dir;
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

    /// [`Self::resolve_bin`] for every component, reusing a lookup younger
    /// than [`BIN_TTL`].
    fn bins_cached(&self) -> Vec<(PathBuf, bool)> {
        {
            let cache = self.bins.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((at, list)) = cache.as_ref()
                && at.elapsed() < BIN_TTL
            {
                return list.clone();
            }
        }
        let fresh: Vec<(PathBuf, bool)> = self
            .components
            .iter()
            .map(|c| self.resolve_bin(c.bin))
            .collect();
        *self.bins.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((Instant::now(), fresh.clone()));
        fresh
    }

    fn invalidate_bins(&self) {
        *self.bins.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// A recording name must be `<session id>.jsonl` with the id in the
    /// workspace-wide alphabet (`telemouse_core::recordings`): that rule is
    /// what makes the joined path a direct child of the recordings
    /// directory on every platform — a separator check alone let a
    /// drive-relative `C:x.jsonl` through on Windows.
    pub fn validate_session(&self, name: &str) -> Result<PathBuf, String> {
        let Some(id) = id_from_file_name(name) else {
            return Err(
                "must be <session id>.jsonl, a recording in the recordings directory".into(),
            );
        };
        let p = self.recordings_dir().join(format!("{id}.jsonl"));
        if !p.is_file() {
            return Err(format!("no such recording: {name}"));
        }
        Ok(p)
    }

    /// `*.jsonl` files in the recordings directory, newest first. Only names
    /// that pass [`Self::validate_session`] are listed, so the picker never
    /// offers something the start request would then refuse.
    pub fn list_sessions(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.recordings_dir()) else {
            return Vec::new();
        };
        let mut v: Vec<(SystemTime, String)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if id_from_file_name(&name).is_none() || !e.file_type().ok()?.is_file() {
                    return None;
                }
                let modified = e.metadata().ok()?.modified().unwrap_or(UNIX_EPOCH);
                Some((modified, name))
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        v.into_iter().map(|(_, n)| n).collect()
    }

    /// Reap exited children. Called on every state read and from the reaper,
    /// so an unwatched panel still records exits.
    pub async fn reap(&self) {
        let mut slots = self.slots.lock().await;
        for c in &self.components {
            slots.get_mut(c.id).expect("slot per component").reap(c);
        }
    }

    /// Is anything still running? The reaper sleeps on [`Self::work`] when
    /// nothing is, instead of waking twice a second forever.
    pub async fn any_running(&self) -> bool {
        let slots = self.slots.lock().await;
        slots.values().any(|s| s.child.is_some())
    }

    pub async fn snapshot(&self, log_lines: usize) -> Vec<ComponentState> {
        self.snapshot_since(log_lines, None).await
    }

    /// The state of every component. With `since`, each component's `log`
    /// holds only the lines that arrived after that cursor (`log_seq` from a
    /// previous read), so a page polling twice a second sends back a few
    /// hundred bytes instead of 120 lines per component.
    pub async fn snapshot_since(
        &self,
        log_lines: usize,
        since: Option<u64>,
    ) -> Vec<ComponentState> {
        // Filesystem work (config mtime, binary lookups, and a PATH walk for
        // a missing binary) happens before the lock, not under it.
        self.refresh_config();
        let bins = self.bins_cached();
        let recording_enabled = self.recording_enabled();
        let mut slots = self.slots.lock().await;
        self.components
            .iter()
            .zip(bins)
            .map(|(c, (bin_path, bin_found))| {
                let s = slots.get_mut(c.id).expect("slot per component");
                s.reap(c);
                let running = s.child.is_some();
                let saving =
                    running && c.id == "capture" && recording_saves(recording_enabled, &s.args);
                let (log, log_seq) = s.log.read(log_lines, since);
                #[cfg(feature = "observability")]
                let stats = s.log.stats();
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
                    running,
                    pid: s.pid,
                    since_unix_s: s.since,
                    last_exit: s.last_exit.clone(),
                    exits: s.exits,
                    unexpected_exits: s.unexpected_exits,
                    args: s.args.clone(),
                    saving,
                    log,
                    log_seq,
                    #[cfg(feature = "observability")]
                    recording: (running && saving)
                        .then(|| {
                            stats
                                .as_ref()
                                .and_then(|st| st.session.as_deref())
                                .map(|session| self.recording_live(session))
                        })
                        .flatten(),
                    #[cfg(feature = "observability")]
                    stats,
                }
            })
            .collect()
    }

    /// The size of the recording being written and what the disk has left,
    /// re-read at most every [`DISK_TTL`]. Two syscalls, so cheap — but the
    /// page and the tray both poll, and neither needs a fresher number than
    /// the one a human can read.
    #[cfg(feature = "observability")]
    fn recording_live(&self, session: &str) -> RecordingLive {
        use std::sync::atomic::Ordering;

        {
            let cache = self.disk.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((at, live)) = cache.as_ref()
                && at.elapsed() < DISK_TTL
                && live.session == session
            {
                return live.clone();
            }
        }
        let dir = self.recordings_dir();
        let file = dir.join(format!("{session}.jsonl"));
        let size_bytes = std::fs::metadata(&file).map(|m| m.len()).ok();
        let free_bytes = crate::stats::free_bytes(&dir);
        if let Some(free) = free_bytes {
            let low = free < LOW_DISK_BYTES;
            if low && !self.low_disk_warned.swap(low, Ordering::Relaxed) {
                warn!(
                    dir = %dir.display(),
                    free_gb = free as f64 / 1e9,
                    "less than 2 GB free where recordings are written; capture will stop when the disk fills"
                );
            } else if !low {
                self.low_disk_warned.store(false, Ordering::Relaxed);
            }
        }
        let live = RecordingLive {
            session: session.to_string(),
            file: file.display().to_string(),
            size_bytes,
            free_bytes,
        };
        *self.disk.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), live.clone()));
        live
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
            .map(|save| recording_flags(self.recording_enabled(), save))
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
        // A config edited since the last poll decides `--record` and where
        // the recording lands, so it is re-read before the arguments are
        // built rather than after.
        self.refresh_config();
        let c = self.component(id).ok_or(StartError::UnknownComponent)?;
        let args = self.arguments(c, req)?;
        let (bin, _) = self.resolve_bin(c.bin);

        let mut slots = self.slots.lock().await;
        let slot = slots.get_mut(c.id).expect("slot per component");
        if !slot.reap(c) {
            return Err(StartError::AlreadyRunning);
        }
        #[cfg(feature = "logging")]
        if let Some(dir) = &self.cfg.log_dir {
            slot.log.ensure_file(dir, c.id);
        }

        let mut cmd = Command::new(&bin);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // What a child prints is read by a page and appended to a file:
            // colour escapes are noise in both. Children that honour this
            // never emit them; `LogSink::push` strips whatever slips past.
            .env("NO_COLOR", "1")
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
        drop(slots);
        self.invalidate_bins();
        self.work.notify_waiters();
        Ok(pid)
    }

    /// Stop a component: Ctrl-Break, wait up to the grace period, then
    /// terminate. `force` skips straight to terminating.
    pub async fn stop(&self, id: &str, force: bool) -> Result<StopOutcome, StopError> {
        self.stop_within(id, force, self.cfg.grace).await
    }

    async fn stop_within(
        &self,
        id: &str,
        force: bool,
        grace: Duration,
    ) -> Result<StopOutcome, StopError> {
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
        // Whatever happens below, the process table and the binary cache are
        // stale from here on.
        self.invalidate_bins();

        // A pid of 0 would address the whole console group — this panel
        // included — so a child whose pid was never known is only terminated.
        if !force && pid.is_some_and(send_ctrl_break) {
            let deadline = tokio::time::Instant::now() + grace;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut slots = self.slots.lock().await;
                if slots.get_mut(c.id).expect("slot per component").reap(c) {
                    info!(component = c.id, pid, "stopped gracefully");
                    self.work.notify_waiters();
                    return Ok(StopOutcome::Graceful);
                }
            }
            warn!(
                component = c.id,
                pid,
                grace_s = grace.as_secs(),
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
        drop(slots);
        self.work.notify_waiters();
        Ok(StopOutcome::Terminated)
    }

    /// Shutdown: stop every running component, gracefully where possible.
    pub async fn stop_all(&self) {
        self.stop_all_within(self.cfg.grace).await;
    }

    /// The same, but bounded — for a shutdown Windows is holding open. The
    /// children have `kill_on_drop`, so this has to complete *before* the
    /// manager is dropped; that is why every exit path runs it explicitly.
    pub async fn stop_all_fast(&self) {
        self.stop_all_within(stop_grace(self.cfg.grace, true)).await;
    }

    async fn stop_all_within(&self, grace: Duration) {
        let ids: Vec<&'static str> = self.components.iter().map(|c| c.id).collect();
        for id in ids {
            let _ = self.stop_within(id, false, grace).await;
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

/// A throwaway directory under the system temp, removed first so a previous
/// run cannot leak into this one. Shared with `main.rs`'s tests.
#[cfg(test)]
pub(crate) fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "telemouse-ctl-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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
        ManagerConfig::for_test(dir)
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
        assert_eq!(s.last_exit.as_ref().map(|e| e.code), Some(Some(3)));
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

    /// The failing child's own last line is carried on the exit, so the card
    /// can say what it printed without the log being open.
    #[tokio::test]
    async fn a_failed_exit_carries_the_last_line() {
        let m = manager(Path::new("."));
        m.start("failer", &StartRequest::default()).await.unwrap();
        // The pumps may land the line after try_wait sees the exit; the next
        // reap does not re-run, so read the ring directly.
        let s = wait_exit(&m, "failer").await;
        let exit = s.last_exit.unwrap();
        assert_eq!(exit.code, Some(3));
        assert!(exit.failed());
        assert!(
            exit.last_line.is_none() || exit.last_line.as_deref() == Some("about to fail"),
            "{:?}",
            exit.last_line
        );
        // A clean stop carries neither hint nor line.
        m.start("sleeper", &StartRequest::default()).await.unwrap();
        m.stop("sleeper", true).await.unwrap();
        let s = wait_exit(&m, "sleeper").await;
        let exit = s.last_exit.unwrap();
        if !exit.failed() {
            assert!(exit.last_line.is_none());
            assert!(exit.hint.is_none());
        }
    }

    #[cfg(feature = "logging")]
    #[tokio::test]
    async fn child_output_is_also_written_to_a_log_file() {
        let dir = tmpdir("logs");
        let mut cfg = config(Path::new("."));
        cfg.log_dir = Some(dir.join("nested"));
        let m = Manager::new(&[FAILER], cfg);
        m.start("failer", &StartRequest::default()).await.unwrap();
        wait_exit(&m, "failer").await;
        // The pumps and the writer thread finish a beat after try_wait sees
        // the exit.
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
        assert!(m.any_running().await);
        m.stop("sleeper", true).await.unwrap();
        assert!(!m.any_running().await);
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

    /// The shutdown path Windows waits on: the same graceful sequence, but
    /// it may never take longer than [`FAST_STOP_GRACE`].
    #[tokio::test]
    async fn a_fast_stop_is_clamped_to_three_seconds() {
        assert_eq!(
            stop_grace(Duration::from_secs(60), true),
            FAST_STOP_GRACE,
            "a long configured grace is cut down"
        );
        assert_eq!(
            stop_grace(Duration::from_secs(1), true),
            Duration::from_secs(1),
            "a shorter one is left alone"
        );
        assert_eq!(
            stop_grace(Duration::from_secs(60), false),
            Duration::from_secs(60)
        );

        let m = Manager::new(
            &[SLEEPER],
            ManagerConfig {
                // Far longer than Windows would ever grant us.
                grace: Duration::from_secs(60),
                ..config(Path::new("."))
            },
        );
        m.start("sleeper", &StartRequest::default()).await.unwrap();
        let t0 = tokio::time::Instant::now();
        m.stop_all_fast().await;
        assert!(
            t0.elapsed() < FAST_STOP_GRACE + Duration::from_secs(2),
            "took {:?}",
            t0.elapsed()
        );
        assert!(!m.any_running().await);
    }

    #[tokio::test]
    async fn stop_all_is_quiet_on_an_idle_manager() {
        let m = manager(Path::new("."));
        m.stop_all().await;
        assert!(m.snapshot(1).await.iter().all(|s| !s.running));
        assert!(!m.any_running().await);
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
            // Drive-relative on Windows: `Path::join` yields `C:x.jsonl`,
            // resolved against the drive's current directory, not ours.
            "C:s-1.jsonl",
            // NTFS alternate data stream spelling.
            "s-1:stream.jsonl",
            "s 1.jsonl",
            ".s-1.jsonl",
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
        // The absolute config path is what the child is told to read.
        assert!(a.iter().any(|x| x == "--config"));

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
        // The cache is a cache: it answers from the same lookup until it is
        // dropped, which every start and stop does.
        let before = m.bins_cached();
        assert_eq!(m.bins_cached(), before);
        m.invalidate_bins();
        assert_eq!(m.bins_cached(), before, "same answer, freshly read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exit_hints_name_the_five_failures_the_panel_can_explain() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // A binary older than the config it was handed.
        let stale = lines(&[
            "Error: load config telemouse.toml",
            "Caused by: failed to parse telemouse.toml: TOML parse error at line 61, column 1",
            "unknown field `hotkey`, expected one of `http_addr`, `bin_dir`",
        ]);
        assert_eq!(exit_hint(&stale).as_deref(), Some(HINT_STALE_BINARY));

        // A port something else already holds.
        assert_eq!(
            exit_hint(&lines(&[
                "Error: failed to bind http 127.0.0.1:7879",
                "Caused by: Only one usage of each socket address (protocol/network address/port) is normally permitted. (os error 10048)",
            ]))
            .as_deref(),
            Some(HINT_PORT_IN_USE)
        );
        assert_eq!(
            exit_hint(&lines(&["Caused by: os error 10048"])).as_deref(),
            Some(HINT_PORT_IN_USE)
        );

        // An elevated target.
        assert_eq!(
            exit_hint(&lines(&["Access is denied. (os error 5)"])).as_deref(),
            Some(HINT_ACCESS_DENIED)
        );

        // A value the config rejects explains itself; echo it verbatim.
        let echo = exit_hint(&lines(&[
            "starting",
            "Error: C:\\tm\\telemouse.toml: invalid config: mouse_cpi: must be a positive number, got 0",
        ]))
        .unwrap();
        assert!(echo.contains("invalid config: mouse_cpi"), "{echo}");
        assert!(echo.starts_with("Error:"), "{echo}");

        assert_eq!(exit_hint(&lines(&["all good"])), None);
        assert_eq!(exit_hint(&[]), None);

        let e = ExitInfo::new(Some(1), 1).with_hint(exit_hint(&stale));
        assert!(describe_exit(&e).starts_with("code 1 ("));
        assert!(describe_exit(&e).contains("rebuild"));
        assert_eq!(describe_exit(&ExitInfo::new(Some(1), 1)), "code 1");
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["hint"], HINT_STALE_BINARY);
        assert!(
            serde_json::to_value(ExitInfo::new(Some(0), 1))
                .unwrap()
                .get("hint")
                .is_none()
        );
        assert!(!ExitInfo::new(Some(0), 1).failed());
        assert!(!ExitInfo::new(Some(STATUS_CONTROL_C_EXIT), 1).failed());
        assert!(ExitInfo::new(Some(1), 1).failed());
    }

    #[test]
    fn a_hint_is_clipped_to_its_first_clause_for_a_narrow_column() {
        assert_eq!(short_hint(HINT_PORT_IN_USE, 40), "port already in use");
        assert_eq!(
            short_hint(HINT_STALE_BINARY, 20),
            "telemouse.toml has …",
            "clipped to the width given"
        );
        assert_eq!(clip("short", 18), "short");
        assert_eq!(clip("0123456789abcdefghij", 18).chars().count(), 18);
        assert!(clip("0123456789abcdefghij", 18).ends_with('…'));
        // Multi-byte input must not be cut mid-character.
        assert_eq!(clip("ü".repeat(30).as_str(), 4).chars().count(), 4);
    }

    /// A colourised line from an old release binary reaches neither the page
    /// nor the log file with its escapes intact.
    #[test]
    fn the_sink_strips_ansi_escapes() {
        let sink = LogSink::default();
        sink.push("\u{1b}[32m INFO\u{1b}[0m telemouse ready".into());
        sink.push("plain line".into());
        assert_eq!(sink.tail(2), vec![" INFO telemouse ready", "plain line"]);
        assert_eq!(sink.last_non_empty().as_deref(), Some("plain line"));
    }

    #[test]
    fn log_since_sends_only_what_is_new() {
        let sink = LogSink::default();
        let (lines, seq) = sink.read(10, None);
        assert!(lines.is_empty());
        assert_eq!(seq, 0);

        for i in 0..5 {
            sink.push(format!("line {i}"));
        }
        let (lines, seq) = sink.read(10, None);
        assert_eq!(lines.len(), 5);
        assert_eq!(seq, 5);

        // Nothing new since the cursor.
        let (lines, seq2) = sink.read(10, Some(seq));
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(seq2, seq);

        sink.push("line 5".into());
        sink.push("line 6".into());
        let (lines, seq3) = sink.read(10, Some(seq));
        assert_eq!(lines, vec!["line 5".to_string(), "line 6".to_string()]);
        assert_eq!(seq3, 7);

        // A client that fell out of the ring gets what is still held, not a
        // panic and not a gap it cannot detect.
        for i in 0..(LOG_CAPACITY + 50) {
            sink.push(format!("flood {i}"));
        }
        let (lines, seq4) = sink.read(LOG_CAPACITY, Some(seq3));
        assert_eq!(lines.len(), LOG_CAPACITY);
        assert_eq!(seq4, 7 + LOG_CAPACITY as u64 + 50);
        // A cursor from the future is treated as "nothing new".
        assert!(sink.read(10, Some(seq4 + 100)).0.is_empty());
    }

    #[tokio::test]
    async fn snapshot_carries_the_log_cursor() {
        let m = manager(Path::new("."));
        m.start("failer", &StartRequest::default()).await.unwrap();
        let s = wait_exit(&m, "failer").await;
        assert!(s.log_seq >= 2, "{} lines", s.log_seq);
        let st = m.snapshot_since(50, Some(s.log_seq)).await;
        let again = st.iter().find(|x| x.id == "failer").unwrap();
        assert!(
            again.log.is_empty(),
            "nothing new since the cursor: {:?}",
            again.log
        );
        assert_eq!(again.log_seq, s.log_seq);
    }

    /// A `telemouse.toml` edited while the panel runs changes what a start
    /// does, without a restart.
    #[tokio::test]
    async fn a_config_edited_while_running_is_reloaded() {
        let dir = tmpdir("reload");
        let cfg_path = dir.join("telemouse.toml");
        std::fs::write(&cfg_path, "[recording]\nenabled = true\ndir = \"a\"\n").unwrap();
        let m = Manager::new(
            &[SLEEPER],
            ManagerConfig {
                config_path: cfg_path.clone(),
                recordings_dir: dir.join("a"),
                recording_enabled: true,
                config_status: ConfigStatus::Loaded,
                ..config(&dir)
            },
        );
        let info = m.config_info();
        assert_eq!(info.status, ConfigStatus::Loaded);
        assert!(info.found);
        assert!(!info.seeded);
        assert!(info.mtime_unix_s.is_some());
        assert!(m.recording().enabled);

        // mtime has a one-second resolution on some filesystems; make the
        // change unmistakable.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&cfg_path, "[recording]\nenabled = false\ndir = \"b\"\n").unwrap();
        m.refresh_config();
        assert!(!m.recording().enabled, "the edit is in force");
        assert!(m.recording().dir.ends_with('b'), "{}", m.recording().dir);
        assert!(m.recording().dir.starts_with(dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_config_reads_as_defaults() {
        let dir = tmpdir("noconfig");
        let m = Manager::new(
            &[SLEEPER],
            ManagerConfig {
                config_path: dir.join("telemouse.toml"),
                config_status: ConfigStatus::Defaults,
                ..config(&dir)
            },
        );
        let info = m.config_info();
        assert!(!info.found);
        assert!(!info.seeded);
        assert_eq!(info.status, ConfigStatus::Defaults);
        assert_eq!(info.mtime_unix_s, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_ring_is_bounded() {
        let mut r = LogRing::default();
        for i in 0..(LOG_CAPACITY + 10) {
            r.push(i.to_string());
        }
        assert_eq!(r.lines.len(), LOG_CAPACITY);
        assert_eq!(r.seq, LOG_CAPACITY as u64 + 10);
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
