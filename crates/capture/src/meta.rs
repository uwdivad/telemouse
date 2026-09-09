//! The session metadata sidecar: `recordings/<session_id>.meta.json`.
//!
//! Written once, when the agent stops, from the final counters. It is the
//! durable answer to "did this recording lose anything?" — ring drops, and
//! per-sink errors, drops and abandoned envelopes — next to the recording
//! itself, where `telemouse-analyze list` and the control panel can show it
//! without anyone having to find the agent's last log line.
//!
//! Building the document is pure ([`build`]); only [`write`] touches disk,
//! and a failed write is a warning, never a reason to exit non-zero: the
//! recording is already on disk by then.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use telemouse_core::recordings::{SessionMeta, SinkMeta, meta_file_name};

use crate::stats::StatsSnapshot;

/// Why the run ended, as it appears in the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Ctrl-C / Ctrl-Break, or the control panel's stop.
    Interrupt,
    /// `--duration-secs` elapsed.
    Duration,
    /// The raw-input thread left its message loop unexpectedly.
    CaptureThreadExited,
    /// The shipping thread returned unexpectedly.
    ShippingThreadExited,
    /// The context thread (foreground / cursor / pointer lock) panicked.
    ContextThreadExited,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Duration => "duration",
            Self::CaptureThreadExited => "capture-thread-exited",
            Self::ShippingThreadExited => "shipping-thread-exited",
            Self::ContextThreadExited => "context-thread-exited",
        }
    }
}

/// Everything the sidecar records besides the counters.
#[derive(Debug, Clone)]
pub struct RunInfo<'a> {
    pub session_id: &'a str,
    pub capture_version: &'a str,
    pub started_utc_us: i64,
    pub ended_utc_us: i64,
    pub exit: ExitReason,
    /// Every worker thread joined without a panic.
    pub clean: bool,
    /// Names of the sinks that were enabled for this run, in fan-out order.
    pub sinks: &'a [&'static str],
}

/// Assemble the sidecar from the final counters. Only the sinks named in
/// `run.sinks` get an entry, so a disabled sink is absent rather than a row
/// of zeroes that reads as "delivered everything".
pub fn build(run: &RunInfo<'_>, s: &StatsSnapshot) -> SessionMeta {
    let mut sinks = BTreeMap::new();
    for name in run.sinks {
        let m = match *name {
            "udp" => SinkMeta {
                errors: s.udp_errors,
                // A datagram nobody was listening for is not a loss of the
                // recording; oversized envelopes are.
                dropped: s.udp_oversized,
                abandoned: 0,
            },
            "jsonl" => SinkMeta {
                errors: s.jsonl_errors,
                dropped: s.jsonl_dropped,
                abandoned: s.jsonl_abandoned,
            },
            "kafka" => SinkMeta {
                errors: s.kafka_errors,
                dropped: s.kafka_dropped,
                abandoned: s.kafka_abandoned,
            },
            _ => SinkMeta::default(),
        };
        sinks.insert((*name).to_string(), m);
    }
    SessionMeta {
        session_id: run.session_id.to_string(),
        capture_version: run.capture_version.to_string(),
        started_utc_us: run.started_utc_us,
        ended_utc_us: run.ended_utc_us,
        exit: run.exit.as_str().to_string(),
        clean: run.clean,
        events: s.events,
        batches: s.batches,
        markers: s.markers,
        ring_drops: s.ring_drops,
        ring_high_water: s.ring_high_water,
        abs_frames: s.abs_frames,
        sinks,
    }
}

/// Write the sidecar next to the recording. Returns the path written.
pub fn write(dir: &Path, meta: &SessionMeta) -> Result<PathBuf> {
    let name = meta_file_name(&meta.session_id).with_context(|| {
        format!(
            "session id {:?} is not a valid recording id",
            meta.session_id
        )
    })?;
    let path = dir.join(name);
    let text = meta
        .to_json_pretty()
        .context("serialize session metadata")?;
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> StatsSnapshot {
        StatsSnapshot {
            events: 12_345,
            batches: 400,
            markers: 2,
            ring_drops: 3,
            ring_high_water: 45,
            abs_frames: 1,
            udp_errors: 0,
            udp_unreachable: 400,
            udp_oversized: 0,
            jsonl_errors: 0,
            jsonl_dropped: 0,
            jsonl_abandoned: 0,
            kafka_errors: 1,
            kafka_dropped: 397,
            kafka_abandoned: 3,
            ..Default::default()
        }
    }

    fn run<'a>(sinks: &'a [&'static str]) -> RunInfo<'a> {
        RunInfo {
            session_id: "s-20260908-120000-abcd",
            capture_version: "0.1.0",
            started_utc_us: 1_756_000_000_000_000,
            ended_utc_us: 1_756_000_010_000_000,
            exit: ExitReason::Interrupt,
            clean: true,
            sinks,
        }
    }

    #[test]
    fn only_enabled_sinks_appear_and_losses_are_summed() {
        let m = build(&run(&["udp", "jsonl", "kafka"]), &snapshot());
        assert_eq!(m.sinks.len(), 3);
        assert_eq!(m.sinks["kafka"].dropped, 397);
        assert_eq!(m.sinks["kafka"].abandoned, 3);
        assert_eq!(m.sinks["kafka"].errors, 1);
        // Unreachable datagrams are expected with no viz running: not a loss.
        assert_eq!(m.sinks["udp"].lost(), 0);
        assert_eq!(m.losses(), vec![("kafka".to_string(), 400)]);
        assert_eq!(m.recording_lost(), 0);
        assert_eq!(m.exit, "interrupt");
        assert!(m.clean);
        assert_eq!(m.events, 12_345);
        assert_eq!(m.ring_drops, 3);

        let m = build(&run(&["udp"]), &snapshot());
        assert_eq!(m.sinks.len(), 1);
        assert!(!m.sinks.contains_key("kafka"));
        assert!(m.losses().is_empty());
    }

    #[test]
    fn the_sidecar_lands_next_to_the_recording_and_parses_back() {
        let dir = std::env::temp_dir().join(format!("telemouse-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = build(&run(&["jsonl"]), &snapshot());
        let path = write(&dir, &m).unwrap();
        assert_eq!(path, dir.join("s-20260908-120000-abcd.meta.json"));
        let back = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsafe_session_id_is_refused_rather_than_written_anywhere() {
        let mut m = build(&run(&[]), &snapshot());
        m.session_id = "../escape".into();
        assert!(write(Path::new("."), &m).is_err());
    }

    #[test]
    fn exit_reasons_have_stable_names() {
        assert_eq!(ExitReason::Interrupt.as_str(), "interrupt");
        assert_eq!(ExitReason::Duration.as_str(), "duration");
        assert_eq!(
            ExitReason::CaptureThreadExited.as_str(),
            "capture-thread-exited"
        );
        assert_eq!(
            ExitReason::ShippingThreadExited.as_str(),
            "shipping-thread-exited"
        );
        assert_eq!(
            ExitReason::ContextThreadExited.as_str(),
            "context-thread-exited"
        );
    }
}
