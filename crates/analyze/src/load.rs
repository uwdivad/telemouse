//! Reading `recordings/<session_id>.jsonl` back into memory.
//!
//! File shape (see `docs/CONVENTIONS.md`): one `telemouse_core::Envelope` per
//! line, the first line always the `session` envelope, then `batch` and
//! `marker` envelopes in capture order.
//!
//! The loader is deliberately forgiving about everything except the leading
//! session envelope: a truncated or corrupt trailing line (the capture agent
//! being killed mid-write) costs one line, not the whole recording, and is
//! surfaced as a data-quality warning instead of an error.
//!
//! At 8 M events a recording is a ~570 MB file, so the read path is written for
//! that scale: the event vector is sized from the file length up front, one
//! `String` is reused for every line, batches deserialize through a
//! borrow-based shadow struct that never allocates for the fields we only
//! read, and process names are interned so a three-hour session holds one
//! `"cs2.exe"` rather than half a million of them.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use telemouse_core::{Envelope, GameSens, Marker, RawEvent, SessionConfig};

/// Rough serialized size of one motion event, used to size the event vector
/// from the file length. Measured over real recordings: a motion-only event
/// serializes to `{"ts_qpc":51234567890,"dx":-3,"dy":2},` ≈ 45 B, and batch
/// envelopes add their own overhead; 71 B/event lands within ~10 % on captures
/// with the usual mix of clicks and idle gaps. Over-reserving costs one
/// oversized allocation, under-reserving costs a doubling — both are cheap
/// compared to the growth series this replaces.
const BYTES_PER_EVENT: u64 = 71;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is empty")]
    Empty { path: PathBuf },
    #[error("{path}: first line is not a session envelope ({detail})")]
    NoSessionHeader { path: PathBuf, detail: String },
}

/// Per-batch bookkeeping kept after the events are flattened.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchMeta {
    pub seq_no: u64,
    pub ts_anchor_us: i64,
    /// Interned foreground process name — one allocation per distinct name.
    pub game: Option<Arc<str>>,
    pub pointer_locked: bool,
    pub drops_since_last: u32,
    /// Absolute-motion frames the capture agent discarded before this batch.
    pub abs_frames_since_last: u32,
    /// QPC of this batch's first event, for the batch-latency check.
    pub first_event_qpc: Option<u64>,
    pub event_count: usize,
}

/// A recording, flattened and ready for the metric modules.
#[derive(Debug, Clone)]
pub struct LoadedSession {
    pub path: PathBuf,
    pub config: SessionConfig,
    /// All events from all batches, concatenated in file order.
    pub events: Vec<RawEvent>,
    pub markers: Vec<Marker>,
    pub batches: Vec<BatchMeta>,
    /// Sum of `drops_since_last` across batches.
    pub total_drops: u64,
    /// Sum of `abs_frames_since_last` across batches.
    pub total_abs_frames: u64,
    /// Lines that failed to parse (truncated tail, corruption).
    pub bad_lines: usize,
}

impl LoadedSession {
    /// Events per foreground process name, batch-attributed.
    pub fn game_event_counts(&self) -> BTreeMap<String, usize> {
        let mut m = BTreeMap::new();
        for b in &self.batches {
            if let Some(g) = &b.game {
                *m.entry(g.to_ascii_lowercase()).or_insert(0) += b.event_count;
            }
        }
        m
    }

    /// The process that owned the most events — the session's "the game".
    pub fn dominant_game(&self) -> Option<String> {
        self.game_event_counts()
            .into_iter()
            .max_by_key(|(name, n)| (*n, std::cmp::Reverse(name.clone())))
            .map(|(name, _)| name)
    }

