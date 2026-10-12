//! The chunk-parallel read, and the stitching both paths end in.
//!
//! Lines are independent, so anything past [`PARALLEL_MIN_BYTES`] is split at
//! newline boundaries into one byte range per core and parsed on worker
//! threads ([`load_parallel`]); the ranges are stitched back together in file
//! order by [`assemble`]. Both paths run the same `parse_body` loop, so the
//! events, batches, markers, drop counters, interned names and `BadLines`
//! (with the file's own line numbers) come out identical either way —
//! `the_parallel_loader_matches_the_sequential_one` asserts exactly that.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use telemouse_core::{Marker, RawEvent, SessionConfig};

use super::parse::{Body, LoadProgress, parse_body, read_header};
use super::{BadLines, BatchMeta, LoadError, LoadedSession, open, read_sidecar, warn_bad_lines};

/// Rough serialized size of one motion event, used to size the event vector
/// from the file length. Measured over real recordings: a motion-only event
/// serializes to `{"ts_qpc":51234567890,"dx":-3,"dy":2},` ≈ 45 B, and batch
/// envelopes add their own overhead; 71 B/event lands within ~10 % on captures
/// with the usual mix of clicks and idle gaps. Over-reserving costs one
/// oversized allocation, under-reserving costs a doubling — both are cheap
/// compared to the growth series this replaces.
pub(super) const BYTES_PER_EVENT: u64 = 71;

/// Smallest recording the chunk-parallel loader is used for. Below it the
/// boundary seeks, the extra file handles and the thread spawns cost more than
/// the parse saves — a 4 MB recording loads in ~10 ms single-threaded.
pub(super) const PARALLEL_MIN_BYTES: u64 = 8 * 1024 * 1024;
/// Smallest byte range worth giving a worker: the number of workers is capped
/// so no chunk is smaller than this, however many cores the box has.
const MIN_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// Workers never exceed this, whatever `available_parallelism` reports. Past
/// ~16 the loader is memory-bandwidth-bound and each extra thread only adds a
/// file handle and a 1 MB read buffer.
pub(super) const MAX_LOAD_WORKERS: usize = 16;

/// Stitch the parsed chunks back into one recording, in file order.
///
/// Three things have to be put back together that a per-chunk parse cannot get
/// right on its own: line numbers (each chunk's are relative to its own
/// start), the interned process names (each chunk has its own interner, so
/// `"cs2.exe"` in chunk 3 is a different `Arc` from chunk 0's), and the
/// [`BadLines`] summary, whose `tail_only` flag is a statement about the file
/// rather than about any one chunk.
///
/// Chunks are consumed one at a time and dropped as they are appended, so the
/// concatenation does not hold two copies of the event vector.
pub(super) fn assemble(
    path: &Path,
    config: SessionConfig,
    size: u64,
    header_lines: u64,
    bodies: Vec<Body>,
    progress: LoadProgress,
) -> LoadedSession {
    let total_events: usize = bodies.iter().map(|b| b.events.len()).sum();
    let total_batches: usize = bodies.iter().map(|b| b.batches.len()).sum();
    let total_markers: usize = bodies.iter().map(|b| b.markers.len()).sum();

    // One `Arc` per distinct name across the whole file, so the per-process
    // tally and the pointer identity the sequential loader guarantees survive
    // the split. The earliest chunk's copy wins, which leaves a single-chunk
    // (sequential) load with nothing to do.
    let mut canonical: HashMap<Arc<str>, Arc<str>> = HashMap::new();
    for b in &bodies {
        for name in b.games.0.values() {
            canonical
                .entry(Arc::clone(name))
                .or_insert_with(|| Arc::clone(name));
        }
    }

    let mut events: Vec<RawEvent> = Vec::with_capacity(total_events);
    let mut markers: Vec<Marker> = Vec::with_capacity(total_markers);
    let mut batches: Vec<BatchMeta> = Vec::with_capacity(total_batches);
    let mut total_drops = 0u64;
    let mut total_abs_frames = 0u64;
    let mut bad_lines = BadLines::default();
    let mut last_good_line = header_lines;
    let mut base = header_lines;

    for mut b in bodies {
        // A handful of names per chunk at most, so this is a couple of passes
        // over the batch metadata and nothing per event.
        let mut locals: Vec<Arc<str>> = Vec::new();
        for name in b.games.0.values() {
            if !locals.iter().any(|l| Arc::ptr_eq(l, name)) {
                locals.push(Arc::clone(name));
            }
        }
        for local in &locals {
            let Some(canon) = canonical.get(local).map(Arc::clone) else {
                continue;
            };
            if Arc::ptr_eq(local, &canon) {
                continue;
            }
            for m in &mut b.batches {
                if m.game.as_ref().is_some_and(|g| Arc::ptr_eq(g, local)) {
                    m.game = Some(Arc::clone(&canon));
                }
            }
        }

        if b.last_good > 0 {
            last_good_line = base + b.last_good;
        }
        bad_lines.count += b.bad.count;
        if let Some(first) = b.bad.first_line {
            bad_lines.first_line.get_or_insert(base + first);
        }
        if let Some(last) = b.bad.last_line {
            bad_lines.last_line = Some(base + last);
        }
        if bad_lines.first_error.is_none() {
            bad_lines.first_error = b.bad.first_error.take();
        }
        total_drops += b.total_drops;
        total_abs_frames += b.total_abs_frames;
        base += b.lines;

        batches.append(&mut b.batches);
        markers.append(&mut b.markers);
        events.append(&mut b.events);
    }

    bad_lines.seal(last_good_line);
    warn_bad_lines(path, &bad_lines);
    let load_ms = progress.finish(path, events.len());

    LoadedSession {
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
        // Read once, here: the quality report and `report --summary` both
        // take it off the session rather than going back to the disk.
        sidecar: read_sidecar(path),
    }
}

