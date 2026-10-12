//! The per-session record: what a consumer needs, besides the batches, to
//! turn raw counts into time, centimetres and aim-space degrees. One
//! [`SessionConfig`] opens every recording and goes to `mouse.sessions`;
//! [`Marker`]s annotate the timeline. Loaders call
//! [`SessionConfig::validate`] before trusting the numbers in one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::clock::QpcAnchor;

/// Source-engine `m_yaw` / `m_pitch` default: degrees per count per unit
/// sensitivity. The `[games]` coefficients default to it.
pub const DEFAULT_SOURCE_COEFF: f64 = 0.022;

/// Per-game aim-space conversion: `degrees = counts * sens * coeff`.
/// For CS2/Source games the coefficient is [`DEFAULT_SOURCE_COEFF`].
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
    DEFAULT_SOURCE_COEFF
}

/// Points in a Windows `SmoothMouse{X,Y}Curve`.
pub const CURVE_POINTS: usize = 5;
/// Bytes in the registry value: one 8-byte entry per point.
const CURVE_BYTES: usize = CURVE_POINTS * 8;
/// Scale of a 16.16 fixed-point number.
const FIXED_16_16: f64 = 65536.0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: Option<u32>,
    pub primary: bool,
}

/// The Windows pointer settings in force when the session started: what the
/// desktop cursor makes of the raw counts on the wire.
///
/// Raw Input is pre-acceleration, so nothing in a batch depends on any of
/// this — a game reading raw input never sees it. It is here for a consumer
/// that wants to show *where the cursor went* (the viz's pointer view), which
/// has to re-run Windows' pointer ballistics over the counts and needs the
/// inputs Windows had: the "Enhance pointer precision" switch, the pointer
/// speed slider, the transfer curve and the screen DPI.
///
/// Absent from recordings made before it was collected, and from any
/// platform that is not Windows; a consumer then assumes
/// [`PointerSettings::windows_defaults`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointerSettings {
    /// "Enhance pointer precision" (the acceleration flag `SPI_GETMOUSE`
    /// reports). On, the curve below applies; off, the pointer moves the
    /// counts times the slider's linear multiplier.
    pub enhance_pointer_precision: bool,
    /// `SPI_GETMOUSESPEED`, 1..=20. The control panel's slider has eleven
    /// notches at 1, 2, 4, 6, …, 20; 10 is the middle one (1:1 with
    /// precision off).
    pub speed: u32,
    /// `SmoothMouseXCurve`: the five inflection points' mouse speed, in
    /// inches per second as Windows reckons it, decoded from 16.16 fixed
    /// point. The first point is always 0.
    pub curve_x: [f64; CURVE_POINTS],
    /// `SmoothMouseYCurve`: the matching pointer speeds, same units.
    pub curve_y: [f64; CURVE_POINTS],
    /// System DPI (`GetDpiForSystem`): 96 at 100% display scaling. Enters the
    /// curve's pointer-speed → pixels conversion.
    pub screen_dpi: u32,
}

/// Windows' default `SmoothMouseXCurve` (mouse speed, in/s), exactly as the
/// 16.16 registry bytes decode: 0, 0.43, 1.25, 3.86, 40. Unchanged since XP.
pub const WINDOWS_CURVE_X: [f64; CURVE_POINTS] = [
    0.0,
    0x6e15 as f64 / FIXED_16_16,
    1.25,
    0x3dc29 as f64 / FIXED_16_16,
    40.0,
];
/// Windows' default `SmoothMouseYCurve` (pointer speed, in/s): 0, 1.37, 5.3,
/// 24.3, 568. A machine whose display scaling has ever changed carries a
/// uniformly rescaled copy of this instead (Windows rewrites it), so "not
/// the default" is common and not a customisation.
pub const WINDOWS_CURVE_Y: [f64; CURVE_POINTS] = [
    0.0,
    0x15eb8 as f64 / FIXED_16_16,
    0x54ccd as f64 / FIXED_16_16,
    0x184ccd as f64 / FIXED_16_16,
    568.0,
];

impl PointerSettings {
    /// A fresh Windows install: precision on, slider in the middle, the
    /// default curve, 100% scaling.
    pub fn windows_defaults() -> Self {
        Self {
            enhance_pointer_precision: true,
            speed: 10,
            curve_x: WINDOWS_CURVE_X,
            curve_y: WINDOWS_CURVE_Y,
            screen_dpi: 96,
        }
    }

    /// Decode a `SmoothMouse{X,Y}Curve` registry value: five 8-byte
    /// little-endian entries whose low 32 bits are a 16.16 fixed-point
    /// number (the high 32 are padding). Anything but exactly forty bytes,
    /// or a curve that does not start at 0 or is not non-decreasing, is
    /// `None` — a caller falls back to the defaults rather than ship a
    /// curve the model would misread.
    pub fn decode_curve(bytes: &[u8]) -> Option<[f64; CURVE_POINTS]> {
        if bytes.len() != CURVE_BYTES {
            return None;
        }
        let mut out = [0.0; CURVE_POINTS];
        for (i, chunk) in bytes.as_chunks::<8>().0.iter().enumerate() {
            let fixed = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            out[i] = fixed as f64 / FIXED_16_16;
        }
        if out[0] != 0.0 || out.windows(2).any(|w| w[1] < w[0]) {
            return None;
        }
        Some(out)
    }
}