    /// Dominant process plus its share of all attributed events. A session
    /// where the "game" only covers half the events is really two sessions.
    pub fn dominant_game_share(&self) -> Option<(String, f64)> {
        let counts = self.game_event_counts();
        let total: usize = counts.values().sum();
        let (name, n) = counts
            .into_iter()
            .max_by_key(|(name, n)| (*n, std::cmp::Reverse(name.clone())))?;
        (total > 0).then(|| (name, n as f64 / total as f64))
    }

    /// Distinct `device_ix` values seen in the events.
    pub fn device_indices(&self) -> Vec<u8> {
        let mut seen = [false; 256];
        for e in &self.events {
            seen[e.device_ix as usize] = true;
        }
        (0..=255u8).filter(|&i| seen[i as usize]).collect()
    }

    /// Aim-space conversion for the dominant game, plus whether we had to fall
    /// back. The fallback (`sens 1.0`, Source-style `0.022` coefficients) keeps
    /// degree-valued metrics computable but not comparable across sessions, so
    /// it is flagged all the way out to the report.
    pub fn resolve_aim(&self) -> (Option<String>, GameSens, bool) {
        let game = self.dominant_game();
        let sens = game.as_deref().and_then(|g| self.config.sens_for(g));
        match sens {
            Some(g) => (game, *g, false),
            None => (
                game,
                GameSens {
                    sens: 1.0,
                    yaw_coeff: 0.022,
                    pitch_coeff: 0.022,
                },
                true,
            ),
        }
    }
}

/// A cheap header-and-counters scan for `telemouse-analyze list`, which never
/// needs the events themselves.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionIndexEntry {
    pub path: PathBuf,
    pub session_id: String,
    pub started_utc_us: i64,
    pub duration_s: f64,
    pub events: u64,
    pub drops: u64,
    pub games: Vec<String>,
    pub bad_lines: usize,
}

/// The batch fields the analyzer actually reads, borrowed straight out of the
/// line buffer. `session_id`, `screen_w/h` and the cursor sample are ignored
/// rather than allocated; serde skips unknown fields by default.
#[derive(Debug, Deserialize)]
struct BatchRef<'a> {
    seq_no: u64,
    ts_anchor_us: i64,
    #[serde(borrow, default)]
    game: Option<std::borrow::Cow<'a, str>>,
    #[serde(default)]
    pointer_locked: bool,
    #[serde(default)]
    drops_since_last: u32,
    #[serde(default)]
    abs_frames_since_last: u32,
    events: Vec<RawEvent>,
}

/// The three envelope prefixes, matched before invoking serde so the hot path
/// never pays for an internally-tagged enum's content buffering.
const BATCH_TAG: &str = "{\"type\":\"batch\"";
const MARKER_TAG: &str = "{\"type\":\"marker\"";
const SESSION_TAG: &str = "{\"type\":\"session\"";

/// Interns process names so a long session holds one copy of each.
#[derive(Default)]
struct GameInterner(HashMap<String, Arc<str>>);

impl GameInterner {
    fn get(&mut self, s: &str) -> Arc<str> {
        if let Some(v) = self.0.get(s) {
            return Arc::clone(v);
        }
        let v: Arc<str> = Arc::from(s);
        self.0.insert(s.to_string(), Arc::clone(&v));
        v
    }
}

