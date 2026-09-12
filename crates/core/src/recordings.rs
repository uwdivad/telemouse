//! The on-disk recording vocabulary shared by everything that names a
//! recording: the capture agent that writes `recordings/<session_id>.jsonl`,
//! the viz that serves it, the control panel that hands it to the analyzer,
//! and the analyzer that lists it.
//!
//! Two things live here. The **id rule** — the only characters a session id
//! may contain — is what keeps a recording name from ever becoming a path:
//! with this alphabet `dir.join(format!("{id}.jsonl"))` is always a direct
//! child of `dir`, on every platform, with no separators, no traversal, no
//! drive-relative (`C:x`) or NTFS-stream (`x:y`) spellings. Every server
//! applies the same rule, so what one lists another will serve.
//!
//! The **session metadata sidecar** (`<session_id>.meta.json`) is what the
//! capture agent writes next to a recording when it stops: the final
//! counters of the run — events, ring drops, and per-sink errors, drops and
//! abandoned envelopes. A Kafka outage that silently dropped batches used to
//! be discoverable only by re-deriving it from the JSONL weeks later; the
//! sidecar puts the numbers where `telemouse-analyze list` and the control
//! panel can show them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// File extension of a recording, without the dot.
pub const RECORDING_EXT: &str = "jsonl";
/// Suffix of the metadata sidecar written next to a recording at shutdown.
pub const META_SUFFIX: &str = ".meta.json";
/// Longest session id accepted. The agent's ids are 22 characters.
pub const MAX_ID_LEN: usize = 128;

/// True if `id` is shaped like a session id: 1–[`MAX_ID_LEN`] characters of
/// `[A-Za-z0-9_-]`, not starting with a dot (the alphabet already forbids
/// it, but the intent — no hidden files — is worth stating).
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// `<id>.jsonl` for a valid id, `None` otherwise.
pub fn recording_file_name(id: &str) -> Option<String> {
    is_safe_id(id).then(|| format!("{id}.{RECORDING_EXT}"))
}

/// `<id>.meta.json` for a valid id, `None` otherwise.
pub fn meta_file_name(id: &str) -> Option<String> {
    is_safe_id(id).then(|| format!("{id}{META_SUFFIX}"))
}

/// The session id of a recording file name (`s-1.jsonl` → `s-1`), or `None`
/// if the name is not `<safe id>.jsonl` exactly — so a name that came from a
/// request is validated and normalised in one step.
pub fn id_from_file_name(name: &str) -> Option<&str> {
    let id = name.strip_suffix(&format!(".{RECORDING_EXT}"))?;
    is_safe_id(id).then_some(id)
}

/// Per-sink counters at the end of a run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkMeta {
    /// Send/tick failures reported by the sink.
    pub errors: u64,
    /// Envelopes the sink refused because its queue was full or it had
    /// already failed.
    pub dropped: u64,
    /// Envelopes the sink accepted but never delivered (writer failure,
    /// broker failure, or the bounded shutdown drain running out).
    pub abandoned: u64,
}

impl SinkMeta {
    /// Anything this sink did not deliver.
    pub fn lost(&self) -> u64 {
        self.dropped.saturating_add(self.abandoned)
    }
}

/// Why a run ended — the vocabulary of [`SessionMeta::exit`].
///
/// Kept as an enum rather than a comment on a string because the value is
/// read by three programs (the analyzer's listing, the control panel, and
/// whoever greps the sidecars a month later) and written by one. A run that
/// is still going says `running`, so a sidecar written up-front and a
/// process that died without updating it are distinguishable from a clean
/// stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// The sidecar was written while the run was still going.
    Running,
    /// A console control event (Ctrl-C, Ctrl-Break, or a window close).
    Interrupt,
    /// The `--duration` the run was started with elapsed.
    Duration,
    /// A worker thread returned on its own.
    CaptureThreadExited,
    ShippingThreadExited,
    ContextThreadExited,
    /// A worker thread stopped making progress and teardown did not wait for
    /// it. The recording is as complete as that thread's last flush.
    CaptureThreadStalled,
    ShippingThreadStalled,
    ContextThreadStalled,
    /// The run ended on an error that is reported in the log.
    Error,
}

