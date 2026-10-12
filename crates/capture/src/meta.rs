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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use telemouse_core::recordings::{ExitReason, SessionMeta, SinkMeta, meta_file_name};

use crate::PROFILE;
use crate::sinks::SinkKind;
use crate::stats::{Stats, StatsSnapshot, Throttle, poll_hz};

/// A sidecar that cannot be written warns at most this often (CONVENTIONS:
/// anything that stays wrong is repeated once a minute, never only once).
const WRITE_WARN_INTERVAL: Duration = Duration::from_secs(60);

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
    /// The sinks that were enabled for this run, in fan-out order.
    pub sinks: &'a [SinkKind],
}

/// Assemble the sidecar from the counters as they stand. Only the sinks named
/// in `run.sinks` get an entry, so a disabled sink is absent rather than a row
/// of zeroes that reads as "delivered everything".
pub fn build(run: &RunInfo<'_>, s: &StatsSnapshot) -> SessionMeta {
    let mut sinks = BTreeMap::new();
    for &kind in run.sinks {
        let m = match kind {
            SinkKind::Udp => SinkMeta {
                errors: s.udp_errors,
                // A datagram nobody was listening for is not a loss of the
                // recording; an oversized envelope and a socket buffer with
                // no room for one are frames the live view never saw.
                dropped: s.udp_oversized.saturating_add(s.udp_would_block),
                abandoned: 0,
            },
            SinkKind::Jsonl => SinkMeta {
                errors: s.jsonl_errors,
                dropped: s.jsonl_dropped,
                abandoned: s.jsonl_abandoned,
            },
            SinkKind::Kafka => SinkMeta {
                errors: s.kafka_errors,
                dropped: s.kafka_dropped,
                abandoned: s.kafka_abandoned,
            },
        };
        sinks.insert(kind.as_str().to_string(), m);
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

/// Everything a run knows about itself before it starts: the part of the
/// sidecar that never changes.
#[derive(Debug, Clone)]
pub struct SidecarRun {
    /// Where the recording (and so the sidecar) lives.
    pub dir: PathBuf,
    pub session_id: String,
    pub capture_version: &'static str,
    pub started_utc_us: i64,
    pub qpc_freq: u64,
    pub anchor_uncertainty_us: i64,
    pub window_ms: u64,
    pub coalesce_ms: u64,
    /// The sinks that actually started, in fan-out order.
    pub sinks: Vec<SinkKind>,
}

/// The live sidecar: everything fixed at startup, plus the handle the three
/// threads use to refresh it.
///
/// Cheap to call from anywhere. Writes are serialized by one mutex (T3's
/// periodic refresh and T2's at-close refresh can otherwise race on the same
/// temp file at shutdown); it is uncontended in practice, a dozen writes a
/// minute. A write that fails warns at most once a minute, so a full disk
/// cannot turn into a log of its own.
pub struct Sidecar {
    run: SidecarRun,
    stats: Arc<Stats>,
    state: Mutex<State>,
    max_drift_us: AtomicI64,
    /// Held across every write; also rate-limits the failure warning.
    write: Mutex<Throttle>,
}

impl Sidecar {
    pub fn new(run: SidecarRun, stats: Arc<Stats>) -> Self {
        Self {
            run,
            stats,
            state: Mutex::new(State {
                exit: ExitReason::Running,
                clean: false,
            }),
            max_drift_us: AtomicI64::new(0),
            write: Mutex::new(Throttle::new(WRITE_WARN_INTERVAL)),
        }
    }

    /// Record how far the anchor was seen to drift. Only the largest
    /// magnitude survives, which is the number that says whether absolute
    /// timestamps late in the recording can be trusted.
    pub fn observe_drift(&self, drift_us: i64) {
        self.max_drift_us
            .fetch_max(drift_us.saturating_abs(), Ordering::Relaxed);
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
        let run = &self.run;
        build(
            &RunInfo {
                session_id: &run.session_id,
                capture_version: run.capture_version,
                capture_profile: PROFILE,
                started_utc_us: run.started_utc_us,
                ended_utc_us: telemouse_core::now_utc_us(),
                exit: state.exit,
                clean: state.clean,
                qpc_freq: run.qpc_freq,
                anchor_uncertainty_us: run.anchor_uncertainty_us,
                max_anchor_drift_us: self.max_drift_us.load(Ordering::Relaxed),
                window_ms: run.window_ms,
                coalesce_ms: run.coalesce_ms,
                sinks: &run.sinks,
            },
            &self.stats.snapshot(),
        )
    }

    /// Refresh the file. Never fails the caller: a failure warns (at most
    /// once a minute) and the run carries on either way.
    pub fn refresh(&self) -> Option<PathBuf> {
        // One writer at a time: two refreshes share `<id>.meta.json.tmp`, and
        // the loser of an unserialized race would fail its rename.
        let mut warn = self.write.lock().unwrap_or_else(|p| p.into_inner());
        let doc = self.document();
        match write(&self.run.dir, &doc) {
            Ok(path) => Some(path),
            Err(e) => {
                if let Some(suppressed) = warn.allow(Instant::now()) {
                    tracing::warn!(
                        dir = %self.run.dir.display(),
                        error = %format!("{e:#}"),
                        suppressed,
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

    fn run<'a>(sinks: &'a [SinkKind]) -> RunInfo<'a> {
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
        let m = build(
            &run(&[SinkKind::Udp, SinkKind::Jsonl, SinkKind::Kafka]),
            &snapshot(),
        );
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

        let m = build(&run(&[SinkKind::Udp]), &snapshot());
        assert_eq!(m.sinks.len(), 1);
        assert!(!m.sinks.contains_key("kafka"));
    }

    #[test]
    fn the_run_parameters_reach_the_document() {
        let m = build(&run(&[SinkKind::Jsonl]), &snapshot());
        assert_eq!(m.capture_profile, "release");
        assert_eq!(m.qpc_freq, 10_000_000);
        assert_eq!(m.anchor_uncertainty_us, 12);
        assert_eq!(m.max_anchor_drift_us, 340);
        assert_eq!((m.window_ms, m.coalesce_ms), (50, 8));
        assert_eq!(m.poll_hz, Some(1_000.0));
        assert!(!m.is_unfinished());

        // No drain yet: the polling rate is absent, not zero.
        let m = build(
            &run(&[SinkKind::Jsonl]),
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
                ..run(&[SinkKind::Jsonl])
            },
            &snapshot(),
        );
        assert!(m.is_unfinished());
        assert_eq!(m.exit, "running");
    }

    #[test]
    fn the_sidecar_lands_next_to_the_recording_and_parses_back() {
        let dir = tempdir("write");
        let m = build(&run(&[SinkKind::Jsonl]), &snapshot());
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
                ..run(&[SinkKind::Jsonl])
            },
            &snapshot(),
        );
        let path = write(&dir, &first).unwrap();
        let second = build(&run(&[SinkKind::Jsonl]), &snapshot());
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
            SidecarRun {
                dir,
                session_id: "s-20260908-120000-abcd".into(),
                capture_version: "0.1.0",
                started_utc_us: 1_756_000_000_000_000,
                qpc_freq: 10_000_000,
                anchor_uncertainty_us: 12,
                window_ms: 50,
                coalesce_ms: 8,
                sinks: vec![SinkKind::Jsonl, SinkKind::Kafka],
            },
            stats,
        )
    }

    /// T3's periodic refresh and T2's at-close refresh can land together at
    /// shutdown; serialized, neither loses its rename to the other.
    #[test]
    fn concurrent_refreshes_never_collide_on_the_temp_file() {
        let dir = tempdir("race");
        let s = Arc::new(sidecar(dir.clone(), Arc::new(Stats::default())));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || (0..25).all(|_| s.refresh().is_some()))
            })
            .collect();
        for t in threads {
            assert!(t.join().unwrap(), "a refresh lost a race");
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&s.run.dir);
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
