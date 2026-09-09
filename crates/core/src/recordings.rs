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

/// What the capture agent knew when it stopped. Written as
/// `recordings/<session_id>.meta.json`; every field has a default so a
/// sidecar from an older agent still parses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionMeta {
    pub session_id: String,
    pub capture_version: String,
    pub started_utc_us: i64,
    pub ended_utc_us: i64,
    /// Why the run ended: `ctrl-c`, `duration`, `capture-thread-exited`,
    /// `shipping-thread-exited`, or `error`.
    pub exit: String,
    /// True if every worker thread joined without panicking.
    pub clean: bool,
    pub events: u64,
    pub batches: u64,
    pub markers: u64,
    pub ring_drops: u64,
    pub ring_high_water: u64,
    pub abs_frames: u64,
    /// Keyed by sink name (`udp`, `jsonl`, `kafka`); only the sinks that
    /// were enabled for the run appear.
    pub sinks: BTreeMap<String, SinkMeta>,
}

impl SessionMeta {
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
            started_utc_us: 1_756_000_000_000_000,
            ended_utc_us: 1_756_000_003_000_000,
            exit: "ctrl-c".into(),
            clean: true,
            events: 3_000,
            batches: 120,
            markers: 1,
            ring_drops: 0,
            ring_high_water: 45,
            abs_frames: 0,
            sinks,
        };
        let back = SessionMeta::from_json(&m.to_json_pretty().unwrap()).unwrap();
        assert_eq!(back, m);
        assert_eq!(m.losses(), vec![("kafka".to_string(), 16_904)]);
        assert_eq!(m.recording_lost(), 0);

        // A sidecar from an older agent that knew fewer fields still parses.
        let old = SessionMeta::from_json(r#"{"session_id":"s-0","events":5}"#).unwrap();
        assert_eq!(old.session_id, "s-0");
        assert_eq!(old.events, 5);
        assert!(old.sinks.is_empty());
        assert!(old.losses().is_empty());
    }
}