fn open(path: &Path) -> Result<(BufReader<File>, u64), LoadError> {
    let file = File::open(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok((BufReader::with_capacity(1 << 20, file), size))
}

/// Read one line into `buf`, stripping the trailing newline. `Ok(false)` at EOF.
fn next_line(path: &Path, r: &mut BufReader<File>, buf: &mut String) -> Result<bool, LoadError> {
    buf.clear();
    let n = r.read_line(buf).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if n == 0 {
        return Ok(false);
    }
    while buf.ends_with('\n') || buf.ends_with('\r') {
        buf.pop();
    }
    Ok(true)
}

/// Parse the session header from the first non-empty line.
fn read_header(
    path: &Path,
    r: &mut BufReader<File>,
    buf: &mut String,
) -> Result<SessionConfig, LoadError> {
    while next_line(path, r, buf)? {
        if buf.trim().is_empty() {
            continue;
        }
        return match Envelope::from_json(buf) {
            Ok(Envelope::Session(cfg)) => Ok(cfg),
            Ok(other) => Err(LoadError::NoSessionHeader {
                path: path.to_path_buf(),
                detail: format!("found a {} envelope", envelope_kind(&other)),
            }),
            Err(e) => Err(LoadError::NoSessionHeader {
                path: path.to_path_buf(),
                detail: e.to_string(),
            }),
        };
    }
    Err(LoadError::Empty {
        path: path.to_path_buf(),
    })
}

fn envelope_kind(e: &Envelope) -> &'static str {
    match e {
        Envelope::Session(_) => "session",
        Envelope::Batch(_) => "batch",
        Envelope::Marker(_) => "marker",
    }
}

/// Load a recording fully into memory.
pub fn load_session(path: &Path) -> Result<LoadedSession, LoadError> {
    let (mut reader, size) = open(path)?;
    let mut line = String::with_capacity(64 * 1024);
    let config = read_header(path, &mut reader, &mut line)?;

    let mut events: Vec<RawEvent> = Vec::with_capacity((size / BYTES_PER_EVENT) as usize);
    let mut markers = Vec::new();
    let mut batches: Vec<BatchMeta> = Vec::new();
    let mut total_drops = 0u64;
    let mut total_abs_frames = 0u64;
    let mut bad_lines = 0usize;
    let mut games = GameInterner::default();

    while next_line(path, &mut reader, &mut line)? {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(BATCH_TAG) {
            match serde_json::from_str::<BatchRef>(&line) {
                Ok(mut b) => {
                    total_drops += b.drops_since_last as u64;
                    total_abs_frames += b.abs_frames_since_last as u64;
                    batches.push(BatchMeta {
                        seq_no: b.seq_no,
                        ts_anchor_us: b.ts_anchor_us,
                        game: b.game.as_deref().map(|g| games.get(g)),
                        pointer_locked: b.pointer_locked,
                        drops_since_last: b.drops_since_last,
                        abs_frames_since_last: b.abs_frames_since_last,
                        first_event_qpc: b.events.first().map(|e| e.ts_qpc),
                        event_count: b.events.len(),
                    });
                    events.append(&mut b.events);
                }
                Err(_) => bad_lines += 1,
            }
            continue;
        }
        if line.starts_with(MARKER_TAG) {
            match serde_json::from_str::<Marker>(&line) {
                Ok(m) => markers.push(m),
                Err(_) => bad_lines += 1,
            }
            continue;
        }
        // A second session envelope mid-file (topic compaction replay, an
        // appended session) is informational, not fatal.
        if line.starts_with(SESSION_TAG) {
            continue;
        }
        // Anything whose tag is not where we expect it still gets the general
        // path before being written off as corrupt.
        match Envelope::from_json(&line) {
            Ok(Envelope::Batch(b)) => {
                total_drops += b.drops_since_last as u64;
                total_abs_frames += b.abs_frames_since_last as u64;
                batches.push(BatchMeta {
                    seq_no: b.seq_no,
                    ts_anchor_us: b.ts_anchor_us,
                    game: b.game.as_deref().map(|g| games.get(g)),
                    pointer_locked: b.pointer_locked,
                    drops_since_last: b.drops_since_last,
                    abs_frames_since_last: b.abs_frames_since_last,
                    first_event_qpc: b.events.first().map(|e| e.ts_qpc),
                    event_count: b.events.len(),
                });
                events.extend_from_slice(&b.events);
            }
            Ok(Envelope::Marker(m)) => markers.push(m),
            Ok(Envelope::Session(_)) => {}
            Err(_) => bad_lines += 1,
        }
    }

    Ok(LoadedSession {
        path: path.to_path_buf(),
        config,
        events,
        markers,
        batches,
        total_drops,
        total_abs_frames,
        bad_lines,
    })
}

