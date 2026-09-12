//! `telemouse.toml` — one config file shared by the capture agent (which
//! reads all of it) and the viz/analyze tools (which read the addresses and
//! game table).
//!
//! **Paths in `telemouse.toml` are relative to the config file's own
//! directory**, not to the working directory of whoever started the process
//! (see [`AppConfig::resolve_paths`] and [`crate::paths`]): started from the
//! tray or a shortcut, the working directory is not something the user
//! chose, and `dir = "recordings"` has to keep meaning the folder beside the
//! config.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::GameSens;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not UTF-8 — almost always Notepad's "Unicode" or a
    /// PowerShell 5.1 redirection, which writes UTF-16. Carries the fix
    /// rather than the byte offset, because the byte offset helps nobody.
    #[error("cannot read {path}: {hint}")]
    Encoding { path: PathBuf, hint: String },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A value the parser accepted but the pipeline cannot use. `path` is
    /// the file it came from, when it came from a file at all.
    #[error("{}", invalid_message(.path.as_deref(), .field, .reason))]
    Invalid {
        field: &'static str,
        reason: String,
        path: Option<PathBuf>,
    },
}

/// `<path>: invalid config: <field>: <reason>`, or the same without the
/// prefix for a config that was never on disk.
fn invalid_message(path: Option<&Path>, field: &str, reason: &str) -> String {
    match path {
        Some(p) => format!("{}: invalid config: {field}: {reason}", p.display()),
        None => format!("invalid config: {field}: {reason}"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub mouse_cpi: f64,
    pub batch: BatchConfig,
    pub udp: UdpConfig,
    pub kafka: KafkaConfig,
    pub recording: RecordingConfig,
    pub viz: VizConfig,
    pub ctl: CtlConfig,
    /// Keyed by lowercase process name (e.g. "cs2.exe").
    pub games: BTreeMap<String, GameSens>,
}

/// Upper bound on `batch.coalesce_ms`: beyond this the read batching stops
/// paying for itself and only adds latency.
pub const MAX_COALESCE_MS: u64 = 10;

/// Upper bound on `batch.window_ms`. One second of events is both a live
/// view nobody would call live and, at 1kHz, more events than the 448-event
/// batch cap can carry — past this the window stops being the thing that
/// decides when a batch ships.
pub const MAX_WINDOW_MS: u64 = 1000;

/// Upper bound on `batch.ring_capacity`. 4M events is ~an hour of 1kHz
/// capture sitting in RAM: a ring this deep no longer protects the capture
/// thread, it just delays the moment anyone notices the shipping thread
/// stopped.
pub const MAX_RING_CAPACITY: usize = 4_194_304;

/// The `[games]` key a process name should be written as: the bare file
/// name, lowercased. Matching is done on `GetModuleFileNameEx`'s base name
/// in lower case, so anything else in the table is a row that can never fire.
pub fn normalize_game_key(key: &str) -> String {
    let base = key.rsplit(['/', '\\']).next().unwrap_or(key);
    base.trim().to_ascii_lowercase()
}

/// Why a `[games]` key would never match a running process, if so.
fn game_key_problem(key: &str) -> Result<(), String> {
    let norm = normalize_game_key(key);
    if key.contains(['/', '\\', ':']) {
        return Err(format!(
            "{key:?} is a path; the key is the bare process name, e.g. {norm:?}"
        ));
    }
    if key != key.to_ascii_lowercase() {
        return Err(format!(
            "{key:?} is matched case-sensitively against a lowercased process name \
             and can never fire; write it as {norm:?}"
        ));
    }
    if !key.ends_with(".exe") {
        return Err(format!(
            "{key:?} is not an executable name; the key is the process's file name, \
             e.g. {:?}",
            format!("{norm}.exe")
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchConfig {
    /// Batch window in milliseconds: how long the shipping thread
    /// accumulates events before one batch goes to every sink. It sets the
    /// live-viz latency floor (a batch's first event is this old when it
    /// leaves) and, with it, the batches/s that the UDP hop, the recording
    /// and every WebSocket client pay for — each batch costs ~150µs of
    /// kernel CPU across the pipeline whatever it holds. The 25ms default
    /// favors responsive live visualization; use 50ms to halve the per-batch
    /// CPU cost when throughput matters more than display latency. Recording
    /// contents and analysis do not depend on it.
    pub window_ms: u64,
    pub max_events: usize,
    /// SPSC ring-buffer capacity in events.
    pub ring_capacity: usize,
    /// How long the capture thread lets raw-input reports pile up before
    /// reading them all in one call, in ms. Being woken by the raw-input
    /// queue costs Windows ~25–30µs of kernel CPU per wake, so the capture
    /// thread wakes on the first report of a burst, then runs on a timer
    /// (this window plus one report interval per drain) for as long as
    /// reports keep coming. At 1kHz: `2` ≈ 0.45% of a core, `8` ≈ 0.22%.
    /// Only a burst's first report has an observed arrival; the rest are
    /// spaced by the estimated report interval, so timestamps are exact to
    /// about this many ms and a pause shorter than this inside a burst is
    /// smoothed over. `0` reads every report at its own wake (exact stamps,
    /// ~2.8% of a core at 1kHz). At most [`MAX_COALESCE_MS`].
    pub coalesce_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UdpConfig {
    pub enabled: bool,
    /// Where the capture agent sends live envelopes.
    pub addr: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KafkaConfig {
    pub enabled: bool,
    pub brokers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    pub enabled: bool,
    /// Directory for per-session JSONL recordings.
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VizConfig {
    /// HTTP + WebSocket listen address for the viz server.
    pub http_addr: String,
    /// Defaults for the `/obs` browser-source page. Any of these can be
    /// overridden per source with URL query parameters.
    pub obs: ObsConfig,
}

/// Upper bound on `ctl.stop_grace_secs`: a stop that has not completed in a
/// minute is not going to.
pub const MAX_STOP_GRACE_SECS: u64 = 60;

/// Default `ctl.stop_grace_secs`. The capture agent's teardown drains its
/// JSONL and Kafka sinks in sequence, each bounded at 3 s, plus the thread
/// joins; 8 s covers that worst case, so a slow disk or broker at stop time
/// costs a delay rather than a truncated recording.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 8;

/// The control panel (`telemouse-ctl`): where it listens and how it finds
/// and stops the binaries it launches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CtlConfig {
    /// HTTP listen address for the control panel.
    pub http_addr: String,
    /// Directory holding the telemouse binaries the panel launches. Unset =
    /// the directory `telemouse-ctl` itself runs from, then `PATH`.
    pub bin_dir: Option<PathBuf>,
    /// How long a graceful stop (Ctrl-Break) may take before the child is
    /// terminated outright.
    pub stop_grace_secs: u64,
    /// Where the panel writes `ctl.log` (its own log — the console is hidden
    /// when started from Explorer) and one `<component>.log` per launched
    /// component (what the child printed, kept across panel restarts).
    pub log_dir: PathBuf,
    /// System-wide hotkey (Windows, with the tray running) that starts a new
    /// saved session: stops the capture agent if it is running, then starts
    /// one that records. `ctrl+alt+r` by default; `""` for none. Grammar in
    /// [`crate::hotkey`].
    pub hotkey: String,
}

impl Default for CtlConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:7880".into(),
            bin_dir: None,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            log_dir: PathBuf::from("logs"),
            hotkey: "ctrl+alt+r".into(),
        }
    }
}

/// Layouts the OBS page understands. Kept as a list so validation and the
/// page agree on the vocabulary.
pub const OBS_LAYOUTS: &[&str] = &["split", "stack", "desk", "aim"];
/// HUD readouts the OBS page can show, in the order they are listed.
pub const OBS_HUD_ITEMS: &[&str] = &[
    "speed", "aim", "cpm", "eps", "dist", "aimdist", "clicks", "game", "latency",
];
pub const OBS_HUD_POSITIONS: &[&str] = &["top-left", "top-right", "bottom-left", "bottom-right"];

/// How the viz renders when it is an OBS browser source rather than a
/// dashboard: no chrome, a transparent background by default, and a small
/// overlay HUD instead of the stats bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObsConfig {
    /// `split` (desk | aim side by side), `stack` (desk over aim), `desk`, `aim`.
    pub layout: String,
    /// `transparent`, or `#rrggbb` / `#rrggbbaa` painted behind the panels.
    pub background: String,
    /// Readouts drawn in the overlay HUD; empty hides it.
    pub hud: Vec<String>,
    /// Corner the HUD sits in.
    pub hud_position: String,
    /// Multiplier on stroke widths, head/ring sizes and HUD text — raise it
    /// for a small source on a 1080p canvas.
    pub scale: f64,
    /// Trail decay in seconds.
    pub trail_secs: f64,
    /// Live buffer (how far the play head trails the newest event).
    pub buffer_ms: u64,
    pub grid: bool,
    pub legend: bool,
    /// Panel titles ("Desk space — hand path", grid step).
    pub labels: bool,
    /// How long the overlay waits, in seconds, before it says there is no
    /// feed. Short enough that a dead capture agent is visible on stream,
    /// long enough that an idle hand is not mistaken for one. `0` turns the
    /// indicator off, for a source that must never draw anything but the
    /// trails.
    pub stale_secs: f64,
}

/// Upper bound on `viz.obs.stale_secs`: past a minute the indicator would
/// outlive the stream it is meant to warn about.
pub const MAX_OBS_STALE_SECS: f64 = 60.0;

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mouse_cpi: 1600.0,
            batch: BatchConfig::default(),
            udp: UdpConfig::default(),
            kafka: KafkaConfig::default(),
            recording: RecordingConfig::default(),
            viz: VizConfig::default(),
            ctl: CtlConfig::default(),
            games: BTreeMap::new(),
        }
    }
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            window_ms: 25,
            max_events: crate::wire::MAX_EVENTS_PER_BATCH,
            ring_capacity: 65_536,
            coalesce_ms: 8,
        }
    }
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            addr: "127.0.0.1:7878".into(),
        }
    }
}

