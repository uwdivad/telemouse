//! The session metadata sidecar: `recordings/<session_id>.meta.json`.
//!
//! Written *during* the run, not only after it: the first context tick puts
//! one on disk with `exit = "running"`, every 5s report refreshes it, and
//! shutdown writes the final one. A run that is killed outright therefore
//! still leaves counters within five seconds of the truth, flagged as
//! unfinished ([`SessionMeta::is_unfinished`]) so nobody reads them as final.
//!
//! It is the durable answer to "did this recording lose anything?" — ring
//! drops, and per-sink errors, drops and abandoned envelopes — next to the
//! recording itself, where `telemouse-analyze list` and the control panel can
//! show it without anyone having to find the agent's last log line.
//!
//! Building the document is pure ([`build`]); only [`write`] touches disk, via
//! a temp file and a rename so a reader never sees half a document, and a
//! failed write is one warning, never a panic and never a reason to exit
//! non-zero: the recording is already on disk by then.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use telemouse_core::recordings::{ExitReason, SessionMeta, SinkMeta, meta_file_name};

use crate::PROFILE;
use crate::stats::{Stats, StatsSnapshot, poll_hz};

/// Everything the sidecar records besides the counters.
#[derive(Debug, Clone)]
pub struct RunInfo<'a> {
    pub session_id: &'a str,
    pub capture_version: &'a str,
    /// [`PROFILE`] of the build that produced the recording.
    pub capture_profile: &'a str,
    pub started_utc_us: i64,
    pub ended_utc_us: i64,
    pub exit: ExitReason,
    /// Every worker thread joined without a panic.
    pub clean: bool,
    /// QPC ticks per second, so the recording's `ts_qpc` values can be read
    /// as time from the sidecar alone.
    pub qpc_freq: u64,
    /// Half-width of the anchor's QPC/UTC sandwich, in µs.
    pub anchor_uncertainty_us: i64,
    /// Largest `|drift|` T3's periodic anchor check saw during the run.
    pub max_anchor_drift_us: i64,
    pub window_ms: u64,
    pub coalesce_ms: u64,
    /// Names of the sinks that were enabled for this run, in fan-out order.
    pub sinks: &'a [&'static str],
}

/// Assemble the sidecar from the counters as they stand. Only the sinks named
/// in `run.sinks` get an entry, so a disabled sink is absent rather than a row
/// of zeroes that reads as "delivered everything".
pub fn build(run: &RunInfo<'_>, s: &StatsSnapshot) -> SessionMeta {
    let mut sinks = BTreeMap::new();
    for name in run.sinks {
        let m = match *name {
            "udp" => SinkMeta {
                errors: s.udp_errors,
                // A datagram nobody was listening for is not a loss of the
                // recording; an oversized envelope and a socket buffer with
                // no room for one are frames the live view never saw.
                dropped: s.udp_oversized.saturating_add(s.udp_would_block),
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
        capture_profile: run.capture_profile.to_string(),
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
        qpc_freq: run.qpc_freq,
        anchor_uncertainty_us: run.anchor_uncertainty_us,
        max_anchor_drift_us: run.max_anchor_drift_us,
        window_ms: run.window_ms,
        coalesce_ms: run.coalesce_ms,
        poll_hz: poll_hz(s.report_interval_us),
        sinks,
    }
}

/// Write the sidecar next to the recording. Returns the path written.
///
/// The document goes to `<id>.meta.json.tmp` first and is renamed over the
/// real name, which is atomic on both NTFS and POSIX: a reader that opens the
/// file while the agent is halfway through a refresh gets the previous
/// document, never a truncated one.
pub fn write(dir: &Path, meta: &SessionMeta) -> Result<PathBuf> {
    let name = meta_file_name(&meta.session_id).with_context(|| {
        format!(
            "session id {:?} is not a valid recording id",
            meta.session_id
        )
    })?;
    let path = dir.join(&name);
    let tmp = dir.join(format!("{name}.tmp"));
    let text = meta
        .to_json_pretty()
        .context("serialize session metadata")?;
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("rename into {}", path.display()));
    }
    Ok(path)
}

/// What a refresh of the sidecar may still change.
#[derive(Debug, Clone, Copy)]
struct State {
    exit: ExitReason,
    clean: bool,
}

/// The live sidecar: everything fixed at startup, plus the handle the three
/// threads use to refresh it.
///
/// Cheap to call from anywhere — the per-write mutex is uncontended (four
/// writes a minute) and a write that fails warns exactly once for the life of
/// the run, so a full disk cannot turn into a log of its own.
pub struct Sidecar {
    dir: PathBuf,
    session_id: String,
    capture_version: &'static str,
    started_utc_us: i64,
    qpc_freq: u64,
    anchor_uncertainty_us: i64,
    window_ms: u64,
    coalesce_ms: u64,
    sinks: Vec<&'static str>,
    stats: Arc<Stats>,
    state: Mutex<State>,
    max_drift_us: AtomicI64,
    warned: AtomicBool,
}