/// Produced once per session to the compacted `mouse.sessions` topic (and the
/// local recording). Everything a consumer needs to reconstruct physical cm
/// and aim-space degrees from raw counts, later.
///
/// Deliberately *not* `deny_unknown_fields`: [`crate::wire::Envelope::from_json`]'s
/// fast path parses this struct straight from a document that still carries
/// the envelope's `"type"` key, and an older reader must accept a newer
/// agent's extra fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub session_id: String,
    /// UTC µs when the session started. A copy of `anchor.utc_us`, kept for
    /// readers that want it without the anchor; [`Self::anchor`] is the
    /// authority and [`Self::validate`] checks the two agree.
    pub started_utc_us: i64,
    /// QPC ticks per second. A copy of `anchor.qpc_freq`; [`Self::anchor`]
    /// is the authority and [`Self::validate`] checks the two agree.
    pub qpc_freq: u64,
    /// The QPC↔UTC mapping every timestamp in the session goes through.
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
    /// The desktop's pointer settings at session start (see
    /// [`PointerSettings`]). Absent for recordings made before this was
    /// collected and off Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<PointerSettings>,
}

/// Longest marker label accepted, in characters (not bytes): one line in a
/// report, short enough for the panel's input and the viz's timeline. The
/// single limit ctl, the capture agent's stdin reader, the MCP server and
/// the analyzer's summary all apply.
pub const MAX_MARKER_LABEL_CHARS: usize = 120;

/// Why a session record cannot be trusted to convert counts and ticks.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SessionError {
    /// The anchor's QPC frequency is zero: no tick can be mapped to time.
    #[error("session {session_id:?}: anchor.qpc_freq is 0, so no timestamp can be converted")]
    ZeroQpcFreq { session_id: String },
    /// `mouse_cpi` is zero, negative, NaN or infinite: no count can be
    /// converted to distance.
    #[error("session {session_id:?}: mouse_cpi must be a positive number, got {mouse_cpi}")]
    BadMouseCpi { session_id: String, mouse_cpi: f64 },
    /// The top-level `qpc_freq` copy disagrees with the anchor's.
    #[error(
        "session {session_id:?}: qpc_freq ({qpc_freq}) disagrees with anchor.qpc_freq ({anchor})"
    )]
    QpcFreqMismatch {
        session_id: String,
        qpc_freq: u64,
        anchor: u64,
    },
    /// The top-level `started_utc_us` copy disagrees with the anchor's.
    #[error(
        "session {session_id:?}: started_utc_us ({started_utc_us}) disagrees with \
         anchor.utc_us ({anchor})"
    )]
    StartMismatch {
        session_id: String,
        started_utc_us: i64,
        anchor: i64,
    },
}

impl SessionConfig {
    /// Look up aim conversion for a process name, normalised the way the
    /// `[games]` keys are (bare file name, lowercase; see
    /// [`crate::config`]).
    pub fn sens_for(&self, process: &str) -> Option<&GameSens> {
        self.games.get(&crate::config::normalize_game_key(process))
    }

    /// Reject a session record whose numbers would make every derived value
    /// wrong: a zero QPC frequency, a non-positive or non-finite
    /// `mouse_cpi`, or top-level copies of the anchor that disagree with it.
    /// The conversions themselves never panic on such a record; this is for
    /// a loader that should refuse it with a message rather than report
    /// nonsense.
    pub fn validate(&self) -> Result<(), SessionError> {
        let session_id = || self.session_id.clone();
        if !self.anchor.is_valid() {
            return Err(SessionError::ZeroQpcFreq {
                session_id: session_id(),
            });
        }
        if !(self.mouse_cpi.is_finite() && self.mouse_cpi > 0.0) {
            return Err(SessionError::BadMouseCpi {
                session_id: session_id(),
                mouse_cpi: self.mouse_cpi,
            });
        }
        if self.qpc_freq != self.anchor.qpc_freq {
            return Err(SessionError::QpcFreqMismatch {
                session_id: session_id(),
                qpc_freq: self.qpc_freq,
                anchor: self.anchor.qpc_freq,
            });
        }
        if self.started_utc_us != self.anchor.utc_us {
            return Err(SessionError::StartMismatch {
                session_id: session_id(),
                started_utc_us: self.started_utc_us,
                anchor: self.anchor.utc_us,
            });
        }
        Ok(())
    }
}

