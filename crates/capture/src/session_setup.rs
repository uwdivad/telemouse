//! Session identity and `SessionConfig` assembly.
//!
//! Deliberately pure: everything the Win32 side contributes (QPC frequency,
//! monitors, the wall-clock anchor) arrives as plain data in [`SessionEnv`], so
//! the assembly is testable from a fake environment.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, RandomState};

use telemouse_core::session::{GameSens, MonitorInfo};
use telemouse_core::{QpcAnchor, SessionConfig};

/// UTC microseconds since the Unix epoch, right now (the workspace-wide
/// definition, re-exported so the anchor and the drift check use it).
pub use telemouse_core::now_utc_us;

/// Everything the platform layer contributes to a session record.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEnv {
    pub anchor: QpcAnchor,
    /// Half-width of the QPC/UTC/QPC sandwich the anchor was taken from.
    pub anchor_uncertainty_us: Option<i64>,
    pub mouse_cpi: f64,
    /// HID pointing device names indexed by `RawEvent::device_ix`; index 0 is
    /// always [`crate::devices::UNKNOWN_DEVICE`].
    pub devices: Vec<String>,
    pub games: BTreeMap<String, GameSens>,
    pub monitors: Vec<MonitorInfo>,
    pub capture_version: String,
    /// `batch.coalesce_ms` the agent is running with.
    pub coalesce_ms: u64,
}

/// Build an anchor from a QPC/UTC/QPC sandwich.
///
/// A single "read QPC, read the wall clock" pair silently attributes the whole
/// gap between the two reads to the anchor. Bracketing the wall-clock read with
/// two QPC reads pins it to the midpoint and *measures* how wrong that can be:
/// the returned µs value is the half-width of the bracket, which bounds the
/// pairing error. Consumers get it as `SessionConfig.anchor_uncertainty_us`.
pub fn sandwich_anchor(
    qpc_before: u64,
    utc_us: i64,
    qpc_after: u64,
    qpc_freq: u64,
) -> (QpcAnchor, i64) {
    let freq = if qpc_freq == 0 { 1 } else { qpc_freq };
    let span = qpc_after.saturating_sub(qpc_before);
    let half = span / 2;
    let anchor = QpcAnchor {
        qpc: qpc_before.saturating_add(half),
        utc_us,
        qpc_freq: freq,
    };
    // Round up: an uncertainty of "0µs" would overstate what we know.
    let half_us = (half as u128 * 1_000_000).div_ceil(freq as u128) as i64;
    (anchor, half_us)
}

/// Take a fresh sandwich against the live clocks.
pub fn measure_anchor(qpc_freq: u64) -> (QpcAnchor, i64) {
    let before = crate::platform::qpc();
    let utc = now_utc_us();
    let after = crate::platform::qpc();
    sandwich_anchor(before, utc, after, qpc_freq)
}

/// How far the session anchor has drifted, in µs, at `fresh`'s instant:
/// positive means the wall clock has run ahead of what the anchor predicts.
pub fn anchor_drift_us(anchor: &QpcAnchor, fresh: &QpcAnchor) -> i64 {
    fresh.utc_us - anchor.qpc_to_utc_us(fresh.qpc)
}

/// Drift expressed as parts per million of the elapsed interval. 0 when no
/// time has passed (nothing to normalise against).
pub fn drift_ppm(drift_us: i64, elapsed_us: i64) -> f64 {
    if elapsed_us <= 0 {
        return 0.0;
    }
    drift_us as f64 * 1_000_000.0 / elapsed_us as f64
}

/// `s-YYYYMMDD-HHMMSS-xxxx` where `xxxx` is 4 hex digits of randomness, so two
/// sessions started in the same second still get distinct ids.
pub fn format_session_id(utc_us: i64, salt: u16) -> String {
    let stamp = match chrono::DateTime::from_timestamp_micros(utc_us) {
        Some(dt) => dt.format("%Y%m%d-%H%M%S").to_string(),
        None => "00000000-000000".to_string(),
    };
    format!("s-{stamp}-{salt:04x}")
}