impl Sidecar {
    /// Everything a run knows about itself before it starts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dir: PathBuf,
        session_id: String,
        capture_version: &'static str,
        started_utc_us: i64,
        qpc_freq: u64,
        anchor_uncertainty_us: i64,
        window_ms: u64,
        coalesce_ms: u64,
        sinks: Vec<&'static str>,
        stats: Arc<Stats>,
    ) -> Self {
        Self {
            dir,
            session_id,
            capture_version,
            started_utc_us,
            qpc_freq,
            anchor_uncertainty_us,
            window_ms,
            coalesce_ms,
            sinks,
            stats,
            state: Mutex::new(State {
                exit: ExitReason::Running,
                clean: false,
            }),
            max_drift_us: AtomicI64::new(0),
            warned: AtomicBool::new(false),
        }
    }

    /// Record how far the anchor was seen to drift. Only the largest
    /// magnitude survives, which is the number that says whether absolute
    /// timestamps late in the recording can be trusted.
    pub fn observe_drift(&self, drift_us: i64) {
        let magnitude = drift_us.saturating_abs();
        let mut best = self.max_drift_us.load(Ordering::Relaxed);
        while magnitude > best {
            match self.max_drift_us.compare_exchange_weak(
                best,
                magnitude,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => best = actual,
            }
        }
    }

    /// Settle why the run ended. Called once, before the workers are joined,
    /// so every later refresh — including the one T2 triggers between the
    /// recording's last flush and the Kafka drain — already carries it.
    pub fn set_exit(&self, exit: ExitReason, clean: bool) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.exit = exit;
        state.clean = clean;
    }

    /// The document as it would be written right now.
    pub fn document(&self) -> SessionMeta {
        let state = *self.state.lock().unwrap_or_else(|p| p.into_inner());
        build(
            &RunInfo {
                session_id: &self.session_id,
                capture_version: self.capture_version,
                capture_profile: PROFILE,
                started_utc_us: self.started_utc_us,
                ended_utc_us: telemouse_core::now_utc_us(),
                exit: state.exit,
                clean: state.clean,
                qpc_freq: self.qpc_freq,
                anchor_uncertainty_us: self.anchor_uncertainty_us,
                max_anchor_drift_us: self.max_drift_us.load(Ordering::Relaxed),
                window_ms: self.window_ms,
                coalesce_ms: self.coalesce_ms,
                sinks: &self.sinks,
            },
            &self.stats.snapshot(),
        )
    }

    /// Refresh the file. Never fails the caller: the first failure warns, the
    /// rest are silent, and the run carries on either way.
    pub fn refresh(&self) -> Option<PathBuf> {
        let doc = self.document();
        match write(&self.dir, &doc) {
            Ok(path) => Some(path),
            Err(e) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        dir = %self.dir.display(),
                        error = %format!("{e:#}"),
                        "could not write session metadata; the run continues without a sidecar"
                    );
                }
                None
            }
        }
    }
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
            report_interval_us: 1_000,
            udp_errors: 0,
            udp_unreachable: 400,
            udp_oversized: 0,
            udp_would_block: 7,
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
            capture_profile: "release",
            started_utc_us: 1_756_000_000_000_000,
            ended_utc_us: 1_756_000_010_000_000,
            exit: ExitReason::Interrupt,
            clean: true,
            qpc_freq: 10_000_000,
            anchor_uncertainty_us: 12,
            max_anchor_drift_us: 340,
            window_ms: 50,
            coalesce_ms: 8,
            sinks,
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "telemouse-meta-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn only_enabled_sinks_appear_and_losses_are_summed() {
        let m = build(&run(&["udp", "jsonl", "kafka"]), &snapshot());
        assert_eq!(m.sinks.len(), 3);
        assert_eq!(m.sinks["kafka"].dropped, 397);
        assert_eq!(m.sinks["kafka"].abandoned, 3);
        assert_eq!(m.sinks["kafka"].errors, 1);
        // Unreachable datagrams are expected with no viz running: not a loss.
        // A full socket buffer is, and lands in `dropped`.
        assert_eq!(m.sinks["udp"].lost(), 7);
        assert_eq!(
            m.losses(),
            vec![("kafka".to_string(), 400), ("udp".to_string(), 7)]
        );
        assert_eq!(m.recording_lost(), 0);
        assert_eq!(m.exit, "interrupt");
        assert!(m.clean);
        assert_eq!(m.events, 12_345);
        assert_eq!(m.ring_drops, 3);

        let m = build(&run(&["udp"]), &snapshot());
        assert_eq!(m.sinks.len(), 1);
        assert!(!m.sinks.contains_key("kafka"));
    }

    #[test]
    fn the_run_parameters_reach_the_document() {
        let m = build(&run(&["jsonl"]), &snapshot());
        assert_eq!(m.capture_profile, "release");
        assert_eq!(m.qpc_freq, 10_000_000);
        assert_eq!(m.anchor_uncertainty_us, 12);
        assert_eq!(m.max_anchor_drift_us, 340);
        assert_eq!((m.window_ms, m.coalesce_ms), (50, 8));
        assert_eq!(m.poll_hz, Some(1_000.0));
        assert!(!m.is_unfinished());

        // No drain yet: the polling rate is absent, not zero.
        let m = build(
            &run(&["jsonl"]),
            &StatsSnapshot {
                report_interval_us: 0,
                ..snapshot()
            },
        );
        assert_eq!(m.poll_hz, None);
    }

    #[test]
    fn a_running_sidecar_says_so() {
        let m = build(
            &RunInfo {
                exit: ExitReason::Running,
                clean: false,
                ..run(&["jsonl"])
            },
            &snapshot(),
        );
        assert!(m.is_unfinished());
        assert_eq!(m.exit, "running");
    }

    #[test]
    fn the_sidecar_lands_next_to_the_recording_and_parses_back() {
        let dir = tempdir("write");
        let m = build(&run(&["jsonl"]), &snapshot());
        let path = write(&dir, &m).unwrap();
        assert_eq!(path, dir.join("s-20260908-120000-abcd.meta.json"));
        let back = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, m);
        // The temp file never survives a successful write.
        assert!(!dir.join("s-20260908-120000-abcd.meta.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewriting_replaces_the_document_in_place() {
        let dir = tempdir("rewrite");
        let first = build(
            &RunInfo {
                exit: ExitReason::Running,
                ..run(&["jsonl"])
            },
            &snapshot(),
        );
        let path = write(&dir, &first).unwrap();
        let second = build(&run(&["jsonl"]), &snapshot());
        assert_eq!(write(&dir, &second).unwrap(), path);
        let back = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.exit, "interrupt");
        assert!(!back.is_unfinished());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsafe_session_id_is_refused_rather_than_written_anywhere() {
        let mut m = build(&run(&[]), &snapshot());
        m.session_id = "../escape".into();
        assert!(write(Path::new("."), &m).is_err());
    }

    fn sidecar(dir: PathBuf, stats: Arc<Stats>) -> Sidecar {
        Sidecar::new(
            dir,
            "s-20260908-120000-abcd".into(),
            "0.1.0",
            1_756_000_000_000_000,
            10_000_000,
            12,
            50,
            8,
            vec!["jsonl", "kafka"],
            stats,
        )
    }

    #[test]
    fn the_live_sidecar_starts_unfinished_and_settles_at_exit() {
        let dir = tempdir("live");
        let stats = Arc::new(Stats::default());
        let s = sidecar(dir.clone(), Arc::clone(&stats));

        let path = s.refresh().expect("the first write succeeds");
        let doc = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc.is_unfinished());
        assert!(!doc.clean);
        assert_eq!(doc.events, 0);

        // Counters move; a refresh picks them up without being told.
        stats.batches.store(12, Ordering::Relaxed);
        s.refresh().unwrap();
        let doc = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc.batches, 12);
        assert!(doc.is_unfinished());

        s.set_exit(ExitReason::Duration, true);
        s.refresh().unwrap();
        let doc = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc.exit, "duration");
        assert!(doc.clean && !doc.is_unfinished());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_largest_drift_magnitude_is_kept() {
        let s = sidecar(tempdir("drift"), Arc::new(Stats::default()));
        s.observe_drift(120);
        s.observe_drift(-4_000);
        s.observe_drift(30);
        assert_eq!(s.document().max_anchor_drift_us, 4_000);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn an_unwritable_directory_is_a_warning_not_a_failure() {
        // A directory that does not exist stands in for one we cannot write.
        let dir = std::env::temp_dir().join("telemouse-meta-nope-does-not-exist");
        let _ = std::fs::remove_dir_all(&dir);
        let s = sidecar(dir, Arc::new(Stats::default()));
        assert_eq!(s.refresh(), None);
        assert_eq!(s.refresh(), None, "and it does not warn twice");
    }
}