impl ExitReason {
    /// The kebab-case spelling that goes on disk.
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Running => "running",
            ExitReason::Interrupt => "interrupt",
            ExitReason::Duration => "duration",
            ExitReason::CaptureThreadExited => "capture-thread-exited",
            ExitReason::ShippingThreadExited => "shipping-thread-exited",
            ExitReason::ContextThreadExited => "context-thread-exited",
            ExitReason::CaptureThreadStalled => "capture-thread-stalled",
            ExitReason::ShippingThreadStalled => "shipping-thread-stalled",
            ExitReason::ContextThreadStalled => "context-thread-stalled",
            ExitReason::Error => "error",
        }
    }

    /// Every reason, for tests and for anything that lists the vocabulary.
    pub const ALL: &'static [ExitReason] = &[
        ExitReason::Running,
        ExitReason::Interrupt,
        ExitReason::Duration,
        ExitReason::CaptureThreadExited,
        ExitReason::ShippingThreadExited,
        ExitReason::ContextThreadExited,
        ExitReason::CaptureThreadStalled,
        ExitReason::ShippingThreadStalled,
        ExitReason::ContextThreadStalled,
        ExitReason::Error,
    ];
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ExitReason {
    type Err = UnknownExitReason;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| UnknownExitReason(s.to_string()))
    }
}

/// An `exit` string that is not part of the [`ExitReason`] vocabulary —
/// an older agent's spelling, or a hand-edited sidecar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown exit reason {0:?}")]
pub struct UnknownExitReason(pub String);

/// What the capture agent knew when it stopped. Written as
/// `recordings/<session_id>.meta.json`; every field has a default so a
/// sidecar from an older agent still parses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionMeta {
    pub session_id: String,
    pub capture_version: String,
    /// `debug` or `release` — the same capture code drops events at rates a
    /// release build does not, and "was this a debug build?" is the first
    /// question a surprising drop count raises.
    pub capture_profile: String,
    pub started_utc_us: i64,
    pub ended_utc_us: i64,
    /// Why the run ended, spelled as [`ExitReason::as_str`]. Kept a string
    /// on the wire so a sidecar written by a newer agent still parses here.
    pub exit: String,
    /// True if every worker thread joined without panicking.
    pub clean: bool,
    pub events: u64,
    pub batches: u64,
    pub markers: u64,
    pub ring_drops: u64,
    pub ring_high_water: u64,
    pub abs_frames: u64,
    /// QPC ticks per second on the capture host. Every `ts_qpc` in the
    /// recording is in these units, so a sidecar read on its own is still
    /// enough to convert the counters to time.
    pub qpc_freq: u64,
    /// Half-width of the QPC/UTC read sandwich at anchor time, in µs: how
    /// far the recording's whole timeline may be offset from true UTC.
    pub anchor_uncertainty_us: i64,
    /// Largest drift between the anchor's projection and a fresh QPC/UTC
    /// pair observed during the run, in µs. Small means the one anchor held
    /// for the whole session; large means the host's clock was disciplined
    /// mid-run and absolute timestamps late in the file drifted.
    pub max_anchor_drift_us: i64,
    /// Batch window the run used, in ms.
    pub window_ms: u64,
    /// Raw-input read coalescing window the run used, in ms.
    pub coalesce_ms: u64,
    /// Observed report rate of the mouse, in Hz, when it could be measured.
    /// A 1000Hz mouse that actually reported at 125Hz explains a suspiciously
    /// smooth recording.
    pub poll_hz: Option<f64>,
    /// Keyed by sink name (`udp`, `jsonl`, `kafka`); only the sinks that
    /// were enabled for the run appear.
    pub sinks: BTreeMap<String, SinkMeta>,
}

impl SessionMeta {
    /// The parsed [`ExitReason`], or `None` for a spelling this build does
    /// not know.
    pub fn exit_reason(&self) -> Option<ExitReason> {
        self.exit.parse().ok()
    }

    /// True while the run is still going — or, on a sidecar that outlived
    /// its process, true because the agent never got to write the real
    /// reason. Either way the counters are not final.
    pub fn is_unfinished(&self) -> bool {
        self.exit == ExitReason::Running.as_str()
    }
    /// Sinks that lost envelopes, as `name=lost` pairs — the one-line
    /// summary a listing shows. Empty when nothing was lost.
    pub fn losses(&self) -> Vec<(String, u64)> {
        self.sinks
            .iter()
            .filter(|(_, s)| s.lost() > 0)
            .map(|(n, s)| (n.clone(), s.lost()))
            .collect()
    }

