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
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use telemouse_core::recordings::SessionMeta;
use telemouse_core::{Envelope, GameSens, Marker, RawEvent, SessionConfig};

/// Rough serialized size of one motion event, used to size the event vector
/// from the file length. Measured over real recordings: a motion-only event
/// serializes to `{"ts_qpc":51234567890,"dx":-3,"dy":2},` ≈ 45 B, and batch
/// envelopes add their own overhead; 71 B/event lands within ~10 % on captures
/// with the usual mix of clicks and idle gaps. Over-reserving costs one
/// oversized allocation, under-reserving costs a doubling — both are cheap
/// compared to the growth series this replaces.
const BYTES_PER_EVENT: u64 = 71;

/// Versioned, disposable metadata cache used by [`scan_dir`]. It intentionally
/// does not end in `.jsonl`, so it can live beside recordings without ever
/// being mistaken for one.
const INDEX_CACHE_FILE: &str = ".telemouse-analyze-index-v1.json";
const INDEX_CACHE_VERSION: u32 = 1;
const SIGNATURE_SAMPLE_BYTES: u64 = 4 * 1024;
static CACHE_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Longest parse error kept from a corrupt line. A type mismatch quotes the
/// offending value, so a 64 KB field would otherwise land whole in a warning,
/// a JSON report, and the listing cache.
const MAX_BAD_LINE_ERROR: usize = 200;

/// Progress is logged at whichever comes first: this much wall time, or
/// [`PROGRESS_BYTE_SHARE`] of the file.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_BYTE_SHARE: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    // The message must not repeat `{source}`: `thiserror` already chains a
    // field named `source`, and every caller (anyhow, tracing) prints the
    // chain — so spelling it here produced "failed to read x: no such file:
    // no such file".
    #[error("failed to read {path}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is empty")]
    Empty { path: PathBuf },
    #[error("{path}: first line is not a session envelope ({detail})")]
    NoSessionHeader { path: PathBuf, detail: String },
}

/// `e` plus every source beneath it, joined by `: `.
///
/// `tracing`'s `%` fields print `Display` only, which for a `#[source]`-chained
/// error is just the outer message. Anything that logs a [`LoadError`] rather
/// than returning it goes through here so the cause survives.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}

/// What the loader saw of a recording's unparseable lines.
///
/// The count alone cannot answer the question that matters — *was the tail
/// truncated, or is the middle of the file corrupt?* A killed capture agent
/// costs the last line; anything else means events are missing from the
/// interior and every rate over the session is understated.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BadLines {
    pub count: usize,
    /// 1-based line numbers, as a text editor counts them.
    pub first_line: Option<u64>,
    pub last_line: Option<u64>,
    /// True when no line parsed *after* the first bad one — the signature of
    /// a truncated tail rather than interior corruption. Vacuously true when
    /// there are no bad lines at all.
    pub tail_only: bool,
    /// The first parse error, capped at [`MAX_BAD_LINE_ERROR`] characters.
    pub first_error: Option<String>,
}

impl BadLines {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Note one unparseable line at 1-based `line_no`.
    fn record(&mut self, line_no: u64, error: &serde_json::Error) {
        self.count += 1;
        self.first_line.get_or_insert(line_no);
        self.last_line = Some(line_no);
        if self.first_error.is_none() {
            let text = error.to_string();
            self.first_error = Some(match text.char_indices().nth(MAX_BAD_LINE_ERROR) {
                Some((cut, _)) => format!("{}…", &text[..cut]),
                None => text,
            });
        }
    }

    /// Called once at the end of a pass: `last_good_line` is the last line
    /// number that parsed.
    fn seal(&mut self, last_good_line: u64) {
        self.tail_only = self.first_line.is_none_or(|first| first > last_good_line);
    }

    /// The `BAD` column: `-`, `12` for a truncated tail, `12!` when the
    /// corruption reaches into the body of the recording.
    pub fn flag(&self) -> String {
        match (self.count, self.tail_only) {
            (0, _) => "-".to_string(),
            (n, true) => n.to_string(),
            (n, false) => format!("{n}!"),
        }
    }
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
    pub bad_lines: BadLines,
    /// Bytes read off disk, and how long that took — reported as the `load`
    /// phase of `--timing`, which is otherwise invisible next to `compute_ms`.
    pub bytes: u64,
    pub load_ms: f64,
}

