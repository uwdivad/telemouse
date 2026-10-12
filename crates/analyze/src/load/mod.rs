//! Reading `recordings/<session_id>.jsonl` back into memory.
//!
//! File shape (see `docs/CONVENTIONS.md`): one `telemouse_core::Envelope` per
//! line, the first line always the `session` envelope, then `batch` and
//! `marker` envelopes in capture order.
//!
//! The loader is deliberately forgiving about everything except the leading
//! session envelope: a truncated or corrupt trailing line (the capture agent
//! being killed mid-write) costs one line, not the whole recording, and is
//! surfaced as a data-quality warning instead of an error. The header itself
//! must parse *and* pass `SessionConfig::validate`, because every timestamp
//! and distance is derived from it.
//!
//! At 8 M events a recording is a ~570 MB file, so the read path is written for
//! that scale: the event vector is sized from the file length up front, one
//! `String` is reused for every line, batches deserialize through a
//! borrow-based shadow struct that never allocates for the fields we only
//! read, and process names are interned so a three-hour session holds one
//! `"cs2.exe"` rather than half a million of them.
//!
//! The pieces:
//!
//! * `parse` — the header, the one line classifier the full load and the
//!   listing scan share, and the body loop;
//! * `parallel` — the chunk-parallel read and the stitching both paths end in;
//! * `signature` — [`FileSignature`], a recording's identity for caches;
//! * `index_cache` — the listing's own disposable cache.

mod index_cache;
mod parallel;
mod parse;
mod signature;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use telemouse_core::recordings::{self, SessionMeta};
use telemouse_core::{GameSens, Marker, RawEvent, SessionConfig, SessionError};

use index_cache::{CachedSession, read_index_cache, write_index_cache};
use parse::{BodyLine, EventTally, GameInterner, classify, next_line, read_header};

pub use signature::{FileSignature, Timestamp, file_signature};

/// Longest parse error kept from a corrupt line. A type mismatch quotes the
/// offending value, so a 64 KB field would otherwise land whole in a warning,
/// a JSON report, and the listing cache.
const MAX_BAD_LINE_ERROR: usize = 200;

/// Schema tag on every row of `list --json`.
pub const LIST_SCHEMA: &str = "telemouse-list/1";

/// Why a recording could not be read.
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
    /// The header parsed, but its clock or CPI cannot be trusted to convert a
    /// single timestamp or count (`SessionConfig::validate`).
    #[error("{path}: the session header is unusable")]
    BadSessionHeader { path: PathBuf, source: SessionError },
}

/// `e` plus every source beneath it, joined by `: `.
///
/// `tracing`'s `%` fields print `Display` only, which for a `#[source]`-chained
/// error is just the outer message. Anything that logs a [`LoadError`] (or an
/// [`crate::AnalyzeError`]) rather than returning it goes through here so the
/// cause survives.
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
    /// The capture agent's `<session_id>.meta.json`, read once at load time
    /// (see [`read_sidecar`]). `None` without one.
    pub sidecar: Option<SessionMeta>,
}