impl Default for KafkaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            brokers: vec!["127.0.0.1:9092".into()],
        }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: PathBuf::from("recordings"),
        }
    }
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:7879".into(),
            obs: ObsConfig::default(),
        }
    }
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            layout: "split".into(),
            background: "transparent".into(),
            hud: vec!["speed".into(), "aim".into(), "cpm".into()],
            hud_position: "bottom-left".into(),
            scale: 1.0,
            trail_secs: 3.0,
            buffer_ms: 35,
            grid: true,
            legend: false,
            labels: false,
            stale_secs: 3.0,
        }
    }
}

impl ObsConfig {
    /// `transparent`, `#rgb`, `#rrggbb` or `#rrggbbaa` (leading `#` optional).
    fn background_is_valid(s: &str) -> bool {
        if s == "transparent" || s == "none" {
            return true;
        }
        let hex = s.strip_prefix('#').unwrap_or(s);
        matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
    }

    /// Reject overlay settings the page cannot render.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_at(None)
    }

    /// Same, naming the file the values came from in the error.
    fn validate_at(&self, path: Option<&Path>) -> Result<(), ConfigError> {
        let invalid = |field: &'static str, reason: String| ConfigError::Invalid {
            field,
            reason,
            path: path.map(Path::to_path_buf),
        };
        if !OBS_LAYOUTS.contains(&self.layout.as_str()) {
            return Err(invalid(
                "viz.obs.layout",
                format!("{:?} is not one of {}", self.layout, OBS_LAYOUTS.join(", ")),
            ));
        }
        if !Self::background_is_valid(&self.background) {
            return Err(invalid(
                "viz.obs.background",
                format!(
                    "{:?}: expected \"transparent\" or a #rrggbb / #rrggbbaa colour",
                    self.background
                ),
            ));
        }
        for item in &self.hud {
            if !OBS_HUD_ITEMS.contains(&item.as_str()) {
                return Err(invalid(
                    "viz.obs.hud",
                    format!("{item:?} is not one of {}", OBS_HUD_ITEMS.join(", ")),
                ));
            }
        }
        if !OBS_HUD_POSITIONS.contains(&self.hud_position.as_str()) {
            return Err(invalid(
                "viz.obs.hud_position",
                format!(
                    "{:?} is not one of {}",
                    self.hud_position,
                    OBS_HUD_POSITIONS.join(", ")
                ),
            ));
        }
        if !(self.scale.is_finite() && (0.5..=4.0).contains(&self.scale)) {
            return Err(invalid("viz.obs.scale", "must be between 0.5 and 4".into()));
        }
        if !(self.trail_secs.is_finite() && (0.3..=12.0).contains(&self.trail_secs)) {
            return Err(invalid(
                "viz.obs.trail_secs",
                "must be between 0.3 and 12".into(),
            ));
        }
        if !(10..=200).contains(&self.buffer_ms) {
            return Err(invalid(
                "viz.obs.buffer_ms",
                "must be between 10 and 200".into(),
            ));
        }
        if !(self.stale_secs.is_finite() && (0.0..=MAX_OBS_STALE_SECS).contains(&self.stale_secs)) {
            return Err(invalid(
                "viz.obs.stale_secs",
                format!("must be between 0 (off) and {MAX_OBS_STALE_SECS:.0}"),
            ));
        }
        Ok(())
    }
}