/// A manual annotation produced by the global hotkey (and later, game-state
/// integrations). Goes to `mouse.markers` so game context can evolve without
/// touching the hot batch schema.
///
/// Deliberately *not* `deny_unknown_fields`: the envelope fast path parses it
/// from a document that still carries the `"type"` key.
///
/// `label` is at most [`MAX_MARKER_LABEL_CHARS`] characters wherever one is
/// accepted (ctl's API, the capture agent's stdin, the MCP tool).
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
            pointer: Some(PointerSettings {
                speed: 12,
                ..PointerSettings::windows_defaults()
            }),
        }
    }

    /// The default curves as the registry stores them (the bytes Windows
    /// ships in `HKCU\Control Panel\Mouse`).
    const REG_X: [u8; 40] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x15, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x40, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x29, 0xdc, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const REG_Y: [u8; 40] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0xb8, 0x5e, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0xcd, 0x4c, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0xcd, 0x4c, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x38, 0x02, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn the_registry_default_curves_decode_to_the_documented_points() {
        // The constants are the bytes' exact value, so a machine on the
        // default curve compares equal to them.
        assert_eq!(PointerSettings::decode_curve(&REG_X), Some(WINDOWS_CURVE_X));
        assert_eq!(PointerSettings::decode_curve(&REG_Y), Some(WINDOWS_CURVE_Y));
        // ...and they are the documented points to the precision they are
        // documented at.
        for (got, want) in WINDOWS_CURVE_X
            .iter()
            .zip([0.0, 0.43001, 1.25, 3.86001, 40.0])
        {
            assert!((got - want).abs() < 1e-4, "x {got} vs {want}");
        }
        for (got, want) in WINDOWS_CURVE_Y
            .iter()
            .zip([0.0, 1.37, 5.30001, 24.30001, 568.0])
        {
            assert!((got - want).abs() < 1e-4, "y {got} vs {want}");
        }
    }

    #[test]
    fn a_malformed_curve_is_refused_rather_than_misread() {
        assert_eq!(PointerSettings::decode_curve(&REG_X[..32]), None, "short");
        assert_eq!(PointerSettings::decode_curve(&[0u8; 48]), None, "long");
        // Does not start at zero.
        let mut shifted = REG_X;
        shifted[0] = 1;
        assert_eq!(PointerSettings::decode_curve(&shifted), None);
        // Not monotonic: the fourth point below the third.
        let mut dip = REG_Y;
        dip[24..28].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        assert_eq!(PointerSettings::decode_curve(&dip), None);
        // A flat curve (the "no acceleration" registry fix) is fine.
        assert_eq!(PointerSettings::decode_curve(&[0u8; 40]), Some([0.0; 5]));
    }

    #[test]
    fn pointer_settings_default_to_a_fresh_windows_install() {
        let p = PointerSettings::windows_defaults();
        assert!(p.enhance_pointer_precision);
        assert_eq!((p.speed, p.screen_dpi), (10, 96));
        assert_eq!(p.curve_x[0], 0.0);
        assert_eq!(p.curve_y, WINDOWS_CURVE_Y);
    }

    #[test]
    fn pointer_settings_ride_the_session_json_and_are_optional() {
        let c = cfg();
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"pointer\":{\"enhance_pointer_precision\":true,\"speed\":12,"));
        let back: SessionConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.pointer, c.pointer);

        let none = SessionConfig {
            pointer: None,
            ..cfg()
        };
        let s = serde_json::to_string(&none).unwrap();
        assert!(!s.contains("\"pointer\""), "{s}");
        let mut v: serde_json::Value = serde_json::to_value(cfg()).unwrap();
        v.as_object_mut().unwrap().remove("pointer");
        let back: SessionConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.pointer, None);
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
    fn game_sens_coeff_defaults_apply() {
        let g: GameSens = serde_json::from_str(r#"{"sens": 2.0}"#).unwrap();
        assert_eq!(g.yaw_coeff, DEFAULT_SOURCE_COEFF);
        assert_eq!(g.pitch_coeff, DEFAULT_SOURCE_COEFF);
    }

    #[test]
    fn a_well_formed_session_validates() {
        cfg().validate().unwrap();
    }

    #[test]
    fn a_corrupt_session_is_refused_with_a_reason() {
        let mut zero = cfg();
        zero.anchor.qpc_freq = 0;
        zero.qpc_freq = 0;
        assert!(matches!(
            zero.validate(),
            Err(SessionError::ZeroQpcFreq { .. })
        ));
        assert!(zero.validate().unwrap_err().to_string().contains("s-1"));

        for cpi in [0.0, -1600.0, f64::NAN, f64::INFINITY] {
            let c = SessionConfig {
                mouse_cpi: cpi,
                ..cfg()
            };
            assert!(
                matches!(c.validate(), Err(SessionError::BadMouseCpi { .. })),
                "{cpi}"
            );
        }

        let mut freq = cfg();
        freq.qpc_freq = 3_579_545;
        assert!(matches!(
            freq.validate(),
            Err(SessionError::QpcFreqMismatch { .. })
        ));

        let mut start = cfg();
        start.started_utc_us += 1;
        assert!(matches!(
            start.validate(),
            Err(SessionError::StartMismatch { .. })
        ));
    }
}
