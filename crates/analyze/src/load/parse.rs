//! The line-level reader: the session header, the one body-line classifier
//! both the full load and the listing scan share, and the body loop.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::{Deserializer, SeqAccess, Visitor};
use telemouse_core::{Batch, Envelope, Marker, RawEvent, SessionConfig};

use super::{BadLines, BatchMeta, LoadError};

/// Progress is logged at whichever comes first: this much wall time, or
/// [`PROGRESS_BYTE_SHARE`] of the file.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_BYTE_SHARE: u64 = 10;

/// The batch fields the analyzer actually reads, borrowed straight out of the
/// line buffer. `session_id`, `screen_w/h` and the cursor sample are ignored
/// rather than allocated; serde skips unknown fields by default.
///
/// `E` is what the `events` array becomes: the events themselves for a full
/// load, an [`EventTally`] for the listing scan. Both deserialize every event
/// with the same `RawEvent` rules, so a line is good or bad identically for
/// either reader.
#[derive(Debug, Deserialize)]
pub(super) struct BatchRef<'a, E> {
    pub seq_no: u64,
    pub ts_anchor_us: i64,
    #[serde(borrow, default)]
    pub game: Option<Cow<'a, str>>,
    #[serde(default)]
    pub pointer_locked: bool,
    #[serde(default)]
    pub drops_since_last: u32,
    #[serde(default)]
    pub abs_frames_since_last: u32,
    pub events: E,
}

impl<E: From<Vec<RawEvent>>> BatchRef<'_, E> {
    /// The general-path batch, already owned.
    fn from_owned(b: Batch) -> Self {
        Self {
            seq_no: b.seq_no,
            ts_anchor_us: b.ts_anchor_us,
            game: b.game.map(Cow::Owned),
            pointer_locked: b.pointer_locked,
            drops_since_last: b.drops_since_last,
            abs_frames_since_last: b.abs_frames_since_last,
            events: E::from(b.events),
        }
    }
}

/// A batch's events, counted rather than kept: what the listing scan needs.
/// Every element is still deserialized as a full [`RawEvent`] (a `Copy` value,
/// no allocation), so a malformed event fails the line here exactly as it does
/// in the full load.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct EventTally {
    pub count: usize,
    pub first_qpc: Option<u64>,
    pub last_qpc: Option<u64>,
}

impl EventTally {
    fn push(&mut self, e: &RawEvent) {
        self.count += 1;
        self.first_qpc.get_or_insert(e.ts_qpc);
        self.last_qpc = Some(e.ts_qpc);
    }
}

impl From<Vec<RawEvent>> for EventTally {
    fn from(events: Vec<RawEvent>) -> Self {
        let mut t = Self::default();
        for e in &events {
            t.push(e);
        }
        t
    }
}

impl<'de> Deserialize<'de> for EventTally {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Tally;
        impl<'de> Visitor<'de> for Tally {
            type Value = EventTally;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of events")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<EventTally, A::Error> {
                let mut t = EventTally::default();
                while let Some(e) = seq.next_element::<RawEvent>()? {
                    t.push(&e);
                }
                Ok(t)
            }
        }
        d.deserialize_seq(Tally)
    }
}

/// The envelope prefixes a recording can hold, matched before invoking serde
/// so the hot path never pays for an internally-tagged enum's content
/// buffering. (`heartbeat` is not one of them — it is a live-path signal the
/// agent sends to the viz alone — but it is listed so a file that somehow
/// picked one up is skipped rather than counted as corruption.)
const BATCH_TAG: &str = "{\"type\":\"batch\"";
const MARKER_TAG: &str = "{\"type\":\"marker\"";
const SESSION_TAG: &str = "{\"type\":\"session\"";
const HEARTBEAT_TAG: &str = "{\"type\":\"heartbeat\"";

/// What one body line is. The single definition of a good and a bad line:
/// the full load and the listing scan both read every line through
/// [`classify`], so `list`'s `BAD` column and the report agree.
pub(super) enum BodyLine<'a, E> {
    Blank,
    Batch(BatchRef<'a, E>),
    Marker(Marker),
    /// Well-formed but nothing to analyse: a second session envelope
    /// (topic compaction replay, an appended session) or a heartbeat teed
    /// off the live stream.
    Ignored,
    Bad(serde_json::Error),
}

/// Classify one body line: the tag fast paths first, then the general
/// envelope parser for anything whose tag is not where it is expected, and
/// only then is a line written off as corrupt.
pub(super) fn classify<'a, E>(line: &'a str) -> BodyLine<'a, E>
where
    E: Deserialize<'a> + From<Vec<RawEvent>>,
{
    if line.trim().is_empty() {
        return BodyLine::Blank;
    }
    if line.starts_with(BATCH_TAG) {
        return match serde_json::from_str::<BatchRef<'a, E>>(line) {
            Ok(b) => BodyLine::Batch(b),
            Err(e) => BodyLine::Bad(e),
        };
    }
    if line.starts_with(MARKER_TAG) {
        return match serde_json::from_str::<Marker>(line) {
            Ok(m) => BodyLine::Marker(m),
            Err(e) => BodyLine::Bad(e),
        };
    }
    if line.starts_with(SESSION_TAG) || line.starts_with(HEARTBEAT_TAG) {
        return BodyLine::Ignored;
    }
    match Envelope::from_json(line) {
        Ok(Envelope::Batch(b)) => BodyLine::Batch(BatchRef::from_owned(b)),
        Ok(Envelope::Marker(m)) => BodyLine::Marker(m),
        Ok(Envelope::Session(_) | Envelope::Heartbeat(_)) => BodyLine::Ignored,
        Err(e) => BodyLine::Bad(e),
    }
}