/// How many workers `load_session` gives a recording of `size` bytes.
/// One means the sequential path.
pub(super) fn load_workers(size: u64) -> usize {
    if size < PARALLEL_MIN_BYTES {
        return 1;
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let by_size = (size / MIN_CHUNK_BYTES).max(1);
    cpus.min(by_size as usize).clamp(1, MAX_LOAD_WORKERS)
}

/// The one-thread read: header, then every body line in order.
pub(super) fn load_sequential(path: &Path) -> Result<LoadedSession, LoadError> {
    let (mut reader, size) = open(path)?;
    let mut progress = LoadProgress::start(path, size);
    let mut line = String::with_capacity(64 * 1024);
    let (config, header_lines, header_bytes) = read_header(path, &mut reader, &mut line)?;
    drop(line);
    progress.advance(header_bytes as usize, 0);

    let body = parse_body(
        path,
        &mut reader,
        (size / BYTES_PER_EVENT) as usize,
        Some(&mut progress),
    )?;
    Ok(assemble(
        path,
        config,
        size,
        header_lines,
        vec![body],
        progress,
    ))
}

/// The chunk-parallel read: `workers` byte ranges, each starting on a line
/// boundary, parsed on their own threads and stitched in order.
///
/// Progress is reported once at the end rather than at byte milestones: no
/// thread sees the file go by in order, and a load this path is taken for
/// finishes in a fraction of a second anyway.
pub(super) fn load_parallel(path: &Path, workers: usize) -> Result<LoadedSession, LoadError> {
    let (mut reader, size) = open(path)?;
    let mut progress = LoadProgress::start(path, size);
    let mut line = String::with_capacity(64 * 1024);
    let (config, header_lines, header_bytes) = read_header(path, &mut reader, &mut line)?;
    drop(reader);
    drop(line);

    let bounds = chunk_bounds(path, header_bytes, size, workers)?;
    let bodies = std::thread::scope(|scope| {
        let handles: Vec<_> = bounds
            .windows(2)
            .map(|w| {
                let (from, to) = (w[0], w[1]);
                scope.spawn(move || parse_range(path, from, to))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect::<Result<Vec<Body>, LoadError>>()
    })?;

    progress.complete(size);
    Ok(assemble(path, config, size, header_lines, bodies, progress))
}

/// Line-aligned split points over `[start, size)`, `workers + 1` of them.
///
/// Each interior boundary is pushed forward to just past the next newline, so
/// the line straddling a nominal split belongs to the chunk before it and no
/// line is parsed twice or dropped. Boundaries are kept non-decreasing, which
/// is what makes an empty chunk (a file with fewer lines than workers, or one
/// enormous line) harmless rather than a panic.
fn chunk_bounds(path: &Path, start: u64, size: u64, workers: usize) -> Result<Vec<u64>, LoadError> {
    let mut file = File::open(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let span = size.saturating_sub(start);
    let mut bounds = Vec::with_capacity(workers + 1);
    bounds.push(start);
    let mut previous = start;
    for i in 1..workers as u64 {
        let nominal = start + span * i / workers as u64;
        let at = next_line_start(path, &mut file, nominal, size)?.max(previous);
        bounds.push(at);
        previous = at;
    }
    bounds.push(size.max(start));
    Ok(bounds)
}

/// The offset just past the first newline at or after `from`, or `size` when
/// the rest of the file has none.
fn next_line_start(path: &Path, file: &mut File, from: u64, size: u64) -> Result<u64, LoadError> {
    if from >= size {
        return Ok(size);
    }
    let io = |source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    };
    file.seek(SeekFrom::Start(from)).map_err(io)?;
    let mut buf = [0u8; 64 * 1024];
    let mut at = from;
    loop {
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            return Ok(size);
        }
        if let Some(i) = buf[..n].iter().position(|&b| b == b'\n') {
            return Ok(at + i as u64 + 1);
        }
        at += n as u64;
    }
}

/// Parse the body lines in `[from, to)`. Both ends are line boundaries, so this
/// is the same loop the sequential path runs, over a slice of the same file.
fn parse_range(path: &Path, from: u64, to: u64) -> Result<Body, LoadError> {
    if from >= to {
        return Ok(Body::default());
    }
    let io = |source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(io)?;
    file.seek(SeekFrom::Start(from)).map_err(io)?;
    let span = to - from;
    let mut reader = BufReader::with_capacity(1 << 20, file.take(span));
    parse_body(path, &mut reader, (span / BYTES_PER_EVENT) as usize, None)
}