/// Whether a `kafka.brokers` entry is a usable bootstrap address: `host:port`
/// (or `[v6]:port`) with a non-empty host and a non-zero port. A bare host
/// is rejected rather than defaulted, because the client connects with the
/// string as written and a port-less entry can never be reached.
pub fn broker_is_valid(broker: &str) -> bool {
    let s = broker.trim();
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        let host = &rest[..end];
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match rest[end + 1..].strip_prefix(':') {
            Some(port) => (host, port),
            None => return false,
        }
    } else {
        match s.rsplit_once(':') {
            Some((host, port)) => (host, port),
            None => return false,
        }
    };
    !host.is_empty()
        && !host.chars().any(|c| c.is_whitespace() || c == '/')
        && port.parse::<u16>().is_ok_and(|p| p > 0)
}

/// Decode a config file's bytes as UTF-8, turning the two ways a Windows
/// editor gets this wrong into an error that says how to fix it.
///
/// A UTF-8 BOM is stripped here rather than left to the parser, so the file
/// Notepad writes as "UTF-8 with BOM" parses like any other.
fn decode_config(path: &Path, bytes: Vec<u8>) -> Result<String, ConfigError> {
    let encoding = |hint: &str| ConfigError::Encoding {
        path: path.to_path_buf(),
        hint: hint.to_string(),
    };
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(encoding(
            "the file is UTF-16 (Notepad's \"Unicode\" or PowerShell 5.1 redirection); \
             save it as UTF-8",
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        encoding("not valid UTF-8; save it as UTF-8 (Notepad: Save As → Encoding: UTF-8)")
    })?;
    Ok(text
        .strip_prefix('\u{feff}')
        .map(str::to_string)
        .unwrap_or(text))
}