/// Interns process names so a long session holds one copy of each.
///
/// Every interned name is lower-cased, so the per-process tally can be keyed
/// by the `Arc` directly instead of re-lowercasing a name per batch. Both
/// spellings map to the same `Arc`, so a recording that switches between
/// `CS2.exe` and `cs2.exe` still costs one allocation and one tally entry.
#[derive(Default)]
pub(super) struct GameInterner(pub HashMap<String, Arc<str>>);

impl GameInterner {
    pub fn get(&mut self, s: &str) -> Arc<str> {
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
pub(super) struct LoadProgress {
    total: u64,
    read: u64,
    started: Instant,
    last_log: Instant,
    next_bytes: u64,
    step: u64,
}

impl LoadProgress {
    pub fn start(path: &Path, total: u64) -> Self {
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
    pub fn advance(&mut self, n: usize, events: usize) {
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

    /// Mark the whole file as read. The parallel path has no single place that
    /// sees every line go by, so it reports the read in one piece at the end
    /// instead of at the byte milestones.
    pub fn complete(&mut self, bytes: u64) {
        self.read = bytes;
    }

    /// Milliseconds the read took, logged with its throughput.
    pub fn finish(self, path: &Path, events: usize) -> f64 {
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

/// Read one line into `buf`, stripping the trailing newline. Returns the bytes
/// consumed *including* the line terminator, so the caller can report progress
/// against the file length; `Ok(0)` at EOF.
pub(super) fn next_line<R: BufRead>(
    path: &Path,
    r: &mut R,
    buf: &mut String,
) -> Result<usize, LoadError> {
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
///
/// A header that parses but whose numbers cannot be trusted (a zero QPC
/// frequency, a non-positive CPI, top-level copies that disagree with the
/// anchor; see `SessionConfig::validate`) is refused here, so every reader —
/// the full load, the listing scan, `trend` — skips that recording with the
/// same message instead of reporting nonsense.
pub(super) fn read_header<R: BufRead>(
    path: &Path,
    r: &mut R,
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
            Ok(Envelope::Session(cfg)) => match cfg.validate() {
                Ok(()) => Ok((cfg, lines, bytes)),
                Err(source) => Err(LoadError::BadSessionHeader {
                    path: path.to_path_buf(),
                    source,
                }),
            },
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
        Envelope::Heartbeat(_) => "heartbeat",
    }
}

/// One stretch of body lines, parsed. A sequential load produces exactly one
/// of these; a parallel load produces one per byte range.
///
/// Line numbers in here are 1-based *within the stretch*: a worker cannot know
/// how many lines precede its range without reading them, so
/// [`super::parallel::assemble`] adds the offset once the chunk line counts
/// are known.
#[derive(Default)]
pub(super) struct Body {
    pub events: Vec<RawEvent>,
    pub markers: Vec<Marker>,
    pub batches: Vec<BatchMeta>,
    pub total_drops: u64,
    pub total_abs_frames: u64,
    /// Lines consumed, blank ones included.
    pub lines: u64,
    /// The highest line that parsed, or 0 when none did.
    pub last_good: u64,
    pub bad: BadLines,
    pub games: GameInterner,
}

impl Body {
    /// Flatten one batch into the body: its bookkeeping into `batches`, its
    /// events onto the end of `events`.
    fn push_batch(&mut self, mut b: BatchRef<'_, Vec<RawEvent>>) {
        self.total_drops += b.drops_since_last as u64;
        self.total_abs_frames += b.abs_frames_since_last as u64;
        self.batches.push(BatchMeta {
            seq_no: b.seq_no,
            ts_anchor_us: b.ts_anchor_us,
            game: b.game.as_deref().map(|g| self.games.get(g)),
            pointer_locked: b.pointer_locked,
            drops_since_last: b.drops_since_last,
            abs_frames_since_last: b.abs_frames_since_last,
            first_event_qpc: b.events.first().map(|e| e.ts_qpc),
            event_count: b.events.len(),
        });
        self.events.append(&mut b.events);
    }
}

/// The loader's one line loop, over whatever `r` yields.
///
/// Both the sequential and the parallel path go through here, which is what
/// makes the two byte-identical; what counts as a good line and what is
/// recorded as a bad one is [`classify`]'s call, shared with the scan.
pub(super) fn parse_body<R: BufRead>(
    path: &Path,
    r: &mut R,
    reserve_events: usize,
    mut progress: Option<&mut LoadProgress>,
) -> Result<Body, LoadError> {
    let mut body = Body {
        events: Vec::with_capacity(reserve_events),
        ..Default::default()
    };
    let mut line = String::with_capacity(64 * 1024);

    loop {
        let n = next_line(path, r, &mut line)?;
        if n == 0 {
            break;
        }
        body.lines += 1;
        let line_no = body.lines;
        if let Some(p) = progress.as_deref_mut() {
            p.advance(n, body.events.len());
        }
        match classify::<Vec<RawEvent>>(&line) {
            BodyLine::Blank => {}
            BodyLine::Batch(b) => {
                body.last_good = line_no;
                body.push_batch(b);
            }
            BodyLine::Marker(m) => {
                body.last_good = line_no;
                body.markers.push(m);
            }
            BodyLine::Ignored => body.last_good = line_no,
            BodyLine::Bad(e) => body.bad.record(line_no, &e),
        }
    }
    Ok(body)
}