/// Same, with the salt drawn from the std hasher's per-process random seed
/// (avoids pulling in an RNG crate for four hex digits).
pub fn new_session_id(utc_us: i64) -> String {
    let salt = RandomState::new().hash_one(utc_us) as u16;
    format_session_id(utc_us, salt)
}

pub fn build_session_config(session_id: String, env: SessionEnv) -> SessionConfig {
    SessionConfig {
        session_id,
        started_utc_us: env.anchor.utc_us,
        qpc_freq: env.anchor.qpc_freq,
        anchor: env.anchor,
        anchor_uncertainty_us: env.anchor_uncertainty_us,
        mouse_cpi: env.mouse_cpi,
        devices: env.devices,
        games: env.games,
        monitors: env.monitors,
        capture_version: env.capture_version,
        coalesce_ms: env.coalesce_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_env() -> SessionEnv {
        let mut games = BTreeMap::new();
        games.insert(
            "cs2.exe".to_string(),
            GameSens {
                sens: 1.25,
                yaw_coeff: 0.022,
                pitch_coeff: 0.022,
            },
        );
        SessionEnv {
            anchor: QpcAnchor {
                qpc: 5_000_000_000,
                utc_us: 1_756_000_000_000_000,
                qpc_freq: 10_000_000,
            },
            anchor_uncertainty_us: Some(3),
            mouse_cpi: 1600.0,
            devices: vec![
                crate::devices::UNKNOWN_DEVICE.to_string(),
                r"\\?\HID#VID_1532&PID_0099".to_string(),
            ],
            games,
            monitors: vec![MonitorInfo {
                width: 2560,
                height: 1440,
                refresh_hz: Some(240),
                primary: true,
            }],
            capture_version: "0.1.0".into(),
            coalesce_ms: 2,
        }
    }

    #[test]
    fn session_id_has_the_documented_shape() {
        // 2026-08-23T15:30:00Z
        let utc_us = 1_787_499_000_000_000;
        let id = format_session_id(utc_us, 0x0a1f);
        assert_eq!(id, "s-20260823-153000-0a1f");
    }

    #[test]
    fn session_id_pads_the_salt_to_four_hex_digits() {
        let id = format_session_id(0, 0x7);
        assert_eq!(id, "s-19700101-000000-0007");
        assert_eq!(id.len(), "s-YYYYMMDD-HHMMSS-xxxx".len());
    }

    #[test]
    fn generated_ids_parse_as_ids_and_vary_by_salt() {
        let a = new_session_id(now_utc_us());
        assert!(a.starts_with("s-"));
        assert_eq!(a.len(), 22);
        assert!(a.split('-').count() == 4);
        assert_ne!(format_session_id(0, 1), format_session_id(0, 2));
    }

    #[test]
    fn config_is_assembled_from_the_environment() {
        let env = fake_env();
        let cfg = build_session_config("s-test-0001".into(), env.clone());
        assert_eq!(cfg.session_id, "s-test-0001");
        assert_eq!(cfg.started_utc_us, env.anchor.utc_us);
        assert_eq!(cfg.qpc_freq, 10_000_000);
        assert_eq!(cfg.anchor, env.anchor);
        assert_eq!(cfg.mouse_cpi, 1600.0);
        assert_eq!(cfg.monitors, env.monitors);
        assert_eq!(cfg.capture_version, "0.1.0");
        // The games table survives so consumers can derive aim degrees later.
        assert_eq!(cfg.sens_for("CS2.exe").unwrap().sens, 1.25);
        // Device names ship in `device_ix` order, index 0 reserved for unknown.
        assert_eq!(cfg.devices[0], crate::devices::UNKNOWN_DEVICE);
        assert!(cfg.devices[1].contains("VID_1532"));
        assert_eq!(cfg.anchor_uncertainty_us, Some(3));
    }

    #[test]
    fn the_sandwich_anchors_on_the_midpoint_and_reports_its_half_width() {
        // 1000 ticks between the two QPC reads at 10MHz = 100µs of bracket.
        let (a, uncertainty) = sandwich_anchor(10_000, 1_756_000_000_000_000, 11_000, 10_000_000);
        assert_eq!(a.qpc, 10_500);
        assert_eq!(a.utc_us, 1_756_000_000_000_000);
        assert_eq!(a.qpc_freq, 10_000_000);
        assert_eq!(uncertainty, 50); // half of 100µs
    }

    #[test]
    fn a_tight_sandwich_still_reports_a_nonzero_bound_when_it_should() {
        // 1 tick apart: half a tick rounds *up* to 1µs rather than claiming
        // perfect knowledge... at 10MHz half a tick is 0.05µs -> 1µs.
        let (_, uncertainty) = sandwich_anchor(1_000, 0, 1_001, 10_000_000);
        assert_eq!(uncertainty, 0, "half of 1 tick truncates to 0 ticks");
        let (_, u2) = sandwich_anchor(1_000, 0, 1_041, 10_000_000);
        assert_eq!(u2, 2, "20 ticks = 2µs, rounded up");
        // Identical reads: no bracket, no uncertainty.
        let (a, u3) = sandwich_anchor(500, 7, 500, 10_000_000);
        assert_eq!((a.qpc, u3), (500, 0));
    }

    #[test]
    fn a_reversed_sandwich_degrades_instead_of_underflowing() {
        let (a, u) = sandwich_anchor(2_000, 5, 1_000, 10_000_000);
        assert_eq!((a.qpc, u), (2_000, 0));
    }

    #[test]
    fn drift_is_measured_against_what_the_anchor_predicts() {
        let a = QpcAnchor {
            qpc: 1_000_000,
            utc_us: 1_000_000_000,
            qpc_freq: 10_000_000,
        };
        // 60s later by QPC; the wall clock agrees exactly.
        let on_time = QpcAnchor {
            qpc: a.qpc + 600_000_000,
            utc_us: a.utc_us + 60_000_000,
            qpc_freq: a.qpc_freq,
        };
        assert_eq!(anchor_drift_us(&a, &on_time), 0);
        assert_eq!(drift_ppm(0, 60_000_000), 0.0);

        // The wall clock ran 600µs ahead over 60s => +10 ppm.
        let fast = QpcAnchor {
            utc_us: on_time.utc_us + 600,
            ..on_time
        };
        assert_eq!(anchor_drift_us(&a, &fast), 600);
        assert!((drift_ppm(600, 60_000_000) - 10.0).abs() < 1e-9);

        // And behind, for the sign.
        let slow = QpcAnchor {
            utc_us: on_time.utc_us - 1_200,
            ..on_time
        };
        assert_eq!(anchor_drift_us(&a, &slow), -1_200);
        assert!((drift_ppm(-1_200, 60_000_000) + 20.0).abs() < 1e-9);
    }

    #[test]
    fn drift_ppm_tolerates_a_zero_interval() {
        assert_eq!(drift_ppm(500, 0), 0.0);
        assert_eq!(drift_ppm(500, -1), 0.0);
    }

    #[test]
    fn a_live_sandwich_is_tight_and_self_consistent() {
        let freq = crate::platform::qpc_freq();
        let (a, uncertainty) = measure_anchor(freq);
        assert_eq!(a.qpc_freq, freq);
        assert!(a.qpc > 0);
        // Two adjacent clock reads should bracket well under a millisecond.
        assert!(uncertainty < 1_000, "anchor bracket was {uncertainty}µs");
    }

    #[test]
    fn started_utc_matches_the_anchor_so_replay_lines_up() {
        let cfg = build_session_config("s".into(), fake_env());
        assert_eq!(cfg.started_utc_us, cfg.anchor.utc_us);
        assert_eq!(cfg.anchor.qpc_to_utc_us(cfg.anchor.qpc), cfg.started_utc_us);
    }

    #[test]
    fn generated_ids_are_safe_recording_ids() {
        // The id becomes `recordings/<id>.jsonl`; every reader validates it
        // against the shared rule, so the writer must produce nothing else.
        let id = new_session_id(now_utc_us());
        assert!(telemouse_core::recordings::is_safe_id(&id), "{id}");
        assert!(telemouse_core::recordings::is_safe_id(&format_session_id(
            0, 0xffff
        )));
    }
}
