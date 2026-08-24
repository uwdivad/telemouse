//! Synthetic-recording builders.
//!
//! Public (not `#[cfg(test)]`) so the integration test, the benches — and
//! anyone who wants to eyeball the report without a mouse — can build a
//! recording with known ground truth. Every metric test in this crate is
//! written against streams constructed here, so the fixture constants are
//! effectively part of the test contract: CPI 1600 (1600 counts = 2.54 cm) and
//! `cs2.exe` at sens 2.0 with 0.022 coefficients (1 count = 0.044°, so
//! 1000 counts = 44°).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use telemouse_core::{
    Batch, Envelope, GameSens, Marker, MonitorInfo, QpcAnchor, RawEvent, SessionConfig,
};

use crate::load::{BatchMeta, LoadedSession};
use crate::series::{Params, Prepared, prepare};

/// Windows QPC frequency on effectively every modern machine.
pub const FIXTURE_FREQ: u64 = 10_000_000;
/// Arbitrary anchor QPC (~500s of uptime).
pub const FIXTURE_QPC0: u64 = 5_000_000_000;
/// 2025-08-24 01:46:40 UTC.
pub const FIXTURE_UTC0: i64 = 1_756_000_000_000_000;
pub const FIXTURE_CPI: f64 = 1600.0;
/// Degrees per count in the fixture: `sens 2.0 * 0.022`.
pub const FIXTURE_DEG_PER_COUNT: f64 = 0.044;

/// The fixture session config: `cs2.exe` has an aim profile, nothing else does.
pub fn session_cfg() -> SessionConfig {
    let mut games = BTreeMap::new();
    games.insert(
        "cs2.exe".to_string(),
        GameSens {
            sens: 2.0,
            yaw_coeff: 0.022,
            pitch_coeff: 0.022,
        },
    );
    SessionConfig {
        session_id: "s-test".into(),
        started_utc_us: FIXTURE_UTC0,
        qpc_freq: FIXTURE_FREQ,
        anchor: QpcAnchor {
            qpc: FIXTURE_QPC0,
            utc_us: FIXTURE_UTC0,
            qpc_freq: FIXTURE_FREQ,
        },
        anchor_uncertainty_us: Some(8),
        mouse_cpi: FIXTURE_CPI,
        devices: vec![r"\\?\HID#VID_1532&PID_0099".into()],
        games,
        monitors: vec![MonitorInfo {
            width: 2560,
            height: 1440,
            refresh_hz: Some(240),
            primary: true,
        }],
        capture_version: "test".into(),
    }
}

/// Wrap `events` in a batch envelope carrying `drops` ring drops.
pub fn batch_env(
    cfg: &SessionConfig,
    seq_no: u64,
    game: Option<&str>,
    drops: u32,
    events: Vec<RawEvent>,
) -> Envelope {
    batch_env_full(cfg, seq_no, game, drops, true, events)
}

/// [`batch_env`] with the pointer-lock flag spelled out.
pub fn batch_env_full(
    cfg: &SessionConfig,
    seq_no: u64,
    game: Option<&str>,
    drops: u32,
    pointer_locked: bool,
    events: Vec<RawEvent>,
) -> Envelope {
    let ts_anchor_us = events
        .first()
        .map(|e| cfg.anchor.qpc_to_utc_us(e.ts_qpc))
        .unwrap_or(cfg.started_utc_us);
    Envelope::Batch(Batch {
        session_id: cfg.session_id.clone(),
        seq_no,
        ts_anchor_us,
        game: game.map(str::to_string),
        pointer_locked,
        screen_w: 2560,
        screen_h: 1440,
        cursor_x: None,
        cursor_y: None,
        drops_since_last: drops,
        abs_frames_since_last: 0,
        events,
    })
}

/// Write `lines` as a JSONL file and return its path.
pub fn write_lines(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::new();
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write fixture");
    path
}

/// Write a complete recording: session header, then `events` chunked into
/// batches of `batch_size`, with `markers` appended at the end.
pub fn write_session(
    dir: &Path,
    cfg: &SessionConfig,
    game: Option<&str>,
    events: &[RawEvent],
    markers: &[Marker],
    batch_size: usize,
) -> PathBuf {
    let mut lines = vec![Envelope::Session(cfg.clone()).to_json().unwrap()];
    for (i, chunk) in events.chunks(batch_size.max(1)).enumerate() {
        lines.push(
            batch_env(cfg, i as u64, game, 0, chunk.to_vec())
                .to_json()
                .unwrap(),
        );
    }
    for m in markers {
        lines.push(Envelope::Marker(m.clone()).to_json().unwrap());
    }
    write_lines(dir, &format!("{}.jsonl", cfg.session_id), &lines)
}

/// One batch of metadata covering `count` events.
pub fn batch_meta(seq_no: u64, game: Option<&str>, locked: bool, count: usize) -> BatchMeta {
    BatchMeta {
        seq_no,
        ts_anchor_us: FIXTURE_UTC0,
        game: game.map(Arc::from),
        pointer_locked: locked,
        drops_since_last: 0,
        abs_frames_since_last: 0,
        first_event_qpc: None,
        event_count: count,
    }
}

