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
    Invalid {
        field: &'static str,
        reason: String,
    },
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
    /// Keyed by lowercase process name (e.g. "cs2.exe").
    pub games: BTreeMap<String, GameSens>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchConfig {
    /// Batch window in milliseconds (the plan's 25ms).
    pub window_ms: u64,
    pub max_events: usize,
    /// SPSC ring-buffer capacity in events.
    pub ring_capacity: usize,
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
        }
    }
}

impl Default for UdpConfig {
    fn default() -> Self {
        Self { enabled: true, addr: "127.0.0.1:7878".into() }
    }
}

impl Default for KafkaConfig {
    fn default() -> Self {
        Self { enabled: false, brokers: vec!["127.0.0.1:9092".into()] }
    }
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self { enabled: true, dir: PathBuf::from("recordings") }
    }
}

impl Default for VizConfig {
    fn default() -> Self {
        Self { http_addr: "127.0.0.1:7879".into() }
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
            ConfigError::Invalid { field, reason: reason.into() }
        }
        if !(self.mouse_cpi.is_finite() && self.mouse_cpi > 0.0) {
            return Err(invalid("mouse_cpi", format!("must be a positive number, got {}", self.mouse_cpi)));
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
                format!("exceeds the UDP datagram budget of {}", crate::wire::MAX_EVENTS_PER_BATCH),
            ));
        }
        if self.batch.ring_capacity < self.batch.max_events {
            return Err(invalid("batch.ring_capacity", "must be at least batch.max_events"));
        }
        if self.kafka.enabled && self.kafka.brokers.is_empty() {
            return Err(invalid("kafka.brokers", "kafka is enabled but no brokers are listed"));
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
        assert_eq!(c.batch.window_ms, 25);
        assert!(c.udp.enabled);
        assert!(!c.kafka.enabled);
        assert!(c.recording.enabled);
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
            ("batch.ring_capacity", "[batch]\nmax_events = 448\nring_capacity = 16"),
            ("kafka.brokers", "[kafka]\nenabled = true\nbrokers = []"),
            ("games", "[games.\"a.exe\"]\nsens = -2.0"),
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