    /// Whether the JSONL recording itself is known to be incomplete.
    pub fn recording_lost(&self) -> u64 {
        self.sinks.get("jsonl").map(SinkMeta::lost).unwrap_or(0)
    }

    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_ids_are_a_single_path_component_everywhere() {
        for ok in ["s-20260823-153000-0a1f", "demo-session", "abc_123", "x"] {
            assert!(is_safe_id(ok), "{ok:?}");
            assert_eq!(
                recording_file_name(ok).as_deref(),
                Some(format!("{ok}.jsonl").as_str())
            );
            assert_eq!(
                meta_file_name(ok).as_deref(),
                Some(format!("{ok}.meta.json").as_str())
            );
        }
        for bad in [
            "",
            "a b",
            "a/b",
            "a\\b",
            "a.b",
            "..",
            ".hidden",
            "C:x",      // drive-relative on Windows
            "x:stream", // NTFS alternate data stream
            "\\\\server\\share",
            "s-1\n",
            "ünïcode",
        ] {
            assert!(!is_safe_id(bad), "{bad:?}");
            assert_eq!(recording_file_name(bad), None, "{bad:?}");
            assert_eq!(meta_file_name(bad), None, "{bad:?}");
        }
        assert!(is_safe_id(&"x".repeat(MAX_ID_LEN)));
        assert!(!is_safe_id(&"x".repeat(MAX_ID_LEN + 1)));
    }

    #[test]
    fn file_names_round_trip_through_the_id() {
        assert_eq!(id_from_file_name("s-1.jsonl"), Some("s-1"));
        assert_eq!(
            id_from_file_name("demo-session.jsonl"),
            Some("demo-session")
        );
        for bad in [
            "s-1",           // no extension
            "s-1.JSONL",     // the writer never produces this
            "s-1.jsonl ",    // trailing space
            ".jsonl",        // empty id
            "../x.jsonl",    // traversal
            "sub/x.jsonl",   // separator
            "C:x.jsonl",     // drive-relative
            "x:y.jsonl",     // NTFS stream
            "s-1.meta.json", // the sidecar is not a recording
            "notes.txt",
        ] {
            assert_eq!(id_from_file_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn session_meta_round_trips_and_defaults() {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "kafka".to_string(),
            SinkMeta {
                errors: 1,
                dropped: 16_901,
                abandoned: 3,
            },
        );
        sinks.insert("jsonl".to_string(), SinkMeta::default());
        let m = SessionMeta {
            session_id: "s-1".into(),
            capture_version: "0.1.0".into(),
            capture_profile: "release".into(),
            started_utc_us: 1_756_000_000_000_000,
            ended_utc_us: 1_756_000_003_000_000,
            exit: ExitReason::Interrupt.as_str().into(),
            clean: true,
            events: 3_000,
            batches: 120,
            markers: 1,
            ring_drops: 0,
            ring_high_water: 45,
            abs_frames: 0,
            qpc_freq: 10_000_000,
            anchor_uncertainty_us: 12,
            max_anchor_drift_us: 340,
            window_ms: 25,
            coalesce_ms: 8,
            poll_hz: Some(1000.0),
            sinks,
        };
        let back = SessionMeta::from_json(&m.to_json_pretty().unwrap()).unwrap();
        assert_eq!(back, m);
        assert_eq!(m.losses(), vec![("kafka".to_string(), 16_904)]);
        assert_eq!(m.recording_lost(), 0);
        assert_eq!(m.exit_reason(), Some(ExitReason::Interrupt));
        assert!(!m.is_unfinished());

        // A sidecar from an older agent that knew fewer fields still parses.
        let old = SessionMeta::from_json(r#"{"session_id":"s-0","events":5}"#).unwrap();
        assert_eq!(old.session_id, "s-0");
        assert_eq!(old.events, 5);
        assert!(old.sinks.is_empty());
        assert!(old.losses().is_empty());
        assert_eq!(old.qpc_freq, 0);
        assert_eq!(old.poll_hz, None);
    }

    #[test]
    fn exit_reasons_round_trip_through_their_on_disk_spelling() {
        for r in ExitReason::ALL {
            assert_eq!(r.as_str().parse::<ExitReason>().unwrap(), *r);
            assert_eq!(r.to_string(), r.as_str());
            assert!(
                r.as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{r} is not kebab-case"
            );
        }
        // The spellings the capture agent already writes must keep parsing.
        for s in [
            "interrupt",
            "duration",
            "capture-thread-exited",
            "shipping-thread-exited",
            "context-thread-exited",
        ] {
            assert!(s.parse::<ExitReason>().is_ok(), "{s}");
        }
        // An unknown spelling is data, not a panic.
        let err = "ctrl-c".parse::<ExitReason>().unwrap_err();
        assert_eq!(err.to_string(), "unknown exit reason \"ctrl-c\"");
    }

    #[test]
    fn a_sidecar_of_a_live_run_reads_as_unfinished() {
        let running = SessionMeta {
            exit: ExitReason::Running.as_str().into(),
            ..Default::default()
        };
        assert!(running.is_unfinished());
        assert_eq!(running.exit_reason(), Some(ExitReason::Running));

        // An old sidecar with no exit at all is not claimed to be running.
        let empty = SessionMeta::default();
        assert!(!empty.is_unfinished());
        assert_eq!(empty.exit_reason(), None);
    }
}