/// Wrap raw events in a [`LoadedSession`] without touching the filesystem.
pub fn loaded_from(events: Vec<RawEvent>, game: Option<&str>) -> LoadedSession {
    let cfg = session_cfg();
    LoadedSession {
        path: PathBuf::from("<memory>"),
        config: cfg,
        batches: vec![batch_meta(0, game, true, events.len())],
        events,
        markers: Vec::new(),
        total_drops: 0,
        total_abs_frames: 0,
        bad_lines: 0,
    }
}

/// A [`LoadedSession`] whose events are split across explicit batches — for
/// the pointer-lock, seq-gap and batch-latency checks.
pub fn loaded_with_batches(events: Vec<RawEvent>, batches: Vec<BatchMeta>) -> LoadedSession {
    let cfg = session_cfg();
    LoadedSession {
        path: PathBuf::from("<memory>"),
        config: cfg,
        batches,
        events,
        markers: Vec::new(),
        total_drops: 0,
        total_abs_frames: 0,
        bad_lines: 0,
    }
}

/// Prepared series over raw events, default parameters.
pub fn prepared_from(events: Vec<RawEvent>, game: Option<&str>) -> Prepared {
    prepare(loaded_from(events, game), Params::default())
}

/// Prepared series with explicit parameters.
pub fn prepared_with(events: Vec<RawEvent>, game: Option<&str>, params: Params) -> Prepared {
    prepare(loaded_from(events, game), params)
}

/// The common case: events attributed to the profiled fixture game.
pub fn prep(events: Vec<RawEvent>) -> Prepared {
    prepared_from(events, Some("cs2.exe"))
}

/// A marker at `ms` into the fixture timeline.
pub fn marker_at(ms: u64, label: &str) -> Marker {
    let cfg = session_cfg();
    let qpc = cfg.anchor.qpc + cfg.anchor.ms_to_ticks(ms);
    Marker {
        session_id: cfg.session_id.clone(),
        seq_no: 0,
        ts_qpc: qpc,
        ts_utc_us: cfg.anchor.qpc_to_utc_us(qpc),
        label: label.to_string(),
    }
}

/// Builds a 1 kHz-ish event stream on a microsecond clock.
///
/// Time only ever advances, so a stream is monotonic unless a test explicitly
/// asks for a violation via [`StreamBuilder::push_at_us`].
#[derive(Debug, Clone)]
pub struct StreamBuilder {
    qpc0: u64,
    freq: u64,
    /// Microseconds since the anchor.
    t_us: u64,
    resid_x: f64,
    resid_y: f64,
    events: Vec<RawEvent>,
}

impl Default for StreamBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBuilder {
    pub fn new() -> Self {
        Self {
            qpc0: FIXTURE_QPC0,
            freq: FIXTURE_FREQ,
            t_us: 0,
            resid_x: 0.0,
            resid_y: 0.0,
            events: Vec::new(),
        }
    }

    fn qpc_at(&self, us: u64) -> u64 {
        self.qpc0 + us * self.freq / 1_000_000
    }

    /// Current stream position, in milliseconds since the anchor.
    pub fn now_ms(&self) -> f64 {
        self.t_us as f64 / 1000.0
    }

    /// Advance the clock with no events — a real mouse is silent when still.
    pub fn idle_ms(&mut self, ms: u64) -> &mut Self {
        self.t_us += ms * 1000;
        self
    }

    /// One event at the current instant, then advance 1 ms.
    pub fn push(&mut self, dx: i32, dy: i32, buttons: u16, wheel: i16) -> &mut Self {
        self.events.push(RawEvent {
            ts_qpc: self.qpc_at(self.t_us),
            dx,
            dy,
            buttons,
            wheel,
            ..Default::default()
        });
        self.t_us += 1000;
        self
    }

    /// One event at an explicit microsecond offset, without touching the
    /// clock. Used to inject out-of-order timestamps.
    pub fn push_at_us(&mut self, us: u64, dx: i32, dy: i32, buttons: u16) -> &mut Self {
        self.events.push(RawEvent {
            ts_qpc: self.qpc_at(us),
            dx,
            dy,
            buttons,
            ..Default::default()
        });
        self
    }

    /// `ms` events at 1 kHz, each carrying exactly `(dx, dy)` counts — i.e. a
    /// constant velocity of `dx * 1000` counts/s.
    pub fn move_ms(&mut self, ms: u64, dx: i32, dy: i32) -> &mut Self {
        for _ in 0..ms {
            self.push(dx, dy, 0, 0);
        }
        self
    }

    /// Constant velocity expressed in counts/s, with fractional counts carried
    /// forward so the total displacement stays exact.
    pub fn move_at_ms(&mut self, ms: u64, vx: f64, vy: f64) -> &mut Self {
        for _ in 0..ms {
            let (dx, dy) = self.take_counts(vx / 1000.0, vy / 1000.0);
            self.push(dx, dy, 0, 0);
        }
        self
    }

