//! T3 — the context thread (250ms tick) and everything else that runs on a
//! slow timer: the 5s stats report, the periodic anchor-drift check, and the
//! config file watch.
//!
//! Everything here goes through [`crate::platform`], so the loop itself is
//! platform-neutral; the pointer-lock decision is delegated to the pure
//! [`PointerLockDetector`]. The loop waits on a condvar rather than sleeping in
//! slices, so an idle agent wakes 4×/s and not 40×/s.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use telemouse_core::QpcAnchor;
use telemouse_core::config::AppConfig;

use crate::context::{ContextSnapshot, SharedContext};
use crate::platform::{self, ForegroundCache};
use crate::pointer_lock::PointerLockDetector;
use crate::raw_input::RingWaker;
use crate::session_setup::{anchor_drift_us, drift_ppm, measure_anchor};
use crate::shipping::MarkerSignal;
use crate::shutdown::Shutdown;
use crate::stats::Stats;
#[cfg(feature = "observability")]
use crate::stats::{
    ConditionLimiter, ESCALATION_INTERVAL, Escalation, IDLE_WARN_SECS, StatsSnapshot,
    compute_report, idle_for_secs,
};

pub const TICK: Duration = Duration::from_millis(250);
#[cfg_attr(not(feature = "observability"), allow(dead_code))]
pub const REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// How often the session anchor is re-measured against the wall clock.
pub const ANCHOR_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// How often `telemouse.toml` is stat'ed for a mid-session edit.
pub const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Drift past this is worth recording in the stream, not just the log.
pub const ANCHOR_DRIFT_MARKER_US: i64 = 200;

/// What T3 needs beyond the shared context and counters.
pub struct ContextArgs {
    pub session_id: String,
    /// The session anchor, for drift measurement.
    pub anchor: QpcAnchor,
    pub config_path: PathBuf,
    /// The config as loaded at startup, to diff reloads against.
    pub config: AppConfig,
    /// Markers go out through T2 (which owns the sinks), same as the hotkey's.
    pub marker_tx: Sender<MarkerSignal>,
    /// T2 parks indefinitely on an idle desk; a marker has to wake it.
    pub waker: Arc<RingWaker>,
    /// The live `<session>.meta.json`, when the run is recording. T3 owns its
    /// refresh cadence: one on the first tick, then one per report.
    #[cfg(feature = "observability")]
    pub sidecar: Option<Arc<crate::meta::Sidecar>>,
}

/// The 5s stats line, the escalation warnings and the sidecar refresh —
/// everything `observability` pays for, in one place so the loop below reads
/// the same with the feature off.
#[cfg(feature = "observability")]
struct Reporter {
    session_id: String,
    qpc_freq: u64,
    last_at: Instant,
    last: StatsSnapshot,
    escalation: ConditionLimiter,
    sidecar: Option<Arc<crate::meta::Sidecar>>,
}

#[cfg(feature = "observability")]
impl Reporter {
    fn new(session_id: String, qpc_freq: u64, sidecar: Option<Arc<crate::meta::Sidecar>>) -> Self {
        Self {
            session_id,
            qpc_freq,
            last_at: Instant::now(),
            last: StatsSnapshot::default(),
            escalation: ConditionLimiter::new(ESCALATION_INTERVAL),
            sidecar,
        }
    }

    /// Put a sidecar on disk before anything has happened, so a run that is
    /// killed in its first minute still leaves a document saying which
    /// session the recording belongs to and that it never finished.
    fn start(&self) {
        if let Some(s) = &self.sidecar
            && let Some(path) = s.refresh()
        {
            tracing::debug!(path = %path.display(), "session metadata opened");
        }
    }

    fn observe_drift(&self, drift_us: i64) {
        if let Some(s) = &self.sidecar {
            s.observe_drift(drift_us);
        }
    }