/// Events per foreground process, batch-attributed.
///
/// Keyed by the interned `Arc<str>` the loader already holds — names are
/// lower-cased at intern time, so this is a pointer-keyed tally rather than
/// one `to_ascii_lowercase` allocation per batch. [`GameCounts::dominant`]
/// breaks ties the same way the old `BTreeMap` version did (highest count,
/// then the name that sorts first).
pub type GameCounts = HashMap<Arc<str>, usize>;

/// The per-process tally, computed once and shared by every caller that needs
/// "the game" ([`crate::series::prepare`] stores the result on `Prepared`).
pub fn game_event_counts(batches: &[BatchMeta]) -> GameCounts {
    let mut m: GameCounts = HashMap::new();
    for b in batches {
        if let Some(g) = &b.game {
            *m.entry(Arc::clone(g)).or_insert(0) += b.event_count;
        }
    }
    m
}

/// The process that owned the most events, and its share of all attributed
/// events. A session where the "game" only covers half the events is really
/// two sessions.
pub fn dominant_game(counts: &GameCounts) -> Option<(Arc<str>, f64)> {
    let total: usize = counts.values().sum();
    let (name, n) = counts
        .iter()
        .max_by(|(a_name, a), (b_name, b)| a.cmp(b).then_with(|| b_name.cmp(a_name)))?;
    (total > 0).then(|| (Arc::clone(name), *n as f64 / total as f64))
}

/// Aim-space conversion for `game`, plus whether we had to fall back. The
/// fallback (`sens 1.0`, Source-style `0.022` coefficients) keeps degree-valued
/// metrics computable but not comparable across sessions, so it is flagged all
/// the way out to the report.
pub fn resolve_aim(config: &SessionConfig, game: Option<&str>) -> (GameSens, bool) {
    match game.and_then(|g| config.sens_for(g)) {
        Some(g) => (*g, false),
        None => (
            GameSens {
                sens: 1.0,
                yaw_coeff: 0.022,
                pitch_coeff: 0.022,
            },
            true,
        ),
    }
}

impl LoadedSession {
    /// Events per foreground process name, batch-attributed.
    pub fn game_event_counts(&self) -> GameCounts {
        game_event_counts(&self.batches)
    }

    /// The process that owned the most events — the session's "the game".
    pub fn dominant_game(&self) -> Option<String> {
        dominant_game(&self.game_event_counts()).map(|(name, _)| name.to_string())
    }

    /// Dominant process plus its share of all attributed events.
    pub fn dominant_game_share(&self) -> Option<(String, f64)> {
        dominant_game(&self.game_event_counts()).map(|(name, share)| (name.to_string(), share))
    }

    /// Distinct `device_ix` values seen in the events.
    pub fn device_indices(&self) -> Vec<u8> {
        let mut seen = [false; 256];
        for e in &self.events {
            seen[e.device_ix as usize] = true;
        }
        (0..=255u8).filter(|&i| seen[i as usize]).collect()
    }

    /// Aim-space conversion for the dominant game.
    pub fn resolve_aim(&self) -> (Option<String>, GameSens, bool) {
        let game = self.dominant_game();
        let (sens, fallback) = resolve_aim(&self.config, game.as_deref());
        (game, sens, fallback)
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
    /// Unparseable lines, with enough detail to tell a truncated tail from a
    /// corrupt interior. Older index caches stored a bare count under the same
    /// name; they fail to parse and are rebuilt, which is what the cache is
    /// for.
    #[serde(default)]
    pub bad_lines: BadLines,
    /// Sinks that lost envelopes during the run, as `(sink, count)`, from the
    /// `<session_id>.meta.json` sidecar the capture agent writes when it
    /// stops. Empty when nothing was lost — or when there is no sidecar
    /// (recordings made before it existed, or an agent that was killed).
    #[serde(default)]
    pub losses: Vec<(String, u64)>,
    /// How the run ended, from the sidecar (`interrupt`, `duration`, or one
    /// of the thread-exit reasons). `None` without a sidecar.
    #[serde(default)]
    pub exit: Option<String>,
}

impl SessionIndexEntry {
    /// Fill the sidecar-derived fields for a recording at `path`.
    fn with_sidecar(mut self, path: &Path) -> Self {
        if let Some(meta) = read_sidecar(path) {
            self.losses = meta.losses();
            self.exit = Some(meta.exit.clone()).filter(|e| !e.is_empty());
        } else {
            self.losses.clear();
            self.exit = None;
        }
        self
    }