    /// A drift plus a sinusoidal tremor, both in counts/s.
    pub fn tremor_ms(&mut self, ms: u64, drift_vx: f64, amp: f64, hz: f64) -> &mut Self {
        for _ in 0..ms {
            let t = self.t_us as f64 / 1e6;
            let vx = drift_vx + amp * (std::f64::consts::TAU * hz * t).sin();
            let (dx, dy) = self.take_counts(vx / 1000.0, 0.0);
            self.push(dx, dy, 0, 0);
        }
        self
    }

    /// The repositioning-lift signature: a long slow drift one way, a beat of
    /// stillness while the hand is off the pad, then a fast sweep back.
    pub fn lift(&mut self, drift_ms: u64, drift_v: f64, gap_ms: u64, return_v: f64) -> &mut Self {
        self.move_at_ms(drift_ms, drift_v, 0.0);
        self.idle_ms(gap_ms);
        let counts = drift_v * drift_ms as f64 / 1000.0;
        let ms = (counts / return_v * 1000.0).abs().round().max(1.0) as u64;
        self.move_at_ms(ms, -return_v, 0.0);
        self
    }

    fn take_counts(&mut self, fx: f64, fy: f64) -> (i32, i32) {
        self.resid_x += fx;
        self.resid_y += fy;
        let dx = self.resid_x.round();
        let dy = self.resid_y.round();
        self.resid_x -= dx;
        self.resid_y -= dy;
        (dx as i32, dy as i32)
    }

    /// A button transition event with no motion, then advance 1 ms.
    pub fn button(&mut self, bits: u16) -> &mut Self {
        self.push(0, 0, bits, 0)
    }

    pub fn events(&self) -> Vec<RawEvent> {
        self.events.clone()
    }

    pub fn into_events(self) -> Vec<RawEvent> {
        self.events
    }
}

/// A synthetic session spanning roughly `cells` grid cells (milliseconds), for
/// the benches: flick, correct, click, rest — the shape a real aim session has,
/// including the long idle stretches the sparse grid exists to skip.
pub fn bench_events(cells: usize) -> Vec<RawEvent> {
    use telemouse_core::event::buttons;
    let mut b = StreamBuilder::new();
    // One cycle is 25 + 10 + 10 + 5 + 1 + 39 + 1 + 400 = 491 ms.
    let cycles = (cells / 491).max(1);
    for i in 0..cycles {
        let sign = if i % 3 == 0 { -1 } else { 1 };
        b.move_ms(25, 60 * sign, (i % 7) as i32 - 3)
            .idle_ms(10)
            .move_ms(10, -10 * sign, 0)
            .idle_ms(5)
            .button(buttons::LEFT_DOWN)
            .idle_ms(39)
            .button(buttons::LEFT_UP)
            .idle_ms(400);
    }
    b.into_events()
}

/// [`bench_events`] wrapped as a loadable session.
pub fn bench_session(cells: usize) -> LoadedSession {
    loaded_from(bench_events(cells), Some("cs2.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_ms_produces_one_event_per_ms_at_1khz() {
        let mut b = StreamBuilder::new();
        b.move_ms(10, 5, 0);
        let evs = b.into_events();
        assert_eq!(evs.len(), 10);
        assert_eq!(evs[1].ts_qpc - evs[0].ts_qpc, FIXTURE_FREQ / 1000);
        assert!(evs.iter().all(|e| e.dx == 5));
    }

    #[test]
    fn fractional_velocity_preserves_total_displacement() {
        let mut b = StreamBuilder::new();
        b.move_at_ms(100, 333.0, 0.0); // 33.3 counts over 100ms
        let total: i32 = b.events().iter().map(|e| e.dx).sum();
        assert_eq!(total, 33);
    }

    #[test]
    fn idle_advances_time_without_events() {
        let mut b = StreamBuilder::new();
        b.move_ms(2, 1, 0).idle_ms(50).move_ms(2, 1, 0);
        let evs = b.into_events();
        assert_eq!(evs.len(), 4);
        let gap_us = (evs[2].ts_qpc - evs[1].ts_qpc) * 1_000_000 / FIXTURE_FREQ;
        assert_eq!(gap_us, 51_000); // 1ms of the move + 50ms idle
    }

    #[test]
    fn fixture_config_only_profiles_cs2() {
        let cfg = session_cfg();
        assert!(cfg.sens_for("cs2.exe").is_some());
        assert!(cfg.sens_for("valorant.exe").is_none());
        assert_eq!(
            cfg.sens_for("cs2.exe").unwrap().sens * cfg.sens_for("cs2.exe").unwrap().yaw_coeff,
            FIXTURE_DEG_PER_COUNT
        );
    }

    #[test]
    fn bench_events_span_the_requested_grid() {
        let evs = bench_events(10_000);
        let span = (evs.last().unwrap().ts_qpc - evs[0].ts_qpc) * 1000 / FIXTURE_FREQ;
        assert!((8_000..12_000).contains(&span), "{span} ms");
    }
}