/// Events per foreground process, batch-attributed.
///
/// Keyed by the interned `Arc<str>` the loader already holds — names are
/// lower-cased at intern time, so this is a pointer-keyed tally rather than
/// one `to_ascii_lowercase` allocation per batch. [`dominant_game`] breaks
/// ties the same way the old `BTreeMap` version did (highest count, then the
/// name that sorts first).
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
    #[cfg(test)]
    pub(crate) fn dominant_game_share(&self) -> Option<(String, f64)> {
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
    #[cfg(test)]
    pub(crate) fn resolve_aim(&self) -> (Option<String>, GameSens, bool) {
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
        self.exit.as_deref() == Some(recordings::ExitReason::Running.as_str())
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
///
/// Located by the recording-id rule: `<id>.jsonl` → `<id>.meta.json` in the
/// same directory. A recording whose file name is not a session id has no
/// sidecar by definition.
pub fn read_sidecar(recording: &Path) -> Option<SessionMeta> {
    let id = recordings::id_from_file_name(recording.file_name()?.to_str()?)?;
    let path = recording.with_file_name(recordings::meta_file_name(id)?);
    let text = fs::read_to_string(path).ok()?;
    SessionMeta::from_json(&text).ok()
}

fn open(path: &Path) -> Result<(BufReader<File>, u64), LoadError> {
    let file = File::open(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok((BufReader::with_capacity(1 << 20, file), size))
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

/// Load a recording fully into memory.
///
/// Small recordings are read on this thread; anything past
/// `PARALLEL_MIN_BYTES` is split into one byte range per core and parsed on
/// worker threads. The result is the same either way. The metadata sidecar
/// is read here too, once, into [`LoadedSession::sidecar`].
pub fn load_session(path: &Path) -> Result<LoadedSession, LoadError> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match parallel::load_workers(size) {
        0 | 1 => parallel::load_sequential(path),
        workers => parallel::load_parallel(path, workers),
    }
}

/// Scan a recording for the listing table without retaining its events.
///
/// Every line goes through the same classifier as the full load, so the
/// counts and the `BAD` column agree with what `report` will read; the events
/// of each batch are counted as they deserialize rather than collected.
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
    // Lower-cased once per distinct spelling, not once per batch.
    let mut interner = GameInterner::default();
    let mut games: HashMap<Arc<str>, usize> = HashMap::new();

    loop {
        let n = next_line(path, &mut reader, &mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;
        match classify::<EventTally>(&line) {
            BodyLine::Blank => {}
            BodyLine::Batch(b) => {
                last_good_line = line_no;
                drops += b.drops_since_last as u64;
                events += b.events.count as u64;
                if let Some(g) = &b.game {
                    *games.entry(interner.get(g)).or_insert(0) += b.events.count;
                }
                if let Some(q) = b.events.first_qpc {
                    first_qpc.get_or_insert(q);
                }
                if let Some(q) = b.events.last_qpc {
                    last_qpc = Some(q);
                }
            }
            BodyLine::Marker(_) | BodyLine::Ignored => last_good_line = line_no,
            BodyLine::Bad(e) => bad_lines.record(line_no, &e),
        }
    }
    bad_lines.seal(last_good_line);

    let duration_s = match (first_qpc, last_qpc) {
        (Some(a), Some(b)) => config.anchor.ticks_to_us(a, b) as f64 / 1e6,
        _ => 0.0,
    };
    let mut games: Vec<(Arc<str>, usize)> = games.into_iter().collect();
    // Most events first; ties by name, so the order never depends on hashing.
    games.sort_by(|(an, a), (bn, b)| b.cmp(a).then_with(|| an.cmp(bn)));

    Ok(SessionIndexEntry {
        path: path.to_path_buf(),
        session_id: config.session_id,
        started_utc_us: config.started_utc_us,
        duration_s,
        events,
        drops,
        games: games.into_iter().map(|(g, _)| g.to_string()).collect(),
        bad_lines,
        losses: Vec::new(),
        exit: None,
    }
    .with_sidecar(path))
}

/// Every recording in `dir` — `<id>.jsonl` for a valid session id — scanned
/// and sorted by start time (oldest first). Unreadable recordings and ones
/// whose header fails validation are skipped with a warning rather than
/// failing the listing; a `.jsonl` whose name is not a session id is skipped
/// with a debug line, because nothing else in telemouse would serve it.
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
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(recordings::RECORDING_SUFFIX) {
            continue;
        }
        if recordings::id_from_file_name(name).is_none() {
            tracing::debug!(path = %p.display(), "skipping a .jsonl whose name is not a session id");
            continue;
        }
        let file_name = PathBuf::from(name);
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
    out.sort_by(|a, b| {
        a.started_utc_us
            .cmp(&b.started_utc_us)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(out)
}

/// `list --json`: the entries, each tagged with [`LIST_SCHEMA`].
pub fn list_json(entries: &[SessionIndexEntry]) -> serde_json::Result<String> {
    crate::tagged_rows_json(LIST_SCHEMA, entries)
}

/// The `list` terminal table for the recordings in `dir`.
pub fn render_list(entries: &[SessionIndexEntry], dir: &Path) -> String {
    use crate::timefmt::{format_duration, format_utc_us};

    if entries.is_empty() {
        return format!("no recordings in {}\n", dir.display());
    }
    let mut o = format!(
        "{:<24}  {:<28}  {:>12}  {:>12}  {:>8}  {:>5}  {:<16}  {:<12}  GAME\n",
        "SESSION", "STARTED (UTC)", "DURATION", "EVENTS", "DROPS", "BAD", "LOSS", "EXIT"
    );
    for e in entries {
        o.push_str(&format!(
            "{:<24}  {:<28}  {:>12}  {:>12}  {:>8}  {:>5}  {:<16}  {:<12}  {}\n",
            e.session_id,
            format_utc_us(e.started_utc_us),
            format_duration(e.duration_s),
            e.events,
            e.drops,
            e.bad_lines.flag(),
            e.losses_text(),
            e.exit_text(),
            e.games.first().map_or("—", |g| g.as_str()),
        ));
    }
    let total_events: u64 = entries.iter().map(|e| e.events).sum();
    let total_drops: u64 = entries.iter().map(|e| e.drops).sum();
    let lossy = entries.iter().filter(|e| !e.losses.is_empty()).count();
    let unfinished = entries.iter().filter(|e| e.unfinished()).count();
    o.push_str(&format!(
        "\n{} session(s), {} events, {} drops, {} with sink losses, {} unfinished\n\
         BAD = unparseable JSONL lines (a bare count is a cut-off tail; ! means corruption inside the file); \
         LOSS = sink=envelopes not delivered and EXIT = how the run ended, both from the .meta.json sidecar \
         (\"running\" = it never stopped cleanly)\n",
        entries.len(),
        total_events,
        total_drops,
        lossy,
        unfinished
    ));
    o
}