    /// The `EXIT` column: how the run ended, `-` without a sidecar.
    pub fn exit_text(&self) -> &str {
        self.exit.as_deref().unwrap_or("-")
    }

    /// True when the sidecar still says `running`: the agent never wrote a
    /// final reason, so the recording stops wherever it stopped.
    pub fn unfinished(&self) -> bool {
        self.exit.as_deref() == Some(telemouse_core::recordings::ExitReason::Running.as_str())
    }

    /// The `LOSS` column: `-` or `kafka=400,jsonl=2`.
    pub fn losses_text(&self) -> String {
        if self.losses.is_empty() {
            "-".to_string()
        } else {
            self.losses
                .iter()
                .map(|(n, k)| format!("{n}={k}"))
                .collect::<Vec<_>>()
                .join(",")
        }
    }
}

/// The metadata sidecar next to `recording`, if the agent wrote one.
pub fn read_sidecar(recording: &Path) -> Option<SessionMeta> {
    let path = recording.with_extension("meta.json");
    let text = fs::read_to_string(path).ok()?;
    SessionMeta::from_json(&text).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Timestamp {
    pub secs: u64,
    pub nanos: u32,
}

/// A recording's identity for cache validation: size, timestamps, and content
/// samples. Used by the listing cache here and by the per-session report cache
/// in [`crate::trend`], which needs the same question answered — *is this
/// cached artifact still about this file?* — and cannot answer it with mtime
/// alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileSignature {
    pub len: u64,
    pub modified: Option<Timestamp>,
    pub created: Option<Timestamp>,
    /// FNV-1a over small samples at the front, middle and tail. Size and mtime
    /// are the primary invalidators; this also catches replacement files on
    /// filesystems with coarse timestamp precision.
    pub sample_hash: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedSession {
    /// Path relative to the directory containing the cache: the file's stable
    /// identity for directory-listing purposes.
    file_name: PathBuf,
    signature: FileSignature,
    entry: SessionIndexEntry,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SessionIndexCache {
    version: u32,
    files: Vec<CachedSession>,
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
///
/// Every interned name is lower-cased, so the per-process tally can be keyed
/// by the `Arc` directly instead of re-lowercasing a name per batch. Both
/// spellings map to the same `Arc`, so a recording that switches between
/// `CS2.exe` and `cs2.exe` still costs one allocation and one tally entry.
#[derive(Default)]
struct GameInterner(HashMap<String, Arc<str>>);

impl GameInterner {
    fn get(&mut self, s: &str) -> Arc<str> {
        if let Some(v) = self.0.get(s) {
            return Arc::clone(v);
        }
        let lower = s.to_ascii_lowercase();
        let v = match self.0.get(lower.as_str()) {
            Some(v) => Arc::clone(v),
            None => {
                let v: Arc<str> = Arc::from(lower.as_str());
                self.0.insert(lower.clone(), Arc::clone(&v));
                v
            }
        };
        self.0.insert(s.to_string(), Arc::clone(&v));
        v
    }
}

/// Logs what a long load is doing, so an eight-minute read of a 570 MB
/// recording is not a silent terminal.
struct LoadProgress {
    total: u64,
    read: u64,
    started: Instant,
    last_log: Instant,
    next_bytes: u64,
    step: u64,
}

impl LoadProgress {
    fn start(path: &Path, total: u64) -> Self {
        tracing::info!(
            path = %path.display(),
            mb = format_args!("{:.1}", total as f64 / (1024.0 * 1024.0)),
            "loading recording"
        );
        let step = (total / PROGRESS_BYTE_SHARE).max(1);
        let now = Instant::now();
        Self {
            total,
            read: 0,
            started: now,
            last_log: now,
            next_bytes: step,
            step,
        }
    }

    /// One line of `n` bytes consumed.
    #[inline]
    fn advance(&mut self, n: usize, events: usize) {
        self.read += n as u64;
        if self.read < self.next_bytes {
            // The clock is only read on the byte milestones: `Instant::now()`
            // per line would be a syscall-ish cost per event batch.
            return;
        }
        self.next_bytes = self.read + self.step;
        let now = Instant::now();
        if now.duration_since(self.last_log) < PROGRESS_INTERVAL && self.read < self.total {
            return;
        }
        self.last_log = now;
        tracing::info!(
            pct = format_args!("{:.0}", 100.0 * self.read as f64 / self.total.max(1) as f64),
            mb = format_args!("{:.1}", self.read as f64 / (1024.0 * 1024.0)),
            events,
            elapsed_s = format_args!("{:.1}", now.duration_since(self.started).as_secs_f64()),
            "loading"
        );
    }

    /// Milliseconds the read took, logged with its throughput.
    fn finish(self, path: &Path, events: usize) -> f64 {
        let secs = self.started.elapsed().as_secs_f64();
        let mb = self.read as f64 / (1024.0 * 1024.0);
        tracing::info!(
            path = %path.display(),
            events,
            mb = format_args!("{mb:.1}"),
            elapsed_ms = format_args!("{:.1}", secs * 1000.0),
            mb_per_s = format_args!("{:.1}", if secs > 0.0 { mb / secs } else { 0.0 }),
            "loaded recording"
        );
        secs * 1000.0
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

/// Read one line into `buf`, stripping the trailing newline. Returns the bytes
/// consumed *including* the line terminator, so the caller can report progress
/// against the file length; `Ok(0)` at EOF.
fn next_line(path: &Path, r: &mut BufReader<File>, buf: &mut String) -> Result<usize, LoadError> {
    buf.clear();
    let n = r.read_line(buf).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if n == 0 {
        return Ok(0);
    }
    while buf.ends_with('\n') || buf.ends_with('\r') {
        buf.pop();
    }
    Ok(n)
}

/// Parse the session header from the first non-empty line, and report how many
/// lines and bytes that consumed so the body's line numbers are the file's.
fn read_header(
    path: &Path,
    r: &mut BufReader<File>,
    buf: &mut String,
) -> Result<(SessionConfig, u64, u64), LoadError> {
    let mut lines = 0u64;
    let mut bytes = 0u64;
    loop {
        let n = next_line(path, r, buf)?;
        if n == 0 {
            break;
        }
        lines += 1;
        bytes += n as u64;
        if buf.trim().is_empty() {
            continue;
        }
        return match Envelope::from_json(buf) {
            Ok(Envelope::Session(cfg)) => Ok((cfg, lines, bytes)),
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
    let mut progress = LoadProgress::start(path, size);
    let mut line = String::with_capacity(64 * 1024);
    let (config, mut line_no, header_bytes) = read_header(path, &mut reader, &mut line)?;
    progress.advance(header_bytes as usize, 0);

    let mut events: Vec<RawEvent> = Vec::with_capacity((size / BYTES_PER_EVENT) as usize);
    let mut markers = Vec::new();
    let mut batches: Vec<BatchMeta> = Vec::new();
    let mut total_drops = 0u64;
    let mut total_abs_frames = 0u64;
    let mut bad_lines = BadLines::default();
    let mut last_good_line = line_no;
    let mut games = GameInterner::default();

    loop {
        let n = next_line(path, &mut reader, &mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        progress.advance(n, events.len());
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(BATCH_TAG) {
            match serde_json::from_str::<BatchRef>(&line) {
                Ok(mut b) => {
                    last_good_line = line_no;
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
                Err(e) => bad_lines.record(line_no, &e),
            }
            continue;
        }
        if line.starts_with(MARKER_TAG) {
            match serde_json::from_str::<Marker>(&line) {
                Ok(m) => {
                    last_good_line = line_no;
                    markers.push(m);
                }
                Err(e) => bad_lines.record(line_no, &e),
            }
            continue;
        }
        // A second session envelope mid-file (topic compaction replay, an
        // appended session) is informational, not fatal.
        if line.starts_with(SESSION_TAG) {
            last_good_line = line_no;
            continue;
        }
        // Anything whose tag is not where we expect it still gets the general
        // path before being written off as corrupt.
        match Envelope::from_json(&line) {
            Ok(Envelope::Batch(b)) => {
                last_good_line = line_no;
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
            Ok(Envelope::Marker(m)) => {
                last_good_line = line_no;
                markers.push(m);
            }
            Ok(Envelope::Session(_)) => last_good_line = line_no,
            Err(e) => bad_lines.record(line_no, &e),
        }
    }
    bad_lines.seal(last_good_line);
    warn_bad_lines(path, &bad_lines);
    let load_ms = progress.finish(path, events.len());

    Ok(LoadedSession {
        path: path.to_path_buf(),
        config,
        events,
        markers,
        batches,
        total_drops,
        total_abs_frames,
        bad_lines,
        bytes: size,
        load_ms,
    })
}

/// One warning per load, with what a person needs to decide whether the
/// recording is usable: where the damage starts, where it ends, whether it is
/// only the tail, and what the parser actually said.
fn warn_bad_lines(path: &Path, bad: &BadLines) {
    if bad.is_empty() {
        return;
    }
    tracing::warn!(
        path = %path.display(),
        lines = bad.count,
        first_line = bad.first_line.unwrap_or(0),
        last_line = bad.last_line.unwrap_or(0),
        tail_only = bad.tail_only,
        error = bad.first_error.as_deref().unwrap_or(""),
        "unparseable JSONL lines — events from them are missing from this analysis"
    );
}

/// Scan a recording for the listing table without retaining its events.
pub fn scan_session(path: &Path) -> Result<SessionIndexEntry, LoadError> {
    let (mut reader, _) = open(path)?;
    let mut line = String::with_capacity(64 * 1024);
    let (config, mut line_no, _) = read_header(path, &mut reader, &mut line)?;

    let mut events = 0u64;
    let mut drops = 0u64;
    let mut bad_lines = BadLines::default();
    let mut last_good_line = line_no;
    let mut first_qpc: Option<u64> = None;
    let mut last_qpc: Option<u64> = None;
    let mut games: BTreeMap<String, usize> = BTreeMap::new();

    loop {
        let n = next_line(path, &mut reader, &mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with(MARKER_TAG) || line.starts_with(SESSION_TAG) {
            last_good_line = line_no;
            continue;
        }
        match serde_json::from_str::<BatchRef>(&line) {
            Ok(b) => {
                last_good_line = line_no;
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
            Err(e) => bad_lines.record(line_no, &e),
        }
    }
    bad_lines.seal(last_good_line);

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
        losses: Vec::new(),
        exit: None,
    }
    .with_sidecar(path))
}

fn timestamp(value: std::io::Result<SystemTime>) -> Option<Timestamp> {
    let d = value.ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(Timestamp {
        secs: d.as_secs(),
        nanos: d.subsec_nanos(),
    })
}

fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, Option<Timestamp>, Option<Timestamp>) {
    (
        metadata.len(),
        timestamp(metadata.modified()),
        timestamp(metadata.created()),
    )
}

/// A cheap but change-sensitive recording signature. Sampling three locations
/// keeps a cache hit O(1) in recording size while guarding against same-size
/// replacement files whose timestamps were rounded by the filesystem.
///
/// `Ok(None)` means the file changed *while* it was being sampled (capture is
/// still appending): the current read may still be useful, but nothing about
/// it may be cached.
pub fn file_signature(path: &Path) -> Result<Option<FileSignature>, LoadError> {
    let mut file = File::open(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let before = file.metadata().map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let (len, modified, created) = metadata_identity(&before);

    let sample_len = len.min(SIGNATURE_SAMPLE_BYTES);
    let middle = len
        .saturating_div(2)
        .saturating_sub(sample_len.saturating_div(2));
    let tail = len.saturating_sub(sample_len);
    let mut offsets = [0, middle, tail];
    offsets.sort_unstable();

    let mut hash = 0xcbf29ce484222325u64;
    let mut previous = None;
    let mut buf = vec![0u8; sample_len as usize];
    for offset in offsets {
        if previous == Some(offset) {
            continue;
        }
        previous = Some(offset);
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(&mut buf))
            .map_err(|source| LoadError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        for byte in offset.to_le_bytes().into_iter().chain(buf.iter().copied()) {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }

    // Never cache a view taken while capture was extending or replacing the
    // file. The current scan may still be useful, but the next invocation must
    // inspect it again.
    let after = file.metadata().map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata_identity(&after) != (len, modified, created) {
        return Ok(None);
    }

    Ok(Some(FileSignature {
        len,
        modified,
        created,
        sample_hash: hash,
    }))
}

fn read_index_cache(dir: &Path) -> HashMap<PathBuf, CachedSession> {
    let path = dir.join(INDEX_CACHE_FILE);
    let Ok(file) = File::open(path) else {
        return HashMap::new();
    };
    let Ok(cache) = serde_json::from_reader::<_, SessionIndexCache>(BufReader::new(file)) else {
        return HashMap::new();
    };
    if cache.version != INDEX_CACHE_VERSION {
        return HashMap::new();
    }
    cache
        .files
        .into_iter()
        .map(|cached| (cached.file_name.clone(), cached))
        .collect()
}

/// The cache is only an acceleration structure, so failure to update it must
/// never turn a successful listing into an error. Write and flush a uniquely
/// named sibling first, then atomically replace the published cache so a crash
/// or concurrent `list` cannot expose half a JSON document.
fn write_index_cache(dir: &Path, mut files: Vec<CachedSession>) {
    files.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    let cache = SessionIndexCache {
        version: INDEX_CACHE_VERSION,
        files,
    };
    let path = dir.join(INDEX_CACHE_FILE);
    let seq = CACHE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        ".telemouse-analyze-index-{}.{}.tmp",
        std::process::id(),
        seq
    ));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|file| {
            let mut writer = std::io::BufWriter::new(file);
            serde_json::to_writer(&mut writer, &cache).map_err(std::io::Error::other)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            fs::rename(&temp, &path)
        });
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        tracing::debug!(path = %path.display(), %error, "could not update recording index cache");
    }
}

/// Every `*.jsonl` in `dir`, scanned and sorted by start time (oldest first).
/// Unreadable files are skipped with a warning rather than failing the listing.
pub fn scan_dir(dir: &Path) -> Result<Vec<SessionIndexEntry>, LoadError> {
    let rd = std::fs::read_dir(dir).map_err(|source| LoadError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let cached = read_index_cache(dir);
    let cached_len = cached.len();
    let mut next_cache = Vec::new();
    let mut out = Vec::new();
    let mut cache_changed = false;
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(file_name) = p.file_name().map(PathBuf::from) else {
            continue;
        };
        // A full signature on both sides of a cache-miss scan prevents a
        // same-size replacement with a coarse/restored timestamp from being
        // published under metadata parsed from the previous file.
        let signature_before = file_signature(&p).ok().flatten();
        if let (Some(signature), Some(hit)) = (signature_before.as_ref(), cached.get(&file_name))
            && &hit.signature == signature
        {
            let mut listed = hit.entry.clone();
            // Preserve the caller's spelling of `dir` rather than leaking the
            // path used by whichever invocation originally populated cache.
            listed.path = p.clone();
            // The sidecar is written after the recording's last flush, so it
            // can appear without the recording changing: always re-read it
            // (one small file) rather than trusting the cached copy.
            listed = listed.with_sidecar(&p);
            out.push(listed.clone());
            next_cache.push(CachedSession {
                file_name,
                signature: signature.clone(),
                entry: listed,
            });
            continue;
        }

        cache_changed = true;
        match scan_session(&p) {
            Ok(scanned) => {
                // Check again after the full scan. If capture appended during
                // it, return what we observed but do not persist a stale view.
                if let (Some(before), Some(after)) =
                    (signature_before, file_signature(&p).ok().flatten())
                    && before == after
                {
                    next_cache.push(CachedSession {
                        file_name,
                        signature: after,
                        entry: scanned.clone(),
                    });
                }
                out.push(scanned);
            }
            Err(e) => {
                tracing::warn!(
                    path = %p.display(),
                    error = %error_chain(&e),
                    "skipping unreadable recording"
                )
            }
        }
    }
    if cache_changed || next_cache.len() != cached_len {
        write_index_cache(dir, next_cache);
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
        assert!(s.bad_lines.is_empty());
        assert!(s.bad_lines.tail_only, "nothing bad is vacuously tail-only");
        assert_eq!(s.bad_lines.flag(), "-");
        assert!(s.bytes > 0);
        assert!(s.load_ms >= 0.0);
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
        assert_eq!(s.bad_lines.count, 1);
        assert_eq!(s.bad_lines.first_line, Some(3));
        assert_eq!(s.bad_lines.last_line, Some(3));
        assert!(s.bad_lines.tail_only, "a killed agent costs the last line");
        assert!(s.bad_lines.first_error.is_some());
        assert_eq!(s.bad_lines.flag(), "1");
    }

    /// Corruption in the *body* of a recording is a different failure from a
    /// truncated tail: events are missing from the middle of the timeline.
    #[test]
    fn interior_corruption_is_distinguished_from_a_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let good = |seq| {
            batch_env(
                &cfg,
                seq,
                Some("cs2.exe"),
                0,
                vec![ev(cfg.anchor.qpc + seq * FIXTURE_FREQ / 1000, 1, 1, 0)],
            )
            .to_json()
            .unwrap()
        };
        let lines = vec![
            Envelope::Session(cfg.clone()).to_json().unwrap(),
            good(0),
            r#"{"type":"batch","session_id":"s-t"#.to_string(), // line 3
            good(2),                                            // line 4: a good line *after* it
            r#"{"type":"batch","truncated"#.to_string(),        // line 5
        ];
        let path = write_lines(dir.path(), "s-test.jsonl", &lines);

        let s = load_session(&path).unwrap();
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.bad_lines.count, 2);
        assert_eq!(s.bad_lines.first_line, Some(3));
        assert_eq!(s.bad_lines.last_line, Some(5));
        assert!(!s.bad_lines.tail_only, "line 4 parsed after line 3 did not");
        assert_eq!(s.bad_lines.flag(), "2!");

        // The listing scan agrees with the full load.
        let scanned = scan_session(&path).unwrap();
        assert_eq!(scanned.bad_lines, s.bad_lines);
    }

    /// A long parse error is capped before it reaches a warning, the report
    /// JSON and the listing cache.
    #[test]
    fn the_kept_parse_error_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        // A type mismatch, so serde's message quotes the whole 4 KB value.
        let junk = format!(
            "{{\"type\":\"batch\",\"seq_no\":\"{}\",\"ts_anchor_us\":0,\"events\":[]}}",
            "z".repeat(4096)
        );
        let path = write_lines(
            dir.path(),
            "s-test.jsonl",
            &[Envelope::Session(cfg).to_json().unwrap(), junk],
        );
        let s = load_session(&path).unwrap();
        let error = s.bad_lines.first_error.unwrap();
        assert!(
            error.chars().count() <= MAX_BAD_LINE_ERROR + 1,
            "{} chars",
            error.chars().count()
        );
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
    fn the_metadata_sidecar_puts_sink_losses_in_the_listing() {
        use std::collections::BTreeMap;
        use telemouse_core::recordings::{SessionMeta, SinkMeta};

        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let path = write_lines(
            dir.path(),
            "s-test.jsonl",
            &[Envelope::Session(cfg.clone()).to_json().unwrap()],
        );
        // No sidecar yet: nothing to say.
        let first = scan_dir(dir.path()).unwrap();
        assert!(first[0].losses.is_empty());
        assert_eq!(first[0].exit, None);
        assert_eq!(first[0].losses_text(), "-");

        // The agent stops and writes the sidecar; the recording itself is
        // unchanged, so the cached entry is reused — and must still pick
        // the sidecar up.
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "kafka".to_string(),
            SinkMeta {
                errors: 1,
                dropped: 397,
                abandoned: 3,
            },
        );
        sinks.insert("jsonl".to_string(), SinkMeta::default());
        let meta = SessionMeta {
            session_id: "s-test".into(),
            exit: "interrupt".into(),
            sinks,
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("s-test.meta.json"),
            meta.to_json_pretty().unwrap(),
        )
        .unwrap();
        let second = scan_dir(dir.path()).unwrap();
        assert_eq!(second[0].losses, vec![("kafka".to_string(), 400)]);
        assert_eq!(second[0].exit.as_deref(), Some("interrupt"));
        assert_eq!(second[0].losses_text(), "kafka=400");
        assert_eq!(read_sidecar(&path).unwrap().session_id, "s-test");
        // The sidecar is never mistaken for a recording.
        assert_eq!(second.len(), 1);
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
    fn scan_dir_cache_invalidates_when_a_recording_grows() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let first = batch_env(
            &cfg,
            0,
            Some("cs2.exe"),
            0,
            vec![ev(cfg.anchor.qpc, 1, 0, 0)],
        )
        .to_json()
        .unwrap();
        let path = write_lines(
            dir.path(),
            "s-test.jsonl",
            &[Envelope::Session(cfg.clone()).to_json().unwrap(), first],
        );

        let initial = scan_dir(dir.path()).unwrap();
        assert_eq!(initial[0].events, 1);
        assert!(dir.path().join(INDEX_CACHE_FILE).is_file());

        let second = batch_env(
            &cfg,
            1,
            Some("valorant.exe"),
            2,
            vec![ev(cfg.anchor.qpc + FIXTURE_FREQ, 2, 0, 0)],
        )
        .to_json()
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{second}").unwrap();
        file.flush().unwrap();

        let changed = scan_dir(dir.path()).unwrap();
        assert_eq!(changed[0].events, 2);
        assert_eq!(changed[0].drops, 2);
        assert_eq!(changed[0].games, vec!["cs2.exe", "valorant.exe"]);

        // The replacement write succeeded and contains the refreshed entry,
        // rather than merely returning a correct uncached scan once.
        let cache: SessionIndexCache =
            serde_json::from_reader(File::open(dir.path().join(INDEX_CACHE_FILE)).unwrap())
                .unwrap();
        assert_eq!(cache.files.len(), 1);
        assert_eq!(cache.files[0].entry.events, 2);
        assert_eq!(cache.files[0].entry.drops, 2);
        assert_eq!(
            cache.files[0].signature.len,
            std::fs::metadata(&path).unwrap().len()
        );

        let warm = scan_dir(dir.path()).unwrap();
        assert_eq!(warm, changed);
    }

    #[test]
    fn signature_sampling_detects_a_same_size_tail_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let header = Envelope::Session(cfg.clone()).to_json().unwrap();
        let batch = |dx| {
            batch_env(&cfg, 0, None, 0, vec![ev(cfg.anchor.qpc, dx, 0, 0)])
                .to_json()
                .unwrap()
        };
        let path = write_lines(dir.path(), "s-test.jsonl", &[header.clone(), batch(1)]);
        let before = file_signature(&path).unwrap().unwrap();

        write_lines(dir.path(), "s-test.jsonl", &[header, batch(2)]);
        let after = file_signature(&path).unwrap().unwrap();

        assert_eq!(before.len, after.len);
        assert_ne!(before.sample_hash, after.sample_hash);
    }

    #[test]
    fn scan_dir_invalidates_a_same_size_recording_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        let header = Envelope::Session(cfg.clone()).to_json().unwrap();
        let batch = |game| {
            batch_env(&cfg, 0, Some(game), 0, vec![ev(cfg.anchor.qpc, 1, 0, 0)])
                .to_json()
                .unwrap()
        };
        let path = write_lines(
            dir.path(),
            "s-test.jsonl",
            &[header.clone(), batch("game-a.exe")],
        );
        let initial = scan_dir(dir.path()).unwrap();
        assert_eq!(initial[0].games, vec!["game-a.exe"]);
        let signature_before = file_signature(&path).unwrap().unwrap();

        write_lines(dir.path(), "s-test.jsonl", &[header, batch("game-b.exe")]);
        let signature_after = file_signature(&path).unwrap().unwrap();
        assert_eq!(signature_before.len, signature_after.len);
        assert_ne!(signature_before.sample_hash, signature_after.sample_hash);

        let replaced = scan_dir(dir.path()).unwrap();
        assert_eq!(replaced[0].games, vec!["game-b.exe"]);
        let cache: SessionIndexCache =
            serde_json::from_reader(File::open(dir.path().join(INDEX_CACHE_FILE)).unwrap())
                .unwrap();
        assert_eq!(cache.files[0].signature, signature_after);
        assert_eq!(cache.files[0].entry.games, vec!["game-b.exe"]);
    }

    #[test]
    fn a_corrupt_index_cache_is_disposable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = session_cfg();
        write_lines(
            dir.path(),
            "s-test.jsonl",
            &[Envelope::Session(cfg).to_json().unwrap()],
        );
        std::fs::write(dir.path().join(INDEX_CACHE_FILE), "not json").unwrap();

        let list = scan_dir(dir.path()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].session_id, "s-test");
        let rebuilt: SessionIndexCache =
            serde_json::from_reader(File::open(dir.path().join(INDEX_CACHE_FILE)).unwrap())
                .unwrap();
        assert_eq!(rebuilt.version, INDEX_CACHE_VERSION);
        assert_eq!(rebuilt.files.len(), 1);
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
                batch_env(&cfg, 0, Some("cs2.exe"), 0, vec![e])
                    .to_json()
                    .unwrap(),
                // A pre-device-tracking line, written by hand.
                format!(
                    r#"{{"type":"batch","session_id":"s-test","seq_no":1,"ts_anchor_us":{},"pointer_locked":true,"screen_w":1,"screen_h":1,"drops_since_last":0,"events":[{{"ts_qpc":{},"dx":1,"dy":1,"buttons":0,"wheel":0}}]}}"#,
                    cfg.started_utc_us,
                    cfg.anchor.qpc + 1000
                ),
            ],
        );
        let s = load_session(&path).unwrap();
        assert!(s.bad_lines.is_empty());
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.events[0].wheel_h, -120);
        assert_eq!(s.events[0].device_ix, 2);
        assert_eq!(s.events[1].device_ix, 0);
        assert_eq!(s.device_indices(), vec![0, 2]);
    }
}