/// Scan a recording for the listing table without retaining its events.
pub fn scan_session(path: &Path) -> Result<SessionIndexEntry, LoadError> {
    let (mut reader, _) = open(path)?;
    let mut line = String::with_capacity(64 * 1024);
    let config = read_header(path, &mut reader, &mut line)?;

    let mut events = 0u64;
    let mut drops = 0u64;
    let mut bad_lines = 0usize;
    let mut first_qpc: Option<u64> = None;
    let mut last_qpc: Option<u64> = None;
    let mut games: BTreeMap<String, usize> = BTreeMap::new();

    while next_line(path, &mut reader, &mut line)? {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(MARKER_TAG) || line.starts_with(SESSION_TAG) {
            continue;
        }
        match serde_json::from_str::<BatchRef>(&line) {
            Ok(b) => {
                drops += b.drops_since_last as u64;
                events += b.events.len() as u64;
                if let Some(g) = &b.game {
                    *games.entry(g.to_ascii_lowercase()).or_insert(0) += b.events.len();
                }
                if let Some(e) = b.events.first() {
                    first_qpc.get_or_insert(e.ts_qpc);
                }
                if let Some(e) = b.events.last() {
                    last_qpc = Some(e.ts_qpc);
                }
            }
            Err(_) => bad_lines += 1,
        }
    }

    let duration_s = match (first_qpc, last_qpc) {
        (Some(a), Some(b)) => config.anchor.ticks_to_us(a, b) as f64 / 1e6,
        _ => 0.0,
    };
    let mut games: Vec<(String, usize)> = games.into_iter().collect();
    games.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    Ok(SessionIndexEntry {
        path: path.to_path_buf(),
        session_id: config.session_id,
        started_utc_us: config.started_utc_us,
        duration_s,
        events,
        drops,
        games: games.into_iter().map(|(g, _)| g).collect(),
        bad_lines,
    })
}

