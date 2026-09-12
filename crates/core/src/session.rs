use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::clock::QpcAnchor;

/// Per-game aim-space conversion: `degrees = counts * sens * coeff`.
/// For CS2/Source games the coefficient is 0.022 (m_yaw default).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GameSens {
    pub sens: f64,
    /// Degrees per count per unit sensitivity, horizontal.
    #[serde(default = "default_coeff")]
    pub yaw_coeff: f64,
    /// Degrees per count per unit sensitivity, vertical.
    #[serde(default = "default_coeff")]
    pub pitch_coeff: f64,
}

fn default_coeff() -> f64 {
    0.022
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: Option<u32>,
    pub primary: bool,
}

/// Produced once per session to the compacted `mouse.sessions` topic (and the
/// local recording). Everything a consumer needs to reconstruct physical cm
/// and aim-space degrees from raw counts, later.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub session_id: String,
    /// UTC µs when the session started (equals `anchor.utc_us`).
    pub started_utc_us: i64,
    pub qpc_freq: u64,
    pub anchor: QpcAnchor,
    /// Half-width of the QPC/UTC read sandwich taken at anchor time, in µs.
    /// Bounds how far off the anchor's pairing can be; `None` for recordings
    /// made before this was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_uncertainty_us: Option<i64>,
    /// Mouse resolution in counts per inch.
    pub mouse_cpi: f64,
    /// HID pointing devices present at session start, in [`crate::RawEvent::device_ix`]
    /// order. Empty for recordings made before device tracking existed.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Keyed by lowercase process name (e.g. "cs2.exe").
    pub games: BTreeMap<String, GameSens>,
    pub monitors: Vec<MonitorInfo>,
    pub capture_version: String,
    /// Raw-input read coalescing window the agent ran with, in ms. Within one
    /// drain only the first report's arrival time is observed; the others are
    /// spaced by the estimated report interval, so `ts_qpc` is exact to about
    /// this many ms. 0 (and recordings made before this existed) means every
    /// report was stamped at its own wake.
    #[serde(default)]
    pub coalesce_ms: u64,
    /// Batch window the agent ran with, in ms. A replay can only reproduce
    /// the live view's timing if it knows how long a batch was allowed to
    /// accumulate; 0 means a recording made before this was written down.
    #[serde(default)]
    pub window_ms: u64,
    /// Event cap per batch the agent ran with. With `window_ms` it explains
    /// every batch boundary in the recording: a short batch was capped, not
    /// a gap in the input.
    #[serde(default)]
    pub max_events: usize,
    /// The OS the capture ran on, e.g. `"Windows 10.0.19045"`. Absent for
    /// recordings made before this was collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
}

impl SessionConfig {
    /// Look up aim conversion for a process name (case-insensitive).
    pub fn sens_for(&self, process: &str) -> Option<&GameSens> {
        self.games.get(&process.to_ascii_lowercase())
    }

    /// Record the batching the agent is running with. Builder-shaped so a
    /// caller that builds the rest of the struct by hand can add these two
    /// without naming every other field.
    pub fn with_batch(mut self, window_ms: u64, max_events: usize) -> Self {
        self.window_ms = window_ms;
        self.max_events = max_events;
        self
    }

    /// Record the host OS.
    pub fn with_os(mut self, os: impl Into<String>) -> Self {
        self.os = Some(os.into());
        self
    }
}

/// A manual annotation produced by the global hotkey (and later, game-state
/// integrations). Goes to `mouse.markers` so game context can evolve without
/// touching the hot batch schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub session_id: String,
    pub seq_no: u64,
    pub ts_qpc: u64,
    pub ts_utc_us: i64,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SessionConfig {
        let mut games = BTreeMap::new();
        games.insert(
            "cs2.exe".to_string(),
            GameSens {
                sens: 1.1,
                yaw_coeff: 0.022,
                pitch_coeff: 0.022,
            },
        );
        SessionConfig {
            session_id: "s-1".into(),
            started_utc_us: 1_756_000_000_000_000,
            qpc_freq: 10_000_000,
            anchor: QpcAnchor {
                qpc: 1_000,
                utc_us: 1_756_000_000_000_000,
                qpc_freq: 10_000_000,
            },
            anchor_uncertainty_us: Some(12),
            mouse_cpi: 1600.0,
            devices: vec![r"\\?\HID#VID_1532&PID_0099".into()],
            games,
            monitors: vec![MonitorInfo {
                width: 2560,
                height: 1440,
                refresh_hz: Some(240),
                primary: true,
            }],
            capture_version: "0.1.0".into(),
            coalesce_ms: 2,
            window_ms: 25,
            max_events: 448,
            os: Some("Windows 10.0.19045".into()),
        }
    }

    #[test]
    fn sens_lookup_is_case_insensitive() {
        let c = cfg();
        assert!(c.sens_for("CS2.EXE").is_some());
        assert!(c.sens_for("notepad.exe").is_none());
    }

    #[test]
    fn json_roundtrip() {
        let c = cfg();
        let s = serde_json::to_string(&c).unwrap();
        let back: SessionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn pre_coalescing_session_json_still_parses() {
        let mut v: serde_json::Value = serde_json::to_value(cfg()).unwrap();
        v.as_object_mut().unwrap().remove("coalesce_ms");
        let back: SessionConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.coalesce_ms, 0);
    }

    /// A recording from an agent that knew nothing about the batching fields
    /// still loads: they default, they do not fail the line.
    #[test]
    fn session_json_without_the_batching_fields_still_parses() {
        let mut v: serde_json::Value = serde_json::to_value(cfg()).unwrap();
        let obj = v.as_object_mut().unwrap();
        for key in ["window_ms", "max_events", "os"] {
            obj.remove(key);
        }
        let back: SessionConfig = serde_json::from_value(v).unwrap();
        assert_eq!((back.window_ms, back.max_events), (0, 0));
        assert_eq!(back.os, None);
    }

    #[test]
    fn an_unset_os_is_left_out_of_the_json() {
        let c = SessionConfig { os: None, ..cfg() };
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("\"os\""), "{s}");
        assert!(s.contains("\"window_ms\""));
    }

    #[test]
    fn builders_fill_in_the_run_parameters() {
        let c = SessionConfig {
            session_id: "s-2".into(),
            ..Default::default()
        }
        .with_batch(50, 448)
        .with_os("Windows 10.0.19045");
        assert_eq!(c.session_id, "s-2");
        assert_eq!((c.window_ms, c.max_events), (50, 448));
        assert_eq!(c.os.as_deref(), Some("Windows 10.0.19045"));
    }

    #[test]
    fn game_sens_coeff_defaults_apply() {
        let g: GameSens = serde_json::from_str(r#"{"sens": 2.0}"#).unwrap();
        assert_eq!(g.yaw_coeff, 0.022);
        assert_eq!(g.pitch_coeff, 0.022);
    }
}