    fn maybe_report(&mut self, stats: &Stats, game: Option<&str>, locked: bool) {
        if self.last_at.elapsed() < REPORT_INTERVAL {
            return;
        }
        let now = stats.snapshot();
        let idle = idle_for_secs(stats.last_event_qpc(), platform::qpc(), self.qpc_freq);
        let r = compute_report(&self.last, &now, self.last_at.elapsed().as_secs_f64(), idle);
        tracing::info!(
            session = %self.session_id,
            events_per_s = r.events_per_s,
            events = r.events,
            batches_per_s = r.batches_per_s,
            batches = r.batches,
            drops = r.drops,
            drops_delta = r.drops_delta,
            abs_frames = r.abs_frames,
            markers = r.markers,
            raw_read_errors = r.raw_read_errors,
            poll_hz = r.poll_hz,
            drains_per_s = r.drains_per_s,
            reports_per_drain = r.reports_per_drain,
            ring_high_water = r.ring_high_water,
            capture_to_ship_us_p50 = %r.capture_to_ship_us_p50,
            capture_to_ship_us_p99 = %r.capture_to_ship_us_p99,
            ship_tail_us_p99 = %r.ship_tail_us_p99,
            idle_for_s = r.idle_for_s,
            udp_errors = r.udp_errors,
            jsonl_errors = r.jsonl_errors,
            kafka_errors = r.kafka_errors,
            udp_unreachable = r.udp_unreachable,
            udp_oversized = r.udp_oversized,
            udp_would_block = r.udp_would_block,
            jsonl_queued = r.jsonl_queued,
            jsonl_dropped = r.jsonl_dropped,
            jsonl_abandoned = r.jsonl_abandoned,
            jsonl_dropped_delta = r.jsonl_dropped_delta,
            jsonl_abandoned_delta = r.jsonl_abandoned_delta,
            kafka_queued = r.kafka_queued,
            kafka_dropped = r.kafka_dropped,
            kafka_abandoned = r.kafka_abandoned,
            kafka_dropped_delta = r.kafka_dropped_delta,
            kafka_abandoned_delta = r.kafka_abandoned_delta,
            jsonl_flush_max_us = r.jsonl_flush_max_us,
            game = game.unwrap_or("-"),
            pointer_locked = locked,
            "capture stats"
        );
        self.escalate(&r);
        if let Some(s) = &self.sidecar {
            s.refresh();
        }
        self.last = now;
        self.last_at = Instant::now();
    }

    /// The report line is a wall of numbers nobody reads while things are
    /// fine. These are the few facts worth interrupting for — each one at
    /// most once a minute for as long as it holds, and again from scratch if
    /// it clears and comes back.
    fn escalate(&mut self, r: &crate::stats::ReportFields) {
        let at = Instant::now();
        for sink in ["jsonl", "kafka"] {
            if self.escalation.observe(sink, r.sink_is_losing(sink), at) == Escalation::Warn {
                tracing::warn!(
                    sink,
                    dropped_delta = if sink == "jsonl" {
                        r.jsonl_dropped_delta
                    } else {
                        r.kafka_dropped_delta
                    },
                    abandoned_delta = if sink == "jsonl" {
                        r.jsonl_abandoned_delta
                    } else {
                        r.kafka_abandoned_delta
                    },
                    "sink is losing envelopes"
                );
            }
        }
        if self.escalation.observe("ring", r.drops_delta > 0, at) == Escalation::Warn {
            tracing::warn!(
                drops_delta = r.drops_delta,
                drops = r.drops,
                ring_high_water = r.ring_high_water,
                "the ring overflowed; the shipping thread is not keeping up"
            );
        }
        match self
            .escalation
            .observe("idle", r.idle_for_s >= IDLE_WARN_SECS, at)
        {
            Escalation::Warn => tracing::warn!(
                idle_for_s = r.idle_for_s,
                "no raw input for a while; the desk is idle, or capture has stopped delivering"
            ),
            Escalation::Recovered => tracing::info!("input resumed"),
            _ => {}
        }
    }
}