impl AppConfig {
    /// Read and validate `path`. Every error names the file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let text = decode_config(path, bytes)?;
        let cfg: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate_at(Some(path))?;
        Ok(cfg)
    }

    /// Reject values that would silently corrupt metrics or destabilize the
    /// pipeline. Called by [`Self::load`]; defaults always pass.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_at(None)
    }

    /// Same, naming the file the values came from in the error.
    fn validate_at(&self, path: Option<&Path>) -> Result<(), ConfigError> {
        let invalid = |field: &'static str, reason: String| ConfigError::Invalid {
            field,
            reason,
            path: path.map(Path::to_path_buf),
        };
        if !(self.mouse_cpi.is_finite() && self.mouse_cpi > 0.0) {
            return Err(invalid(
                "mouse_cpi",
                format!("must be a positive number, got {}", self.mouse_cpi),
            ));
        }
        if self.batch.window_ms == 0 {
            return Err(invalid("batch.window_ms", "must be at least 1ms".into()));
        }
        if self.batch.window_ms > MAX_WINDOW_MS {
            return Err(invalid(
                "batch.window_ms",
                format!(
                    "must be at most {MAX_WINDOW_MS}ms; it is the live-viz latency floor, \
                     and a window this long buffers more events than one batch can carry"
                ),
            ));
        }
        if self.batch.max_events == 0 {
            return Err(invalid("batch.max_events", "must be at least 1".into()));
        }
        if self.batch.max_events > crate::wire::MAX_EVENTS_PER_BATCH {
            return Err(invalid(
                "batch.max_events",
                format!(
                    "exceeds the UDP datagram budget of {}",
                    crate::wire::MAX_EVENTS_PER_BATCH
                ),
            ));
        }
        if self.batch.ring_capacity < self.batch.max_events {
            return Err(invalid(
                "batch.ring_capacity",
                "must be at least batch.max_events".into(),
            ));
        }
        if self.batch.ring_capacity > MAX_RING_CAPACITY {
            return Err(invalid(
                "batch.ring_capacity",
                format!(
                    "must be at most {MAX_RING_CAPACITY} events (~{} MB of locked ring); \
                     a ring this deep hides a stalled shipping thread instead of dropping",
                    MAX_RING_CAPACITY * size_of::<crate::RawEvent>() / (1024 * 1024)
                ),
            ));
        }
        if self.batch.coalesce_ms > MAX_COALESCE_MS {
            return Err(invalid(
                "batch.coalesce_ms",
                format!("must be at most {MAX_COALESCE_MS}ms; it is added to live latency"),
            ));
        }
        if self.kafka.enabled && self.kafka.brokers.is_empty() {
            return Err(invalid(
                "kafka.brokers",
                "kafka is enabled but no brokers are listed".into(),
            ));
        }
        // Checked whether or not Kafka is enabled: a broker that cannot be
        // reached as written is a typo whenever it is written.
        if let Some(bad) = self.kafka.brokers.iter().find(|b| !broker_is_valid(b)) {
            return Err(invalid(
                "kafka.brokers",
                format!(
                    "{bad:?} is not a host:port address (a port is required, e.g. \"{}:9092\")",
                    bad.trim()
                ),
            ));
        }
        self.viz.obs.validate_at(path)?;
        if self.ctl.stop_grace_secs > MAX_STOP_GRACE_SECS {
            return Err(invalid(
                "ctl.stop_grace_secs",
                format!("must be at most {MAX_STOP_GRACE_SECS}s"),
            ));
        }
        if let Err(reason) = crate::hotkey::Hotkey::parse(&self.ctl.hotkey) {
            return Err(invalid("ctl.hotkey", reason));
        }
        for (game, g) in &self.games {
            if let Err(reason) = game_key_problem(game) {
                return Err(invalid("games", reason));
            }
            if !(g.sens.is_finite() && g.sens > 0.0)
                || !(g.yaw_coeff.is_finite() && g.yaw_coeff > 0.0)
                || !(g.pitch_coeff.is_finite() && g.pitch_coeff > 0.0)
            {
                return Err(invalid(
                    "games",
                    format!("{game}: sens/yaw_coeff/pitch_coeff must all be positive"),
                ));
            }
        }
        Ok(())
    }

    /// Make `recording.dir`, `ctl.log_dir` and `ctl.bin_dir` absolute against
    /// `base` — the config file's own directory (see [`crate::paths`]).
    ///
    /// Call this once after loading. Without it, a relative `recordings`
    /// means "wherever this process happened to be started from", which for
    /// anything launched by the tray or a shortcut is not a place the user
    /// picked, and a recording written there is a recording nobody finds.
    pub fn resolve_paths(&mut self, base: &Path) {
        fn absolutize(p: &Path, base: &Path) -> PathBuf {
            if p.is_absolute() || p.as_os_str().is_empty() {
                p.to_path_buf()
            } else {
                base.join(p)
            }
        }
        self.recording.dir = absolutize(&self.recording.dir, base);
        self.ctl.log_dir = absolutize(&self.ctl.log_dir, base);
        if let Some(dir) = &self.ctl.bin_dir {
            self.ctl.bin_dir = Some(absolutize(dir, base));
        }
    }

    /// Every scalar setting that differs from the defaults, as
    /// `(field, value)` pairs — the startup banner's job is to say what is
    /// *not* standard, so a support question starts from five lines instead
    /// of the whole file. `games` appears as a count, since the table itself
    /// is not a banner's business.
    pub fn non_default_fields(&self) -> Vec<(&'static str, String)> {
        let d = AppConfig::default();
        let mut out: Vec<(&'static str, String)> = Vec::new();
        let mut add = |name: &'static str, v: String| out.push((name, v));

        if self.mouse_cpi != d.mouse_cpi {
            add("mouse_cpi", self.mouse_cpi.to_string());
        }
        if self.batch.window_ms != d.batch.window_ms {
            add("batch.window_ms", self.batch.window_ms.to_string());
        }
        if self.batch.max_events != d.batch.max_events {
            add("batch.max_events", self.batch.max_events.to_string());
        }
        if self.batch.ring_capacity != d.batch.ring_capacity {
            add("batch.ring_capacity", self.batch.ring_capacity.to_string());
        }
        if self.batch.coalesce_ms != d.batch.coalesce_ms {
            add("batch.coalesce_ms", self.batch.coalesce_ms.to_string());
        }
        if self.udp.enabled != d.udp.enabled {
            add("udp.enabled", self.udp.enabled.to_string());
        }
        if self.udp.addr != d.udp.addr {
            add("udp.addr", self.udp.addr.clone());
        }
        if self.kafka.enabled != d.kafka.enabled {
            add("kafka.enabled", self.kafka.enabled.to_string());
        }
        if self.kafka.brokers != d.kafka.brokers {
            add("kafka.brokers", self.kafka.brokers.join(", "));
        }
        if self.recording.enabled != d.recording.enabled {
            add("recording.enabled", self.recording.enabled.to_string());
        }
        if self.recording.dir != d.recording.dir {
            add("recording.dir", self.recording.dir.display().to_string());
        }
        if self.viz.http_addr != d.viz.http_addr {
            add("viz.http_addr", self.viz.http_addr.clone());
        }
        if self.ctl.http_addr != d.ctl.http_addr {
            add("ctl.http_addr", self.ctl.http_addr.clone());
        }
        if self.ctl.bin_dir != d.ctl.bin_dir {
            add(
                "ctl.bin_dir",
                self.ctl
                    .bin_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
        }
        if self.ctl.stop_grace_secs != d.ctl.stop_grace_secs {
            add("ctl.stop_grace_secs", self.ctl.stop_grace_secs.to_string());
        }
        if self.ctl.log_dir != d.ctl.log_dir {
            add("ctl.log_dir", self.ctl.log_dir.display().to_string());
        }
        if self.ctl.hotkey != d.ctl.hotkey {
            add("ctl.hotkey", self.ctl.hotkey.clone());
        }
        if !self.games.is_empty() {
            add("games", format!("{} entries", self.games.len()));
        }
        out
    }

    /// Load from `path` if it exists, otherwise defaults.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_plan() {
        let c = AppConfig::default();
        // 25ms responsive live batches and an 8ms raw-input read cadence.
        assert_eq!(c.batch.window_ms, 25);
        assert_eq!(c.batch.coalesce_ms, 8);
        assert!(c.batch.coalesce_ms <= MAX_COALESCE_MS);
        assert!(c.udp.enabled);
        assert!(!c.kafka.enabled);
        assert!(c.recording.enabled);
    }

    #[test]
    fn ctl_section_parses_and_defaults() {
        let c = AppConfig::default();
        assert_eq!(c.ctl.http_addr, "127.0.0.1:7880");
        assert_eq!(c.ctl.bin_dir, None);
        assert_eq!(c.ctl.stop_grace_secs, DEFAULT_STOP_GRACE_SECS);
        assert!(
            c.ctl.stop_grace_secs >= 8,
            "the grace must cover both sink drains (3 s each) plus the joins"
        );
        assert_eq!(c.ctl.log_dir, Path::new("logs"));
        assert_eq!(c.ctl.hotkey, "ctrl+alt+r");

        let c: AppConfig = toml::from_str(
            "[ctl]\nhttp_addr = \"127.0.0.1:9000\"\nbin_dir = \"target/release\"\nstop_grace_secs = 2\nlog_dir = \"var/log\"\nhotkey = \"shift+f9\"\n",
        )
        .unwrap();
        assert_eq!(c.ctl.http_addr, "127.0.0.1:9000");
        assert_eq!(c.ctl.bin_dir.as_deref(), Some(Path::new("target/release")));
        assert_eq!(c.ctl.stop_grace_secs, 2);
        assert_eq!(c.ctl.log_dir, Path::new("var/log"));
        assert_eq!(c.ctl.hotkey, "shift+f9");
        c.validate().unwrap();

        let mut bad = AppConfig::default();
        bad.ctl.stop_grace_secs = MAX_STOP_GRACE_SECS + 1;
        assert!(matches!(
            bad.validate(),
            Err(ConfigError::Invalid {
                field: "ctl.stop_grace_secs",
                ..
            })
        ));

        let mut off = AppConfig::default();
        off.ctl.hotkey = String::new();
        off.validate().unwrap();
        let mut bad = AppConfig::default();
        bad.ctl.hotkey = "ctrl+bogus".into();
        assert!(matches!(
            bad.validate(),
            Err(ConfigError::Invalid {
                field: "ctl.hotkey",
                ..
            })
        ));
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let c: AppConfig = toml::from_str(
            r#"
            mouse_cpi = 3200.0

            [kafka]
            enabled = true
            brokers = ["10.0.0.5:9092"]

            [games."cs2.exe"]
            sens = 1.25
            "#,
        )
        .unwrap();
        assert_eq!(c.mouse_cpi, 3200.0);
        assert!(c.kafka.enabled);
        assert_eq!(c.kafka.brokers, vec!["10.0.0.5:9092".to_string()]);
        assert_eq!(c.batch.window_ms, 25); // default preserved
        let g = c.games.get("cs2.exe").unwrap();
        assert_eq!(g.sens, 1.25);
        assert_eq!(g.yaw_coeff, 0.022); // serde default
    }

    #[test]
    fn empty_toml_is_all_defaults() {
        let c: AppConfig = toml::from_str("").unwrap();
        assert_eq!(c, AppConfig::default());
    }

    #[test]
    fn defaults_pass_validation() {
        AppConfig::default().validate().unwrap();
    }

    #[test]
    fn obs_table_parses_and_accepts_every_documented_value() {
        let c: AppConfig = toml::from_str(
            r##"
            [viz.obs]
            layout = "aim"
            background = "#0e131ccc"
            hud = ["speed", "aim", "cpm", "eps", "dist", "aimdist", "clicks", "game", "latency"]
            hud_position = "top-right"
            scale = 1.5
            trail_secs = 2.0
            buffer_ms = 40
            grid = false
            legend = true
            labels = true
            "##,
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.viz.obs.layout, "aim");
        assert_eq!(c.viz.obs.hud.len(), OBS_HUD_ITEMS.len());
        assert_eq!(c.viz.http_addr, "127.0.0.1:7879"); // sibling default preserved
        for bg in ["transparent", "none", "fff", "#ffffff", "#ffffff80"] {
            assert!(ObsConfig::background_is_valid(bg), "{bg}");
        }
    }

    #[test]
    fn broker_addresses_need_a_host_and_a_port() {
        for ok in [
            "127.0.0.1:9092",
            "kafka.lan:9092",
            "[::1]:9092",
            " 192.168.137.67:9092 ",
            "broker-1.internal:19092",
        ] {
            assert!(broker_is_valid(ok), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            "192.168.137.67",
            "[::1]",
            "[::1]9092",
            "[not-v6]:9092",
            ":9092",
            "host:",
            "host:0",
            "host:65536",
            "host:9092/path",
            "http://host:9092",
        ] {
            assert!(!broker_is_valid(bad), "{bad:?} should be rejected");
        }
        // Disabled Kafka still has its broker list checked: the typo is the
        // same typo whenever it gets switched on.
        let cfg: AppConfig =
            toml::from_str("[kafka]\nenabled = false\nbrokers = [\"10.0.0.5\"]").unwrap();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::Invalid {
                field: "kafka.brokers",
                ..
            })
        ));
    }

    #[test]
    fn typoed_key_is_rejected_not_ignored() {
        // "mouse_dpi" instead of "mouse_cpi" must be a hard error, otherwise
        // every cm-derived metric is silently wrong at the default CPI.
        assert!(toml::from_str::<AppConfig>("mouse_dpi = 3200.0").is_err());
        assert!(toml::from_str::<AppConfig>("[batch]\nwindowms = 10").is_err());
    }

    #[test]
    fn corrupting_values_are_rejected() {
        let cases: &[(&str, &str)] = &[
            ("mouse_cpi", "mouse_cpi = -1600.0"),
            ("mouse_cpi", "mouse_cpi = 0.0"),
            ("batch.window_ms", "[batch]\nwindow_ms = 0"),
            ("batch.max_events", "[batch]\nmax_events = 0"),
            ("batch.max_events", "[batch]\nmax_events = 4096"),
            (
                "batch.ring_capacity",
                "[batch]\nmax_events = 448\nring_capacity = 16",
            ),
            ("batch.coalesce_ms", "[batch]\ncoalesce_ms = 11"),
            ("kafka.brokers", "[kafka]\nenabled = true\nbrokers = []"),
            // A port-less broker can never be connected to as written.
            (
                "kafka.brokers",
                "[kafka]\nenabled = true\nbrokers = [\"127.0.0.1:9092\", \"192.168.137.67\"]",
            ),
            ("kafka.brokers", "[kafka]\nbrokers = [\"broker:notaport\"]"),
            ("kafka.brokers", "[kafka]\nbrokers = [\":9092\"]"),
            ("kafka.brokers", "[kafka]\nbrokers = [\"host:0\"]"),
            ("games", "[games.\"a.exe\"]\nsens = -2.0"),
            ("viz.obs.layout", "[viz.obs]\nlayout = \"sideways\""),
            ("viz.obs.background", "[viz.obs]\nbackground = \"blue\""),
            ("viz.obs.background", "[viz.obs]\nbackground = \"#12345\""),
            ("viz.obs.hud", "[viz.obs]\nhud = [\"speed\", \"wpm\"]"),
            (
                "viz.obs.hud_position",
                "[viz.obs]\nhud_position = \"middle\"",
            ),
            ("viz.obs.scale", "[viz.obs]\nscale = 0.1"),
            ("viz.obs.trail_secs", "[viz.obs]\ntrail_secs = 60"),
            ("viz.obs.buffer_ms", "[viz.obs]\nbuffer_ms = 5"),
            ("viz.obs.stale_secs", "[viz.obs]\nstale_secs = -1.0"),
            ("viz.obs.stale_secs", "[viz.obs]\nstale_secs = 61.0"),
            ("batch.window_ms", "[batch]\nwindow_ms = 1001"),
            ("batch.ring_capacity", "[batch]\nring_capacity = 4194305"),
            ("games", "[games.\"CS2.EXE\"]\nsens = 1.0"),
            ("games", "[games.\"cs2\"]\nsens = 1.0"),
            ("games", "[games.\"c:/games/cs2.exe\"]\nsens = 1.0"),
        ];
        for (field, toml_text) in cases {
            let cfg: AppConfig = toml::from_str(toml_text).unwrap();
            let err = cfg.validate().unwrap_err();
            assert!(
                matches!(&err, ConfigError::Invalid { field: f, .. } if f == field),
                "expected Invalid({field}), got: {err}"
            );
        }
    }

    #[test]
    fn game_keys_must_be_the_bare_lowercase_exe_name() {
        assert_eq!(normalize_game_key("CS2.EXE"), "cs2.exe");
        assert_eq!(normalize_game_key(r"C:\Games\CS2.exe"), "cs2.exe");
        assert_eq!(normalize_game_key("/opt/games/cs2.exe"), "cs2.exe");
        assert_eq!(normalize_game_key(" cs2.exe "), "cs2.exe");
        assert!(game_key_problem("cs2.exe").is_ok());
        // The message names both what was written and what to write.
        let err = game_key_problem("CS2.EXE").unwrap_err();
        assert!(
            err.contains("\"CS2.EXE\"") && err.contains("\"cs2.exe\""),
            "{err}"
        );
        let err = game_key_problem("cs2").unwrap_err();
        assert!(err.contains("\"cs2.exe\""), "{err}");
        let err = game_key_problem(r"C:\Games\cs2.exe").unwrap_err();
        assert!(err.contains("path") && err.contains("\"cs2.exe\""), "{err}");
    }

    #[test]
    fn an_error_from_a_file_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemouse.toml");
        std::fs::write(&path, "mouse_cpi = 0.0\n").unwrap();
        let err = AppConfig::load(&path).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("invalid config: mouse_cpi"), "{text}");
        // The same value validated in memory has no file to name.
        let cfg: AppConfig = toml::from_str("mouse_cpi = 0.0").unwrap();
        let text = cfg.validate().unwrap_err().to_string();
        assert!(text.starts_with("invalid config: mouse_cpi"), "{text}");
    }

    #[test]
    fn utf16_and_other_non_utf8_files_say_how_to_fix_them() {
        let dir = tempfile::tempdir().unwrap();

        // What Notepad's "Unicode" and `... > telemouse.toml` in PowerShell
        // 5.1 produce: UTF-16 LE with a BOM.
        let path = dir.path().join("utf16.toml");
        let mut bytes = vec![0xFF, 0xFE];
        for b in "mouse_cpi = 1600.0\n".bytes() {
            bytes.extend_from_slice(&[b, 0]);
        }
        std::fs::write(&path, &bytes).unwrap();
        let err = AppConfig::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Encoding { .. }), "{err}");
        assert!(err.to_string().contains("UTF-16"), "{err}");

        // Big-endian too.
        let path = dir.path().join("utf16be.toml");
        std::fs::write(&path, [0xFE, 0xFF, 0x00, b'a']).unwrap();
        assert!(matches!(
            AppConfig::load(&path),
            Err(ConfigError::Encoding { .. })
        ));

        // Anything else that is not UTF-8 (a stray Latin-1 byte).
        let path = dir.path().join("latin1.toml");
        std::fs::write(&path, b"mouse_cpi = 1600.0 # caf\xe9\n").unwrap();
        let err = AppConfig::load(&path).unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn a_utf8_bom_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.toml");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"mouse_cpi = 3200.0\n");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap().mouse_cpi, 3200.0);
    }

    #[test]
    fn relative_paths_resolve_against_the_config_directory() {
        let base = Path::new("/opt/telemouse");
        let mut cfg = AppConfig::default();
        cfg.ctl.bin_dir = Some(PathBuf::from("bin"));
        cfg.resolve_paths(base);
        assert_eq!(cfg.recording.dir, base.join("recordings"));
        assert_eq!(cfg.ctl.log_dir, base.join("logs"));
        assert_eq!(cfg.ctl.bin_dir, Some(base.join("bin")));

        // Absolute paths and an unset bin_dir are left alone.
        let absolute = if cfg!(windows) {
            PathBuf::from(r"D:\recordings")
        } else {
            PathBuf::from("/srv/recordings")
        };
        let mut cfg = AppConfig::default();
        cfg.recording.dir = absolute.clone();
        cfg.resolve_paths(base);
        assert_eq!(cfg.recording.dir, absolute);
        assert_eq!(cfg.ctl.bin_dir, None);
    }

    #[test]
    fn only_non_default_fields_are_listed() {
        assert!(AppConfig::default().non_default_fields().is_empty());

        let cfg: AppConfig = toml::from_str(
            r#"
            mouse_cpi = 3200.0
            [kafka]
            enabled = true
            [viz]
            http_addr = "0.0.0.0:7879"
            [games."cs2.exe"]
            sens = 1.0
            "#,
        )
        .unwrap();
        let fields = cfg.non_default_fields();
        let names: Vec<&str> = fields.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec!["mouse_cpi", "kafka.enabled", "viz.http_addr", "games"]
        );
        assert_eq!(fields[2].1, "0.0.0.0:7879");
        assert_eq!(fields[3].1, "1 entries");
    }

    /// The shipped sample is the defaults written down. If a default moves
    /// and the sample does not, a fresh install silently runs on different
    /// numbers than a build from source.
    #[test]
    fn the_shipped_sample_is_exactly_the_defaults() {
        let text = include_str!("../../../telemouse.example.toml");
        let sample: AppConfig = toml::from_str(text).expect("the sample must parse");
        sample.validate().expect("the sample must validate");
        assert!(
            !sample.games.is_empty(),
            "the sample carries example [games] entries"
        );
        let expected = AppConfig {
            games: sample.games.clone(),
            ..AppConfig::default()
        };
        assert_eq!(
            sample, expected,
            "telemouse.example.toml has drifted from AppConfig::default()"
        );
    }
}
