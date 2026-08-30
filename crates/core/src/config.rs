//! `telemouse.toml` — one config file shared by the capture agent (which
//! reads all of it) and the viz/analyze tools (which read the addresses and
//! game table).

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
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid config: {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchConfig {
    /// Batch window in milliseconds: how long the shipping thread
    /// accumulates events before one batch goes to every sink. It sets the
    /// live-viz latency floor (a batch's first event is this old when it
    /// leaves) and, with it, the batches/s that the UDP hop, the recording
    /// and every WebSocket client pay for — each batch costs ~150µs of
    /// kernel CPU across the pipeline whatever it holds. 50 (20 batches/s)
    /// is the default since the 2026-08-29 CPU pass; the original 25 halves
    /// live latency for double that cost. Recording contents and analysis
    /// do not depend on it.
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
}

impl Default for CtlConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:7880".into(),
            bin_dir: None,
            stop_grace_secs: 5,
            log_dir: PathBuf::from("logs"),
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
}

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
            window_ms: 50,
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
            buffer_ms: 55,
            grid: true,
            legend: false,
            labels: false,
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

    pub fn validate(&self) -> Result<(), ConfigError> {
        fn invalid(field: &'static str, reason: impl Into<String>) -> ConfigError {
            ConfigError::Invalid {
                field,
                reason: reason.into(),
            }
        }
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
            return Err(invalid("viz.obs.scale", "must be between 0.5 and 4"));
        }
        if !(self.trail_secs.is_finite() && (0.3..=12.0).contains(&self.trail_secs)) {
            return Err(invalid("viz.obs.trail_secs", "must be between 0.3 and 12"));
        }
        if !(10..=200).contains(&self.buffer_ms) {
            return Err(invalid("viz.obs.buffer_ms", "must be between 10 and 200"));
        }
        Ok(())
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject values that would silently corrupt metrics or destabilize the
    /// pipeline. Called by [`Self::load`]; defaults always pass.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn invalid(field: &'static str, reason: impl Into<String>) -> ConfigError {
            ConfigError::Invalid {
                field,
                reason: reason.into(),
            }
        }
        if !(self.mouse_cpi.is_finite() && self.mouse_cpi > 0.0) {
            return Err(invalid(
                "mouse_cpi",
                format!("must be a positive number, got {}", self.mouse_cpi),
            ));
        }
        if self.batch.window_ms == 0 {
            return Err(invalid("batch.window_ms", "must be at least 1ms"));
        }
        if self.batch.max_events == 0 {
            return Err(invalid("batch.max_events", "must be at least 1"));
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
                "must be at least batch.max_events",
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
                "kafka is enabled but no brokers are listed",
            ));
        }
        self.viz.obs.validate()?;
        if self.ctl.stop_grace_secs > MAX_STOP_GRACE_SECS {
            return Err(invalid(
                "ctl.stop_grace_secs",
                format!("must be at most {MAX_STOP_GRACE_SECS}s"),
            ));
        }
        for (game, g) in &self.games {
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
        // 2026-08-29 CPU pass: 50ms batches (20/s) and an 8ms read cadence.
        assert_eq!(c.batch.window_ms, 50);
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
        assert_eq!(c.ctl.stop_grace_secs, 5);
        assert_eq!(c.ctl.log_dir, Path::new("logs"));

        let c: AppConfig = toml::from_str(
            "[ctl]\nhttp_addr = \"127.0.0.1:9000\"\nbin_dir = \"target/release\"\nstop_grace_secs = 2\nlog_dir = \"var/log\"\n",
        )
        .unwrap();
        assert_eq!(c.ctl.http_addr, "127.0.0.1:9000");
        assert_eq!(c.ctl.bin_dir.as_deref(), Some(Path::new("target/release")));
        assert_eq!(c.ctl.stop_grace_secs, 2);
        assert_eq!(c.ctl.log_dir, Path::new("var/log"));
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
        assert_eq!(c.batch.window_ms, 50); // default preserved
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
}