/// Sample the environment once. Pure-ish: everything platform-specific is a
/// call into [`crate::platform`]. `screen` is passed in rather than read
/// here: the primary screen only changes on `WM_DISPLAYCHANGE`, which T1
/// flags, so the loop re-reads it then instead of four times a second.
pub fn sample(
    detector: &mut PointerLockDetector,
    foreground: &mut ForegroundCache,
    events_delta: u64,
    screen: (u32, u32),
) -> ContextSnapshot {
    let (screen_w, screen_h) = screen;
    let cursor = platform::cursor_pos();
    let pointer_locked = match cursor {
        Some(pos) => detector.observe(pos, events_delta),
        // No cursor to compare: keep the previous verdict rather than flapping.
        None => detector.locked(),
    };
    let (cursor_x, cursor_y) = cursor.unwrap_or((0, 0));
    ContextSnapshot {
        game: foreground.get(),
        pointer_locked,
        screen_w,
        screen_h,
        cursor_x,
        cursor_y,
    }
}

/// Human-readable list of what changed between two configs. Empty means the
/// file was touched but says the same thing.
pub fn describe_config_change(old: &AppConfig, new: &AppConfig) -> Vec<String> {
    let scalars = [
        (
            "mouse_cpi",
            old.mouse_cpi.to_string(),
            new.mouse_cpi.to_string(),
        ),
        (
            "marker_hotkey",
            old.marker_hotkey.clone(),
            new.marker_hotkey.clone(),
        ),
        (
            "batch.window_ms",
            old.batch.window_ms.to_string(),
            new.batch.window_ms.to_string(),
        ),
        (
            "batch.max_events",
            old.batch.max_events.to_string(),
            new.batch.max_events.to_string(),
        ),
        (
            "batch.ring_capacity",
            old.batch.ring_capacity.to_string(),
            new.batch.ring_capacity.to_string(),
        ),
        (
            "udp.enabled",
            old.udp.enabled.to_string(),
            new.udp.enabled.to_string(),
        ),
        ("udp.addr", old.udp.addr.clone(), new.udp.addr.clone()),
        (
            "kafka.enabled",
            old.kafka.enabled.to_string(),
            new.kafka.enabled.to_string(),
        ),
        (
            "kafka.brokers",
            old.kafka.brokers.join(","),
            new.kafka.brokers.join(","),
        ),
        (
            "recording.enabled",
            old.recording.enabled.to_string(),
            new.recording.enabled.to_string(),
        ),
        (
            "recording.dir",
            old.recording.dir.display().to_string(),
            new.recording.dir.display().to_string(),
        ),
        (
            "viz.http_addr",
            old.viz.http_addr.clone(),
            new.viz.http_addr.clone(),
        ),
    ];
    let mut out: Vec<String> = scalars
        .into_iter()
        .filter(|(_, a, b)| a != b)
        .map(|(field, a, b)| format!("{field}: {a} -> {b}"))
        .collect();
    for (game, new_sens) in &new.games {
        match old.games.get(game) {
            None => out.push(format!("games.{game}: added")),
            Some(old_sens) if old_sens != new_sens => {
                out.push(format!("games.{game}: {old_sens:?} -> {new_sens:?}"))
            }
            Some(_) => {}
        }
    }
    for game in old.games.keys() {
        if !new.games.contains_key(game) {
            out.push(format!("games.{game}: removed"));
        }
    }
    out
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

pub fn run(ctx: Arc<SharedContext>, stats: Arc<Stats>, shutdown: Arc<Shutdown>, args: ContextArgs) {
    let ContextArgs {
        session_id,
        anchor,
        config_path,
        mut config,
        marker_tx,
        waker,
        #[cfg(feature = "observability")]
        sidecar,
    } = args;
    let _ = &session_id;

    let mut detector = PointerLockDetector::default();
    let mut foreground = ForegroundCache::new();
    let mut last_events = stats.events();
    #[cfg(feature = "observability")]
    let mut reporter = Reporter::new(session_id, anchor.qpc_freq, sidecar);
    #[cfg(feature = "observability")]
    reporter.start();
    let mut last_game: Option<String> = None;
    let mut last_locked = false;
    let mut last_anchor_check = Instant::now();
    let mut config_mtime = mtime(&config_path);
    let mut last_config_check = Instant::now();
    let mut screen = platform::primary_screen();

    while !shutdown.is_set() {
        stats
            .t3_ticks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // T1 saw WM_DISPLAYCHANGE: the desktop geometry changed, so this
        // tick's snapshot carries fresh metrics.
        let display_changed = ctx.take_display_changed();
        if display_changed {
            screen = platform::primary_screen();
        }
        let events = stats.events();
        let snapshot = sample(
            &mut detector,
            &mut foreground,
            events.saturating_sub(last_events),
            screen,
        );
        last_events = events;

        if snapshot.game != last_game {
            tracing::info!(
                game = snapshot.game.as_deref().unwrap_or("-"),
                "foreground changed"
            );
            last_game = snapshot.game.clone();
        }
        if snapshot.pointer_locked != last_locked {
            tracing::info!(
                pointer_locked = snapshot.pointer_locked,
                game = snapshot.game.as_deref().unwrap_or("-"),
                "pointer lock transition"
            );
            last_locked = snapshot.pointer_locked;
        }
        let locked = snapshot.pointer_locked;
        let game = snapshot.game.clone();
        let (screen_w, screen_h) = (snapshot.screen_w, snapshot.screen_h);
        ctx.set(snapshot);

        // The snapshot above already carries the new metrics; this is about
        // telling the operator (and refreshing the full monitor list, which
        // the per-tick sample does not read).
        if display_changed {
            let monitors = platform::monitors();
            tracing::info!(
                screen_w,
                screen_h,
                monitors = monitors.len(),
                "display configuration changed"
            );
        }

        if last_anchor_check.elapsed() >= ANCHOR_CHECK_INTERVAL {
            let drift_us = check_anchor_drift(&anchor, &marker_tx);
            #[cfg(feature = "observability")]
            reporter.observe_drift(drift_us);
            #[cfg(not(feature = "observability"))]
            let _ = drift_us;
            waker.wake();
            last_anchor_check = Instant::now();
        }

        // A file stat four times a second is the most expensive thing an
        // idle agent does; once every couple of seconds is plenty for a
        // config edit to be noticed.
        if last_config_check.elapsed() >= CONFIG_CHECK_INTERVAL {
            last_config_check = Instant::now();
            let current_mtime = mtime(&config_path);
            if current_mtime != config_mtime {
                config_mtime = current_mtime;
                reload_config(&config_path, &mut config, &marker_tx);
                waker.wake();
            }
        }

        #[cfg(feature = "observability")]
        reporter.maybe_report(&stats, game.as_deref(), locked);
        #[cfg(not(feature = "observability"))]
        let _ = (&game, locked);

        shutdown.wait_timeout(TICK);
    }
    tracing::debug!("context thread finished");
}

/// Re-measure the wall clock against the session anchor, returning the drift
/// in µs so the caller can keep the run's worst.
///
/// Drift is *always* logged. A refreshed `session` envelope is deliberately not
/// emitted here: consumers take the **first** session envelope of a stream, so
/// a second one would either be ignored or silently change the meaning of an
/// in-flight recording. Significant drift is recorded as a `marker` instead,
/// which is exactly the schema-stable side channel markers exist for.
fn check_anchor_drift(anchor: &QpcAnchor, marker_tx: &Sender<MarkerSignal>) -> i64 {
    let (fresh, uncertainty_us) = measure_anchor(anchor.qpc_freq);
    let drift = anchor_drift_us(anchor, &fresh);
    let elapsed_us = anchor.ticks_to_us(anchor.qpc, fresh.qpc);
    let ppm = drift_ppm(drift, elapsed_us);
    tracing::info!(
        drift_us = drift,
        ppm,
        elapsed_s = elapsed_us / 1_000_000,
        uncertainty_us,
        "anchor drift"
    );
    if drift.abs() > ANCHOR_DRIFT_MARKER_US {
        let _ = marker_tx.send(MarkerSignal {
            ts_qpc: fresh.qpc,
            label: format!("anchor_drift_us={drift}"),
        });
    }
    drift
}

/// Reload `telemouse.toml` after an mtime change.
///
/// Nothing structural is re-applied mid-session — the ring, the batch window
/// and the sinks are fixed for the session's lifetime, and quietly changing
/// them would make the recording self-inconsistent. What the reload buys is a
/// log of what changed plus a `config_changed` marker in the stream, so the
/// discrepancy between the file and the running session is visible later.
fn reload_config(path: &Path, current: &mut AppConfig, marker_tx: &Sender<MarkerSignal>) {
    match AppConfig::load(path) {
        Ok(next) => {
            let changes = describe_config_change(current, &next);
            if changes.is_empty() {
                tracing::debug!(path = %path.display(), "config touched, no changes");
                return;
            }
            tracing::info!(
                path = %path.display(),
                changes = %changes.join("; "),
                "config changed; the running session keeps its startup settings"
            );
            *current = next;
            let _ = marker_tx.send(MarkerSignal {
                ts_qpc: platform::qpc(),
                label: "config_changed".to_string(),
            });
        }
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "config reload failed; keeping the previous config"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use super::*;

    fn args(marker_tx: Sender<MarkerSignal>) -> ContextArgs {
        ContextArgs {
            session_id: "s-test".into(),
            anchor: QpcAnchor {
                qpc: platform::qpc(),
                utc_us: 1_756_000_000_000_000,
                qpc_freq: platform::qpc_freq(),
            },
            config_path: PathBuf::from("definitely-not-a-config-file.toml"),
            config: AppConfig::default(),
            marker_tx,
            waker: Arc::new(RingWaker::default()),
            #[cfg(feature = "observability")]
            sidecar: None,
        }
    }

    #[test]
    fn sampling_fills_the_snapshot_without_panicking() {
        let mut d = PointerLockDetector::default();
        let mut f = ForegroundCache::new();
        let s = sample(&mut d, &mut f, 0, (2560, 1440));
        // Off Windows the platform layer returns zeros; either way it must not
        // report a lock from a single idle sample.
        assert!(!s.pointer_locked);
        assert_eq!(s.batch_cursor(), (Some(s.cursor_x), Some(s.cursor_y)));
        // The screen is whatever the loop handed in, not re-read per tick.
        assert_eq!((s.screen_w, s.screen_h), (2560, 1440));
    }

    #[test]
    fn the_thread_stops_promptly_when_shutdown_is_set() {
        let ctx = Arc::new(SharedContext::default());
        let stats = Arc::new(Stats::default());
        let shutdown = Arc::new(Shutdown::new());
        let (tx, _rx) = channel();
        let handle = {
            let (c, s, sd) = (ctx.clone(), stats.clone(), shutdown.clone());
            let a = args(tx);
            std::thread::spawn(move || run(c, s, sd, a))
        };
        std::thread::sleep(Duration::from_millis(60));
        shutdown.set();
        let start = Instant::now();
        handle.join().unwrap();
        // The condvar wakes it immediately rather than after the tick.
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn an_unchanged_config_reports_no_changes() {
        let a = AppConfig::default();
        assert!(describe_config_change(&a, &a.clone()).is_empty());
    }

    #[test]
    fn config_changes_are_described_field_by_field() {
        let old = AppConfig::default();
        let mut new = old.clone();
        new.mouse_cpi = 3200.0;
        new.udp.addr = "127.0.0.1:9999".into();
        new.kafka.enabled = true;
        let changes = describe_config_change(&old, &new);
        assert_eq!(changes.len(), 3, "{changes:?}");
        assert!(changes.iter().any(|c| c.starts_with("mouse_cpi: 1600")));
        assert!(changes.iter().any(|c| c.contains("udp.addr")));
        assert!(changes.iter().any(|c| c == "kafka.enabled: false -> true"));
    }

    #[test]
    fn added_removed_and_edited_games_are_all_reported() {
        use telemouse_core::session::GameSens;
        let sens = |s: f64| GameSens {
            sens: s,
            yaw_coeff: 0.022,
            pitch_coeff: 0.022,
        };
        let mut old = AppConfig::default();
        old.games.insert("cs2.exe".into(), sens(1.0));
        old.games.insert("gone.exe".into(), sens(2.0));
        let mut new = AppConfig::default();
        new.games.insert("cs2.exe".into(), sens(1.5));
        new.games.insert("cod.exe".into(), sens(12.5));

        let changes = describe_config_change(&old, &new);
        assert!(changes.iter().any(|c| c.starts_with("games.cs2.exe: ")));
        assert!(changes.contains(&"games.cod.exe: added".to_string()));
        assert!(changes.contains(&"games.gone.exe: removed".to_string()));
        assert_eq!(changes.len(), 3, "{changes:?}");
    }

    #[test]
    fn a_missing_config_file_has_no_mtime() {
        assert!(mtime(Path::new("definitely-not-a-config-file.toml")).is_none());
    }

    #[test]
    fn a_broken_config_reload_keeps_the_previous_one() {
        let dir = std::env::temp_dir().join(format!("telemouse-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("telemouse.toml");
        std::fs::write(&path, "mouse_dpi = 3200.0\n").unwrap(); // typo: rejected
        let (tx, rx) = channel();
        let mut current = AppConfig::default();
        reload_config(&path, &mut current, &tx);
        assert_eq!(current, AppConfig::default());
        assert!(rx.try_recv().is_err(), "no marker for a rejected config");

        // A valid change does produce a marker.
        std::fs::write(&path, "mouse_cpi = 3200.0\n").unwrap();
        reload_config(&path, &mut current, &tx);
        assert_eq!(current.mouse_cpi, 3200.0);
        assert_eq!(rx.try_recv().unwrap().label, "config_changed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_healthy_clock_does_not_produce_a_drift_marker() {
        let anchor = QpcAnchor {
            qpc: platform::qpc(),
            utc_us: crate::session_setup::now_utc_us(),
            qpc_freq: platform::qpc_freq(),
        };
        let (tx, rx) = channel();
        let drift = check_anchor_drift(&anchor, &tx);
        assert!(
            rx.try_recv().is_err(),
            "an anchor taken microseconds ago cannot have drifted 200µs"
        );
        assert!(drift.abs() <= ANCHOR_DRIFT_MARKER_US, "drift {drift}µs");
    }

    /// The reporter is what owns the sidecar's cadence, so the "there is a
    /// document on disk before anything happens" promise is its test.
    #[cfg(feature = "observability")]
    #[test]
    fn the_first_tick_puts_an_unfinished_sidecar_on_disk() {
        use telemouse_core::recordings::SessionMeta;

        let dir = std::env::temp_dir().join(format!("telemouse-t3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let stats = Arc::new(Stats::default());
        let sidecar = Arc::new(crate::meta::Sidecar::new(
            dir.clone(),
            "s-20260908-120000-abcd".into(),
            "0.1.0",
            1_756_000_000_000_000,
            10_000_000,
            3,
            25,
            8,
            vec!["jsonl"],
            Arc::clone(&stats),
        ));
        let reporter = Reporter::new("s-test".into(), 10_000_000, Some(sidecar));
        reporter.start();

        let path = dir.join("s-20260908-120000-abcd.meta.json");
        let doc = SessionMeta::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc.is_unfinished());
        assert!(!doc.clean);
        assert_eq!(doc.window_ms, 25);
        assert!(doc.sinks.contains_key("jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And with no recording there is nothing to write next to — which must
    /// be a no-op, not a path built out of an empty directory.
    #[cfg(feature = "observability")]
    #[test]
    fn a_run_without_a_recording_has_no_sidecar_to_refresh() {
        let mut reporter = Reporter::new("s-test".into(), 10_000_000, None);
        reporter.start();
        reporter.observe_drift(5_000);
        reporter.maybe_report(&Stats::default(), None, false);
    }
}