/// Every `*.jsonl` in `dir`, scanned and sorted by start time (oldest first).
/// Unreadable files are skipped with a warning rather than failing the listing.
pub fn scan_dir(dir: &Path) -> Result<Vec<SessionIndexEntry>, LoadError> {
    let rd = std::fs::read_dir(dir).map_err(|source| LoadError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        match scan_session(&p) {
            Ok(e) => out.push(e),
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "skipping unreadable recording")
            }
        }
    }
    out.sort_by_key(|e| (e.started_utc_us, e.session_id.clone()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FIXTURE_FREQ, batch_env, session_cfg, write_lines};
    use telemouse_core::{Batch, event::buttons};

    fn ev(qpc: u64, dx: i32, dy: i32, buttons: u16) -> RawEvent {
        RawEvent {
            ts_qpc: qpc,
            dx,
            dy,
            buttons,
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_a_hand_built_recording() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let mut lines = vec![Envelope::Session(cfg.clone()).to_json().unwrap()];
        lines.push(
            batch_env(
                &cfg,
                0,
                Some("cs2.exe"),
                3,
                vec![
                    ev(cfg.anchor.qpc, 1, 0, 0),
                    ev(cfg.anchor.qpc + 10_000, 2, -1, 0),
                ],
            )
            .to_json()
            .unwrap(),
        );
        lines.push(
            Envelope::Marker(Marker {
                session_id: cfg.session_id.clone(),
                seq_no: 0,
                ts_qpc: cfg.anchor.qpc + 20_000,
                ts_utc_us: cfg.anchor.qpc_to_utc_us(cfg.anchor.qpc + 20_000),
                label: "clutch".into(),
            })
            .to_json()
            .unwrap(),
        );
        lines.push(
            batch_env(
                &cfg,
                1,
                Some("cs2.exe"),
                4,
                vec![ev(cfg.anchor.qpc + 30_000, 0, 0, buttons::LEFT_DOWN)],
            )
            .to_json()
            .unwrap(),
        );
        let path = write_lines(dir.path(), "s-test.jsonl", &lines);

        let s = load_session(&path).unwrap();
        assert_eq!(s.config.session_id, "s-test");
        assert_eq!(s.events.len(), 3);
        assert_eq!(s.markers.len(), 1);
        assert_eq!(s.markers[0].label, "clutch");
        assert_eq!(s.batches.len(), 2);
        // Drops accumulate across batches.
        assert_eq!(s.total_drops, 7);
        assert_eq!(s.bad_lines, 0);
        assert_eq!(s.dominant_game().as_deref(), Some("cs2.exe"));
        let (game, sens, fallback) = s.resolve_aim();
        assert_eq!(game.as_deref(), Some("cs2.exe"));
        assert!(!fallback);
        assert_eq!(sens.sens, 2.0);
        // Interning: both batches share one allocation for the process name.
        let a = s.batches[0].game.clone().unwrap();
        let b = s.batches[1].game.clone().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(s.batches[0].first_event_qpc, Some(cfg.anchor.qpc));
    }

    #[test]
    fn scan_matches_a_full_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let evs: Vec<RawEvent> = (0..50)
            .map(|i| ev(cfg.anchor.qpc + i * (FIXTURE_FREQ / 1000), 1, 0, 0))
            .collect();
        let lines = vec![
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            batch_env(&cfg, 0, Some("cs2.exe"), 2, evs)
                .to_json()
                .unwrap(),
        ];
        let path = write_lines(dir.path(), "s-test.jsonl", &lines);

        let scanned = scan_session(&path).unwrap();
        assert_eq!(scanned.events, 50);
        assert_eq!(scanned.drops, 2);
        assert_eq!(scanned.games, vec!["cs2.exe".to_string()]);
        assert!((scanned.duration_s - 0.049).abs() < 1e-6);

        let loaded = load_session(&path).unwrap();
        assert_eq!(loaded.events.len() as u64, scanned.events);
        assert_eq!(loaded.total_drops, scanned.drops);
    }

    #[test]
    fn corrupt_trailing_line_costs_one_line_not_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let lines = vec![
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            batch_env(&cfg, 0, None, 0, vec![ev(cfg.anchor.qpc, 1, 1, 0)])
                .to_json()
                .unwrap(),
            r#"{"type":"batch","session_id":"s-tes"#.to_string(), // truncated
        ];
        let path = write_lines(dir.path(), "s-test.jsonl", &lines);
        let s = load_session(&path).unwrap();
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.bad_lines, 1);
    }

    #[test]
    fn missing_header_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let path = write_lines(
            dir.path(),
            "bad.jsonl",
            &[batch_env(&cfg, 0, None, 0, vec![]).to_json().unwrap()],
        );
        assert!(matches!(
            load_session(&path),
            Err(LoadError::NoSessionHeader { .. })
        ));

        let empty = write_lines(dir.path(), "empty.jsonl", &[]);
        assert!(matches!(load_session(&empty), Err(LoadError::Empty { .. })));

        let missing = dir.path().join("nope.jsonl");
        assert!(matches!(load_session(&missing), Err(LoadError::Io { .. })));
    }

    #[test]
    fn dominant_game_wins_on_event_count_and_falls_back_without_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let e = |i: u64| ev(cfg.anchor.qpc + i * 10_000, 1, 0, 0);
        let lines = vec![
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            batch_env(&cfg, 0, Some("cs2.exe"), 0, vec![e(0)])
                .to_json()
                .unwrap(),
            batch_env(&cfg, 1, Some("valorant.exe"), 0, (1..5).map(e).collect())
                .to_json()
                .unwrap(),
        ];
        let path = write_lines(dir.path(), "s-test.jsonl", &lines);
        let s = load_session(&path).unwrap();
        assert_eq!(s.dominant_game().as_deref(), Some("valorant.exe"));
        let (name, share) = s.dominant_game_share().unwrap();
        assert_eq!(name, "valorant.exe");
        assert!((share - 0.8).abs() < 1e-9);
        let (_, sens, fallback) = s.resolve_aim();
        assert!(fallback, "valorant.exe has no profile in the fixture");
        assert_eq!(sens.sens, 1.0);
        assert_eq!(sens.yaw_coeff, 0.022);
    }

    #[test]
    fn scan_dir_lists_only_jsonl_sorted_by_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = session_cfg();
        a.session_id = "later".into();
        a.started_utc_us += 60_000_000;
        let mut b = session_cfg();
        b.session_id = "earlier".into();
        for cfg in [&a, &b] {
            write_lines(
                dir.path(),
                &format!("{}.jsonl", cfg.session_id),
                &[Envelope::Session(cfg.clone()).to_json().unwrap()],
            );
        }
        std::fs::write(dir.path().join("notes.txt"), "ignore me").unwrap();

        let list = scan_dir(dir.path()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "earlier");
        assert_eq!(list[1].session_id, "later");
        assert_eq!(list[0].events, 0);
    }

    #[test]
    fn batch_metadata_survives_the_flattening() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let mut b = Batch {
            session_id: cfg.session_id.clone(),
            seq_no: 9,
            ts_anchor_us: cfg.started_utc_us,
            game: Some("cs2.exe".into()),
            pointer_locked: true,
            screen_w: 2560,
            screen_h: 1440,
            cursor_x: None,
            cursor_y: None,
            drops_since_last: 12,
            abs_frames_since_last: 4,
            events: vec![RawEvent::default()],
        };
        b.events[0].ts_qpc = cfg.anchor.qpc;
        let path = write_lines(
            dir.path(),
            "s-test.jsonl",
            &[
                Envelope::Session(cfg).to_json().unwrap(),
                Envelope::Batch(b).to_json().unwrap(),
            ],
        );
        let s = load_session(&path).unwrap();
        let m = &s.batches[0];
        assert_eq!(m.seq_no, 9);
        assert!(m.pointer_locked);
        assert_eq!(m.drops_since_last, 12);
        assert_eq!(m.abs_frames_since_last, 4);
        assert_eq!(m.event_count, 1);
        assert_eq!(s.total_abs_frames, 4);
    }

    /// New wire fields must survive the borrow-based fast path, and old
    /// recordings that predate them must still parse.
    #[test]
    fn new_and_legacy_event_fields_both_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let mut e = RawEvent {
            ts_qpc: cfg.anchor.qpc,
            dx: 3,
            dy: -2,
            ..Default::default()
        };
        e.wheel_h = -120;
        e.device_ix = 2;
        let path = write_lines(
            dir.path(),
            "s-test.jsonl",
            &[
                Envelope::Session(cfg.clone()).to_json().unwrap(),
                batch_env(&cfg, 0, Some("cs2.exe"), 0, vec![e]).to_json().unwrap(),
                // A pre-device-tracking line, written by hand.
                format!(
                    r#"{{"type":"batch","session_id":"s-test","seq_no":1,"ts_anchor_us":{},"pointer_locked":true,"screen_w":1,"screen_h":1,"drops_since_last":0,"events":[{{"ts_qpc":{},"dx":1,"dy":1,"buttons":0,"wheel":0}}]}}"#,
                    cfg.started_utc_us,
                    cfg.anchor.qpc + 1000
                ),
            ],
        );
        let s = load_session(&path).unwrap();
        assert_eq!(s.bad_lines, 0);
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.events[0].wheel_h, -120);
        assert_eq!(s.events[0].device_ix, 2);
        assert_eq!(s.events[1].device_ix, 0);
        assert_eq!(s.device_indices(), vec![0, 2]);
    }
}
