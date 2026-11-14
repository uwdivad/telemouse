//! Longitudinal analysis — one row per session.
//!
//! The plan's session-level metrics (fatigue, warmup, consistency, sensitivity
//! experiments) are all "the same number, across sessions". That needs the
//! per-session reports to be *cheap to get at*, which is what the JSON cache
//! here is for: a cached report is reused only while its analyzer version and
//! schema, its detector parameters, the recording it describes, and that
//! recording's [`FileSignature`](crate::load::FileSignature) all still match
//! (see [`read_cache`]). The sidecar-derived fields are re-read on every hit,
//! because the capture agent writes its final sidecar after the recording's
//! last flush.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use serde::{Deserialize, Serialize};
use telemouse_core::recordings;

use crate::error::AnalyzeError;
use crate::load::{file_signature, read_sidecar};
use crate::report::slim::{self, Headlines, Stamps};
use crate::report::{ANALYZER_VERSION, Report, ReportSummary, SCHEMA, SessionSummary};
use crate::series::Params;
use crate::timefmt::{format_duration, format_utc_us};

/// Schema tag on every row of `trend --json`.
pub const TREND_SCHEMA: &str = "telemouse-trend/1";

/// Why a session's report was (or was not) recomputed. Logged so a surprising
/// `trend` runtime is explainable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// The cached report was current and used as-is.
    Hit,
    /// No cached report existed.
    Missing,
    /// The cache was written by a different analyzer version.
    VersionChanged,
    /// The cache was computed with different detector parameters.
    ParamsChanged,
    /// The recording has been modified since the cache was written.
    Stale,
    /// The cached report is about a different recording.
    Mismatch,
    /// The cache file existed but could not be read or parsed.
    Unreadable,
}

impl CacheOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheOutcome::Hit => "hit",
            CacheOutcome::Missing => "missing",
            CacheOutcome::VersionChanged => "version-changed",
            CacheOutcome::ParamsChanged => "params-changed",
            CacheOutcome::Stale => "recording-changed",
            CacheOutcome::Mismatch => "different-recording",
            CacheOutcome::Unreadable => "unreadable",
        }
    }
}

/// Where a session's cached report lives inside `dir`.
pub fn cache_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.report.json"))
}

/// Read a cached report if it is still valid for `recording` and `params`.
///
/// Valid means: it parses, its `analyzer_version` and schema match this build,
/// it was computed with the same detector parameters, it is *about this
/// recording* (session id and file name), and the recording's
/// [`FileSignature`] still matches the one stamped into the cache. Anything
/// else recomputes — a stale cache silently answering with the wrong
/// thresholds is exactly the failure this is meant to prevent.
///
/// The signature is what makes the check sound. Modification time alone misses
/// a restored backup, a file copied back with its timestamp, and a
/// same-size replacement on a filesystem with coarse timestamps; it is kept as
/// the cheap first test, and as the only test for a cache written before
/// signatures were stamped.
///
/// The stamps are read and judged on their own first (they are the head of
/// the document), so a cache that is going to be refused costs two kilobytes
/// of parsing rather than the whole report.
#[hotpath::measure]
pub fn read_cache(
    cache: &Path,
    recording: &Path,
    params: Params,
) -> (Option<Report>, CacheOutcome) {
    let bytes = match read_current_cache(cache, recording, params) {
        Ok(b) => b,
        Err(why) => return (None, why),
    };
    // Validated as UTF-8 once, up front: `from_slice` would check every one
    // of the row tables' millions of keys separately, which made the full
    // read a third slower than parsing a `str`.
    let report = String::from_utf8(bytes)
        .ok()
        .and_then(|text| serde_json::from_str::<Report>(&text).ok());
    match report {
        Some(report) => (Some(report), CacheOutcome::Hit),
        None => (None, CacheOutcome::Unreadable),
    }
}

/// [`read_cache`] for the two projections that read no row table: the same
/// checks, the same outcome, but the tables stay on disk (see
/// [`crate::report::slim`]).
#[hotpath::measure]
fn read_cache_headlines(
    cache: &Path,
    recording: &Path,
    params: Params,
) -> (Option<Headlines>, CacheOutcome) {
    let bytes = match read_current_cache(cache, recording, params) {
        Ok(b) => b,
        Err(why) => return (None, why),
    };
    match Headlines::parse(&bytes) {
        Some(headlines) => (Some(headlines), CacheOutcome::Hit),
        None => (None, CacheOutcome::Unreadable),
    }
}

/// The headline summary (`report --summary`) out of a cached report, if the
/// cache is still valid for `recording` and `params` — [`read_cache`]'s
/// rules, without reading the report's row tables. The sidecar-derived
/// fields are re-read from the sidecar, as [`report_for`] does on a hit, so
/// this is exactly what `report_for(..)` then [`Report::headline`] returns.
pub fn read_cache_summary(
    cache: &Path,
    recording: &Path,
    params: Params,
) -> (Option<ReportSummary>, CacheOutcome) {
    let (hit, why) = read_cache_headlines(cache, recording, params);
    (
        hit.map(|h| h.summary(read_sidecar(recording).as_ref())),
        why,
    )
}

/// The trend row out of a cached report, if the cache is still valid for
/// `recording` and `params` — [`read_cache`]'s rules, without reading the
/// report's row tables. The row has no `extra` columns: a `--metric` path
/// can point anywhere in the report, so those go through [`read_cache`] and
/// [`row`].
pub fn read_cache_row(
    cache: &Path,
    recording: &Path,
    params: Params,
) -> (Option<TrendRow>, CacheOutcome) {
    let (hit, why) = read_cache_headlines(cache, recording, params);
    (hit.map(|h| h.trend_row()), why)
}

/// The cached document's bytes, once everything short of parsing its body
/// says it is current: it exists, the recording is not newer than it, and its
/// stamps (see [`judge`]) match this build, `params` and `recording`.
fn read_current_cache(
    cache: &Path,
    recording: &Path,
    params: Params,
) -> Result<Vec<u8>, CacheOutcome> {
    let cache_meta = std::fs::metadata(cache).map_err(|_| CacheOutcome::Missing)?;
    if let (Ok(rec), Ok(cached)) = (
        std::fs::metadata(recording).and_then(|m| m.modified()),
        cache_meta.modified(),
    ) && rec > cached
    {
        return Err(CacheOutcome::Stale);
    }
    let bytes = std::fs::read(cache).map_err(|_| CacheOutcome::Unreadable)?;
    let stamps = slim::read_partial::<slim::StampSlots>(&bytes).ok_or(CacheOutcome::Unreadable)?;
    match judge(&stamps, recording, params) {
        CacheOutcome::Hit => Ok(bytes),
        why => Err(why),
    }
}

/// Whether a cached report with these stamps answers for `recording` under
/// `params`: [`CacheOutcome::Hit`], or the reason it does not.
fn judge(stamps: &Stamps, recording: &Path, params: Params) -> CacheOutcome {
    // Either provenance stamp moving on means the numbers may not mean the
    // same thing any more.
    if stamps.analyzer_version != ANALYZER_VERSION || stamps.schema != SCHEMA {
        return CacheOutcome::VersionChanged;
    }
    if stamps.params != params {
        return CacheOutcome::ParamsChanged;
    }
    if !describes(&stamps.session, recording) {
        return CacheOutcome::Mismatch;
    }
    match (&stamps.recording_signature, file_signature(recording)) {
        (Some(stamped), Ok(Some(now))) if *stamped != now => CacheOutcome::Stale,
        // Unreadable or actively-growing recording: fall back to the mtime
        // test already done rather than refusing to use a cache that is
        // probably fine.
        _ => CacheOutcome::Hit,
    }
}

/// Whether a cached report is about `recording` at all — a cache directory
/// shared between recording directories, or a renamed file, must not answer
/// for the wrong session.
fn describes(session: &SessionSummary, recording: &Path) -> bool {
    let stem_ok = recording_id(recording).is_none_or(|id| id == session.session_id);
    // Compared by file name, not by the whole path: the same recording is
    // legitimately reached as `recordings/x.jsonl` and `C:\...\x.jsonl`.
    let name_ok = Path::new(&session.path)
        .file_name()
        .is_none_or(|name| Some(name) == recording.file_name());
    stem_ok && name_ok
}

/// The session id a recording's file name spells (`<id>.jsonl`), or `None`
/// when the name does not follow the recording-id rule.
fn recording_id(recording: &Path) -> Option<&str> {
    recordings::id_from_file_name(recording.file_name()?.to_str()?)
}

/// Publish a report into the cache atomically: a reader either sees the
/// previous document or this one, never a half-written one, even if two
/// analyzers race on the same session.
#[hotpath::measure]
///
/// Not synced to disk first: the cache is disposable, and a document a power
/// cut leaves torn is refused as unreadable and recomputed.
#[hotpath::measure]
fn write_cache(path: &Path, report: &Report) -> Result<(), AnalyzeError> {
    crate::atomic::write_atomic_unsynced(path, |w| report.write_json_to(w))
        .map_err(AnalyzeError::io("writing the report cache", path))
}

/// The session id `recording` is cached under and where, when `json_dir` is
/// set. A recording whose file name is not `<session id>.jsonl` has no cache
/// entry: the cache is keyed by that id.
fn cache_target<'a>(recording: &'a Path, json_dir: Option<&Path>) -> Option<(&'a str, PathBuf)> {
    let dir = json_dir?;
    match recording_id(recording) {
        Some(id) => Some((id, cache_path(dir, id))),
        None => {
            tracing::debug!(
                path = %recording.display(),
                "recording name is not a session id; not using the report cache"
            );
            None
        }
    }
}

/// Analyze `recording` afresh and publish the report to `cache`, after a
/// cache read that came back `why`.
fn recompute(
    recording: &Path,
    id: &str,
    cache: &Path,
    params: Params,
    why: CacheOutcome,
) -> Result<Report, AnalyzeError> {
    tracing::info!(
        session = %id,
        cache = %cache.display(),
        check = why.as_str(),
        "recomputing report"
    );
    let before = file_signature(recording).ok().flatten();
    let loaded = crate::load::load_session(recording)?;
    let mut report = crate::report::build(loaded, params)?;
    // A signature on both sides of the read: a recording capture was still
    // appending to is analyzed and returned, but never published under an
    // identity that does not describe what was read.
    match (before, file_signature(recording).ok().flatten()) {
        (Some(a), Some(b)) if a == b => {
            report.recording_signature = Some(a);
            write_cache(cache, &report)?;
        }
        _ => tracing::debug!(
            session = %id,
            "recording changed while it was being read; not caching the report"
        ),
    }
    Ok(report)
}

fn log_reuse(id: &str, cache: &Path) {
    tracing::info!(
        session = %id,
        cache = %cache.display(),
        check = CacheOutcome::Hit.as_str(),
        "reusing cached report"
    );
}

/// Load `recording`, reusing `json_dir`'s cached report when it is current.
/// Writes the cache back on a miss when `json_dir` is set.
///
/// A cached report is only cached for its recording: the sidecar-derived
/// fields (quality's sidecar summary, `clean`, the warnings) are re-read
/// from the sidecar on every hit, so a report cached while the run was going
/// stops saying "unfinished" once the agent has written its final sidecar.
///
/// A recording whose file name is not `<session id>.jsonl` is analyzed but
/// never cached: the cache is keyed by that id.
#[hotpath::measure]
pub fn report_for(
    recording: &Path,
    json_dir: Option<&Path>,
    params: Params,
) -> Result<(Report, CacheOutcome), AnalyzeError> {
    let Some((id, path)) = cache_target(recording, json_dir) else {
        let loaded = crate::load::load_session(recording)?;
        return Ok((crate::report::build(loaded, params)?, CacheOutcome::Missing));
    };

    let (hit, why) = read_cache(&path, recording, params);
    if let Some(mut r) = hit {
        log_reuse(id, &path);
        r.refresh_sidecar(read_sidecar(recording).as_ref());
        return Ok((r, why));
    }
    Ok((recompute(recording, id, &path, params, why)?, why))
}

/// The headline summary of `recording` (`report --summary`): what
/// [`report_for`] then [`Report::headline`] returns, with the same cache
/// reuse and write-back — but a cache hit reads only the sections the summary
/// is made of ([`read_cache_summary`]) instead of the whole report.
#[hotpath::measure]
pub fn summary_for(
    recording: &Path,
    json_dir: Option<&Path>,
    params: Params,
) -> Result<(ReportSummary, CacheOutcome), AnalyzeError> {
    let Some((id, path)) = cache_target(recording, json_dir) else {
        let (report, why) = report_for(recording, None, params)?;
        return Ok((report.headline(), why));
    };
    let (hit, why) = read_cache_summary(&path, recording, params);
    if let Some(summary) = hit {
        log_reuse(id, &path);
        return Ok((summary, why));
    }
    Ok((
        recompute(recording, id, &path, params, why)?.headline(),
        why,
    ))
}

/// The trend row of `recording`, and nothing else of its report: what
/// [`report_for`] then [`row`] returns. Without `--metric` columns a cache
/// hit reads only the report's headline sections ([`read_cache_row`]).
fn row_for(
    recording: &Path,
    json_dir: Option<&Path>,
    params: Params,
    metrics: &[String],
) -> Result<TrendRow, AnalyzeError> {
    if metrics.is_empty()
        && let Some((id, path)) = cache_target(recording, json_dir)
    {
        let (hit, why) = read_cache_row(&path, recording, params);
        if let Some(row) = hit {
            log_reuse(id, &path);
            return Ok(row);
        }
        let report = recompute(recording, id, &path, params, why)?;
        return Ok(row(&report, false, metrics));
    }
    // The report is summarized and dropped here, on the worker, so four
    // sessions in flight never means four full reports resident.
    let (report, why) = report_for(recording, json_dir, params)?;
    Ok(row(&report, why == CacheOutcome::Hit, metrics))
}

/// One session's line in the longitudinal table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendRow {
    pub session_id: String,
    pub path: String,
    pub started_utc_us: i64,
    pub started_utc: String,
    pub duration_s: f64,
    pub events: usize,
    pub drops: u64,
    pub game: Option<String>,
    pub flicks: usize,
    pub flicks_per_min: f64,
    pub overshoot_median: f64,
    pub settle_median_ms: f64,
    pub tremor_rms_counts_s: f64,
    pub path_efficiency: f64,
    pub clicks_per_min: f64,
    pub distance_m: f64,
    pub lifts: usize,
    /// Centimetres per 360° turn — the column that says whether two sessions
    /// are even comparable. `None` without a usable CPI or aim profile.
    #[serde(default)]
    pub cm_per_360: Option<f64>,
    /// Whether this row came out of the cache.
    pub from_cache: bool,
    /// Extra metrics requested with `--metric`, in the order they were asked
    /// for. Dotted paths into the report JSON.
    #[serde(default)]
    pub extra: Vec<(String, Option<f64>)>,
}

/// Resolve dotted paths such as `micro.band_ratio_8_12` in a report, against
/// one serialization of it: turning the report into a `Value` is the
/// expensive half (a three-hour session's report is tens of megabytes of
/// `per_second` rows), so it is done once for all of them.
pub fn metrics_at(report: &Report, paths: &[String]) -> Vec<(String, Option<f64>)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let value = serde_json::to_value(report).ok();
    paths
        .iter()
        .map(|p| (p.clone(), value.as_ref().and_then(|v| lookup(v, p))))
        .collect()
}

fn lookup(value: &serde_json::Value, path: &str) -> Option<f64> {
    let mut cur = value;
    for part in path.split('.') {
        cur = match part.parse::<usize>() {
            Ok(i) => cur.get(i)?,
            Err(_) => cur.get(part)?,
        };
    }
    cur.as_f64()
}

/// Summarize one report as a trend row.
pub fn row(report: &Report, from_cache: bool, metrics: &[String]) -> TrendRow {
    TrendRow {
        session_id: report.session.session_id.clone(),
        path: report.session.path.clone(),
        started_utc_us: report.session.started_utc_us,
        started_utc: report.session.started_utc.clone(),
        duration_s: report.session.duration_s,
        events: report.session.event_count,
        drops: report.quality.ring_drops,
        game: report.session.game.clone(),
        flicks: report.flicks.count,
        flicks_per_min: report.flicks.per_minute,
        overshoot_median: report.flicks.overshoot_ratio.median,
        settle_median_ms: report.flicks.settle_ms.median,
        tremor_rms_counts_s: report.micro.tremor_rms_counts_s,
        path_efficiency: report.kinematics.path_efficiency_weighted,
        clicks_per_min: report.clicks.clicks_per_min,
        distance_m: report.kinematics.total_distance_m,
        lifts: report.lifts.count,
        cm_per_360: report.session.cm_per_360,
        from_cache,
        extra: metrics_at(report, metrics),
    }
}

/// Most sessions `compute` analyzes at once.
///
/// Deliberately small. A cold `trend` over 36 recordings used to run them
/// strictly one after another and took 22.9 s (`docs/PERFORMANCE-2026-09-20.md`,
/// A7), but each session is already internally parallel — the loader splits the
/// file across cores and `report::build` runs the metric groups concurrently —
/// so a handful of sessions at a time saturates the box without
/// oversubscribing it badly.
const TREND_MAX_WORKERS: usize = 4;

/// Recording bytes being analyzed at any one instant.
///
/// The real constraint is memory, not cores: a session peaks at roughly 2.5×
/// its file size in RAM, and recordings run past a gigabyte. One session is
/// always admitted however large it is, so a 1.2 GB recording runs on its own
/// and the small ones still pack four at a time — an idle worker takes the
/// first recording that fits rather than waiting behind one that does not
/// (see [`next_admissible`]).
const TREND_BYTES_IN_FLIGHT: u64 = 512 * 1024 * 1024;

/// What one worker hands back for one recording: its row, or why it was
/// skipped. Skips are logged by the caller, in path order, so a `trend` run
/// says the same things in the same sequence however the work was scheduled.
type SessionOutcome = Result<TrendRow, String>;

/// The scheduler's shared state: which jobs have been handed out, how many
/// bytes are being worked on, and where the answers go.
struct Schedule<T> {
    taken: Vec<bool>,
    remaining: usize,
    in_flight: u64,
    active: usize,
    out: Vec<Option<T>>,
}

/// The job an idle worker should start now: the first one not yet taken that
/// fits what is left of `budget`. `None` means wait (or, with nothing left
/// untaken, stop).
///
/// When nothing is running, the first untaken job is admitted whatever its
/// size, so one larger than the whole budget still runs — alone, and only
/// after every job that fits has been handed out.
///
/// Taking strictly the next job instead made every idle worker wait behind a
/// large one while later small ones would have fitted
/// (`docs/PERFORMANCE-2026-11-14.md`, A6).
fn next_admissible(
    sizes: &[u64],
    taken: &[bool],
    in_flight: u64,
    active: usize,
    budget: u64,
) -> Option<usize> {
    let mut untaken = (0..sizes.len()).filter(|&i| !taken[i]);
    let first = untaken.next()?;
    std::iter::once(first)
        .chain(untaken)
        .find(|&i| in_flight.saturating_add(sizes[i]) <= budget)
        .or((active == 0).then_some(first))
}

/// Run `job(i)` for every `i` in `0..sizes.len()` on up to `workers` scoped
/// threads, never with more than `budget` bytes of `sizes` in flight unless a
/// single job exceeds it on its own. Results land in their own slot, so the
/// output does not depend on which worker ran what, or when.
fn run_within_budget<T: Send>(
    sizes: &[u64],
    workers: usize,
    budget: u64,
    job: impl Fn(usize) -> T + Sync,
) -> Vec<Option<T>> {
    let schedule = Mutex::new(Schedule {
        taken: vec![false; sizes.len()],
        remaining: sizes.len(),
        in_flight: 0,
        active: 0,
        out: (0..sizes.len()).map(|_| None).collect(),
    });
    let admitted = Condvar::new();
    let (sched, admitted, job) = (&schedule, &admitted, &job);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(move || {
                loop {
                    let index = {
                        let mut s = sched.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if s.remaining == 0 {
                                return;
                            }
                            if let Some(index) =
                                next_admissible(sizes, &s.taken, s.in_flight, s.active, budget)
                            {
                                s.taken[index] = true;
                                s.remaining -= 1;
                                s.in_flight += sizes[index];
                                s.active += 1;
                                break index;
                            }
                            // Something is running (or nothing would have
                            // been refused), and its completion notifies.
                            s = admitted.wait(s).unwrap_or_else(|e| e.into_inner());
                        }
                    };

                    // Released even when the job panics: the scope re-raises
                    // the panic once every worker is done, and the others can
                    // only get there if the bytes this one held are returned.
                    let slot = Release {
                        sched,
                        admitted,
                        size: sizes[index],
                    };
                    let outcome = job(index);
                    sched.lock().unwrap_or_else(|e| e.into_inner()).out[index] = Some(outcome);
                    drop(slot);
                }
            });
        }
    });
    schedule.into_inner().unwrap_or_else(|e| e.into_inner()).out
}

/// Hands a finished job's bytes back to the budget and wakes the waiters.
struct Release<'a, T> {
    sched: &'a Mutex<Schedule<T>>,
    admitted: &'a Condvar,
    size: u64,
}

impl<T> Drop for Release<'_, T> {
    fn drop(&mut self) {
        {
            let mut s = self.sched.lock().unwrap_or_else(|e| e.into_inner());
            s.in_flight -= self.size;
            s.active -= 1;
        }
        self.admitted.notify_all();
    }
}

fn analyze_one(
    path: &Path,
    json_dir: Option<&Path>,
    params: Params,
    metrics: &[String],
) -> SessionOutcome {
    // The whole chain: the outer message alone is "failed to read x", without
    // the reason it failed.
    row_for(path, json_dir, params, metrics).map_err(|e| crate::load::error_chain(&e))
}

/// The recordings in `dir` by the recording-id rule, in path order. A
/// `.jsonl` whose name is not a session id is skipped with a debug line, as
/// [`crate::load::scan_dir`] skips it.
fn recording_paths(dir: &Path) -> Result<Vec<PathBuf>, AnalyzeError> {
    let rd = std::fs::read_dir(dir).map_err(AnalyzeError::io("reading the directory", dir))?;
    let mut paths: Vec<PathBuf> = Vec::new();
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
        paths.push(p);
    }
    paths.sort();
    Ok(paths)
}

/// Build the trend table over every recording in `dir`, oldest session first.
///
/// Sessions are analyzed concurrently, bounded by [`TREND_MAX_WORKERS`] and by
/// [`TREND_BYTES_IN_FLIGHT`]. The table does not depend on the schedule: rows
/// land in their directory-order slot and are then sorted by start time exactly
/// as they always were. A session that cannot be analyzed is skipped with a
/// warning; only an unreadable `dir`, or parameters that do not validate, fail
/// the whole table.
#[hotpath::measure]
pub fn compute(
    dir: &Path,
    json_dir: Option<&Path>,
    params: Params,
    metrics: &[String],
) -> Result<Vec<TrendRow>, AnalyzeError> {
    // Checked once here rather than failing every session the same way.
    params.validate()?;
    let paths = recording_paths(dir)?;

    let sizes: Vec<u64> = paths
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .collect();
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(TREND_MAX_WORKERS)
        .min(paths.len());

    let out = if workers <= 1 {
        paths
            .iter()
            .map(|p| Some(analyze_one(p, json_dir, params, metrics)))
            .collect()
    } else {
        run_within_budget(&sizes, workers, TREND_BYTES_IN_FLIGHT, |index| {
            analyze_one(&paths[index], json_dir, params, metrics)
        })
    };

    let mut rows = Vec::with_capacity(paths.len());
    for (p, outcome) in paths.iter().zip(out) {
        match outcome {
            Some(Ok(r)) => rows.push(r),
            Some(Err(error)) => {
                tracing::warn!(path = %p.display(), %error, "skipping session")
            }
            None => {}
        }
    }
    rows.sort_by(|a, b| {
        a.started_utc_us
            .cmp(&b.started_utc_us)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(rows)
}

/// `trend --json`: the rows, each tagged with [`TREND_SCHEMA`].
pub fn to_json(rows: &[TrendRow]) -> serde_json::Result<String> {
    crate::tagged_rows_json(TREND_SCHEMA, rows)
}

pub fn write_csv<W: Write>(w: &mut W, rows: &[TrendRow]) -> io::Result<()> {
    let extra: Vec<&str> = rows
        .first()
        .map(|r| r.extra.iter().map(|(k, _)| k.as_str()).collect())
        .unwrap_or_default();
    write!(
        w,
        "session_id,started_utc,started_utc_us,duration_s,events,drops,game,flicks,\
flicks_per_min,overshoot_median,settle_median_ms,tremor_rms_counts_s,path_efficiency,\
clicks_per_min,distance_m,lifts,cm_per_360"
    )?;
    for k in &extra {
        write!(w, ",{}", crate::per_second::csv_field(k))?;
    }
    writeln!(w)?;

    for r in rows {
        write!(
            w,
            "{},{},{},{:.3},{},{},{},{},{:.3},{:.5},{:.2},{:.3},{:.5},{:.3},{:.4},{}",
            crate::per_second::csv_field(&r.session_id),
            crate::per_second::csv_field(&r.started_utc),
            r.started_utc_us,
            r.duration_s,
            r.events,
            r.drops,
            crate::per_second::csv_field(r.game.as_deref().unwrap_or("")),
            r.flicks,
            r.flicks_per_min,
            r.overshoot_median,
            r.settle_median_ms,
            r.tremor_rms_counts_s,
            r.path_efficiency,
            r.clicks_per_min,
            r.distance_m,
            r.lifts,
        )?;
        match r.cm_per_360 {
            Some(cm) => write!(w, ",{cm:.2}")?,
            None => write!(w, ",")?,
        }
        for (_, v) in &r.extra {
            match v {
                Some(x) => write!(w, ",{x}")?,
                None => write!(w, ",")?,
            }
        }
        writeln!(w)?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn to_csv(rows: &[TrendRow]) -> String {
    let mut buf = Vec::new();
    write_csv(&mut buf, rows).expect("writing to a Vec cannot fail");
    String::from_utf8(buf).expect("UTF-8 output")
}

/// The terminal table.
pub fn render(rows: &[TrendRow]) -> String {
    let mut o = String::new();
    if rows.is_empty() {
        return "no sessions found\n".to_string();
    }
    let extra: Vec<&str> = rows[0].extra.iter().map(|(k, _)| k.as_str()).collect();
    o.push_str(&format!(
        "{:<22} {:<20} {:>10} {:>10} {:>7} {:>7} {:>9} {:>9} {:>8} {:>8} {:>9}",
        "SESSION",
        "STARTED (UTC)",
        "DURATION",
        "EVENTS",
        "DROPS",
        "FLICKS",
        "OVERSHOOT",
        "SETTLE ms",
        "TREMOR",
        "PATH EFF",
        "cm/360°"
    ));
    for k in &extra {
        o.push_str(&format!(" {k:>14}"));
    }
    o.push('\n');
    for r in rows {
        o.push_str(&format!(
            "{:<22} {:<20} {:>10} {:>10} {:>7} {:>7} {:>9.4} {:>9.1} {:>8.0} {:>8.3} {:>9}",
            truncate(&r.session_id, 22),
            // Characters, not bytes: the cache is hand-editable, and a byte
            // index into a non-ASCII string can land inside a character.
            r.started_utc.chars().take(19).collect::<String>(),
            format_duration(r.duration_s),
            r.events,
            r.drops,
            r.flicks,
            r.overshoot_median,
            r.settle_median_ms,
            r.tremor_rms_counts_s,
            r.path_efficiency,
            r.cm_per_360
                .map_or("—".to_string(), |cm| format!("{cm:.1}")),
        ));
        for (_, v) in &r.extra {
            match v {
                Some(x) => o.push_str(&format!(" {x:>14.4}")),
                None => o.push_str(&format!(" {:>14}", "—")),
            }
        }
        o.push('\n');
    }
    let cached = rows.iter().filter(|r| r.from_cache).count();
    o.push_str(&format!(
        "\n{} session(s), {} from cache, first {}\n",
        rows.len(),
        cached,
        format_utc_us(rows[0].started_utc_us)
    ));
    o
}

/// `s` cut to at most `n` characters, the last one an ellipsis when
/// anything was cut. `n == 0` is the empty string.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else if n == 0 {
        String::new()
    } else {
        s.chars().take(n - 1).chain(std::iter::once('…')).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn flicky_events(n: usize) -> Vec<telemouse_core::RawEvent> {
        let mut b = testutil::StreamBuilder::new();
        b.idle_ms(200);
        for _ in 0..n {
            b.flick_cycle(1, 0, 500);
        }
        b.into_events()
    }

    /// A report cached while the run was still going must not say
    /// "unfinished" forever: the recording never changes again, so only the
    /// sidecar can bring it up to date, and a cache hit re-reads it.
    #[test]
    fn a_cache_hit_picks_up_a_sidecar_written_after_the_report() {
        use telemouse_core::recordings::{ExitReason, SessionMeta};

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        two_sessions(dir.path());
        let recording = dir.path().join("s-day1.jsonl");
        let sidecar = dir.path().join("s-day1.meta.json");
        let meta = |exit: ExitReason| SessionMeta {
            session_id: "s-day1".into(),
            exit: exit.as_str().into(),
            ..Default::default()
        };
        std::fs::write(
            &sidecar,
            meta(ExitReason::Running).to_json_pretty().unwrap(),
        )
        .unwrap();

        let (first, why) = report_for(&recording, Some(&cache), Params::default()).unwrap();
        assert_eq!(why, CacheOutcome::Missing);
        assert!(first.quality.sidecar.as_ref().unwrap().unfinished);
        assert!(!first.quality.clean);

        // The agent stops and writes its final sidecar. The recording is
        // untouched, so the cache is still valid — and must say so.
        std::fs::write(
            &sidecar,
            meta(ExitReason::Interrupt).to_json_pretty().unwrap(),
        )
        .unwrap();
        let (second, why) = report_for(&recording, Some(&cache), Params::default()).unwrap();
        assert_eq!(why, CacheOutcome::Hit);
        let sc = second.quality.sidecar.as_ref().unwrap();
        assert!(!sc.unfinished);
        assert_eq!(sc.exit, "interrupt");
        assert!(second.quality.clean, "{:?}", second.warnings);
        assert!(second.warnings.is_empty(), "{:?}", second.warnings);
    }

    /// Bad parameters fail the table up front, with the field named, rather
    /// than skipping every session one by one.
    #[test]
    fn a_trend_with_invalid_params_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        two_sessions(dir.path());
        let err = compute(
            dir.path(),
            None,
            Params {
                grid_dt_s: 3.0,
                ..Default::default()
            },
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, AnalyzeError::Params(ref p) if p.field == "grid_dt_s"),
            "{err}"
        );
    }

    #[test]
    fn the_json_rows_carry_their_schema() {
        let dir = tempfile::tempdir().unwrap();
        two_sessions(dir.path());
        let rows = compute(dir.path(), None, Params::default(), &[]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&to_json(&rows).unwrap()).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr.iter().all(|r| r["schema"] == TREND_SCHEMA));
        assert_eq!(arr[0]["session_id"], "s-day1");
        assert_eq!(arr[1]["flicks"], 6);
    }

    #[test]
    fn rendering_survives_hand_edited_strings() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("abcdef", 3), "ab…");
        assert_eq!(truncate("abc", 3), "abc");
        let mut row = TrendRow {
            session_id: "é".repeat(40),
            path: String::new(),
            started_utc_us: 0,
            // Multi-byte characters straddling byte 19.
            started_utc: "2026-09-12 21:46:5é€€€".into(),
            duration_s: 1.0,
            events: 0,
            drops: 0,
            game: None,
            flicks: 0,
            flicks_per_min: 0.0,
            overshoot_median: 0.0,
            settle_median_ms: 0.0,
            tremor_rms_counts_s: 0.0,
            path_efficiency: 0.0,
            clicks_per_min: 0.0,
            distance_m: 0.0,
            lifts: 0,
            cm_per_360: None,
            from_cache: false,
            extra: Vec::new(),
        };
        let text = render(std::slice::from_ref(&row));
        assert!(text.contains("2026-09-12 21:46:5é "), "{text}");
        assert!(!text.contains('€'), "{text}");
        row.started_utc.clear();
        assert!(render(&[row]).contains("1 session(s)"));
    }

    /// Two synthetic sessions in a temp dir, the second starting a day later
    /// and containing twice the flicks.
    fn two_sessions(dir: &Path) {
        let mut a = testutil::session_cfg();
        a.session_id = "s-day1".into();
        testutil::write_session(dir, &a, Some("cs2.exe"), &flicky_events(3), &[], 256);

        let b = testutil::session_cfg_at("s-day2", testutil::FIXTURE_UTC0 + 86_400_000_000);
        testutil::write_session(dir, &b, Some("cs2.exe"), &flicky_events(6), &[], 256);
    }

    #[test]
    fn a_trend_over_two_sessions_has_one_row_each_in_date_order() {
        let dir = tempfile::tempdir().unwrap();
        two_sessions(dir.path());

        let rows = compute(dir.path(), None, Params::default(), &[]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, "s-day1");
        assert_eq!(rows[1].session_id, "s-day2");
        assert!(rows[1].started_utc_us > rows[0].started_utc_us);
        assert_eq!(rows[0].flicks, 3);
        assert_eq!(rows[1].flicks, 6);
        assert!(rows[0].overshoot_median > 0.0);
        assert!(rows[0].path_efficiency > 0.0);
        assert!(rows.iter().all(|r| !r.from_cache));
        assert_eq!(rows[0].game.as_deref(), Some("cs2.exe"));

        let csv = to_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("session_id,started_utc,"));
        assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());

        let text = render(&rows);
        assert!(text.contains("s-day1"), "{text}");
        assert!(text.contains("OVERSHOOT"), "{text}");
    }

    /// Sessions are analyzed concurrently; the table must not be able to tell.
    /// The file names ascend while the start times descend, so directory order
    /// and table order disagree and a completion-ordered result would show.
    #[test]
    fn a_concurrent_trend_is_ordered_by_start_time_not_by_completion() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..6i64 {
            let mut cfg = testutil::session_cfg();
            cfg.session_id = format!("s-{i:02}");
            cfg.started_utc_us -= i * 3_600_000_000;
            cfg.anchor.utc_us -= i * 3_600_000_000;
            testutil::write_session(
                dir.path(),
                &cfg,
                Some("cs2.exe"),
                &flicky_events(1 + i as usize),
                &[],
                64,
            );
        }

        let first = compute(dir.path(), None, Params::default(), &[]).unwrap();
        let ids: Vec<&str> = first.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["s-05", "s-04", "s-03", "s-02", "s-01", "s-00"]);
        assert_eq!(first[0].flicks, 6);
        assert_eq!(first[5].flicks, 1);

        // Same table, run to run, whatever the scheduler did.
        let again = compute(dir.path(), None, Params::default(), &[]).unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn the_second_run_comes_out_of_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        two_sessions(dir.path());

        let first = compute(dir.path(), Some(&cache), Params::default(), &[]).unwrap();
        assert!(first.iter().all(|r| !r.from_cache));
        assert!(cache.join("s-day1.report.json").exists());
        assert!(cache.join("s-day2.report.json").exists());

        let second = compute(dir.path(), Some(&cache), Params::default(), &[]).unwrap();
        assert!(second.iter().all(|r| r.from_cache), "{second:#?}");
        // Same numbers either way.
        assert_eq!(first[0].flicks, second[0].flicks);
        assert!((first[1].overshoot_median - second[1].overshoot_median).abs() < 1e-9);
    }

    #[test]
    fn a_cache_from_another_analyzer_version_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        two_sessions(dir.path());
        compute(dir.path(), Some(&cache_dir), Params::default(), &[]).unwrap();

        let path = cache_path(&cache_dir, "s-day1");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["analyzer_version"] = serde_json::Value::String("0.0.0-ancient".into());
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();

        let recording = dir.path().join("s-day1.jsonl");
        let (hit, why) = read_cache(&path, &recording, Params::default());
        assert!(hit.is_none());
        assert_eq!(why, CacheOutcome::VersionChanged);

        // ...and the next run rewrites it with the current version.
        let rows = compute(dir.path(), Some(&cache_dir), Params::default(), &[]).unwrap();
        assert!(!rows[0].from_cache);
        assert!(rows[1].from_cache);
        let (hit, why) = read_cache(&path, &recording, Params::default());
        assert_eq!(why, CacheOutcome::Hit);
        assert_eq!(hit.unwrap().analyzer_version, ANALYZER_VERSION);
    }

    /// A cache computed with different thresholds answers a different
    /// question, so it must never be handed back for this one.
    #[test]
    fn a_cache_computed_with_other_parameters_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        two_sessions(dir.path());
        compute(dir.path(), Some(&cache_dir), Params::default(), &[]).unwrap();

        // The fixture's pulls peak at 60 000 counts/s, so this threshold puts
        // the flick count at zero — a visibly different answer.
        let other = Params {
            flick_speed: 500_000.0,
            ..Default::default()
        };
        let path = cache_path(&cache_dir, "s-day1");
        let recording = dir.path().join("s-day1.jsonl");
        let (hit, why) = read_cache(&path, &recording, other);
        assert!(hit.is_none());
        assert_eq!(why, CacheOutcome::ParamsChanged);

        // Recomputing under the new thresholds finds no flicks at all, which
        // is exactly the answer a stale cache would have hidden.
        let rows = compute(dir.path(), Some(&cache_dir), other, &[]).unwrap();
        assert!(rows.iter().all(|r| !r.from_cache));
        assert_eq!(rows[0].flicks, 0);
    }

    /// The params comparison above is only sound if `Params` survives the JSON
    /// round-trip bit-for-bit; every field is pinned here so a future float
    /// value that does not cannot slip in unnoticed.
    #[test]
    fn params_round_trip_through_json_exactly() {
        let p = Params::default();
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Params>(&text).unwrap(), p);
        let odd = Params {
            grid_dt_s: 1.0 / 3.0,
            still_speed: 0.1 + 0.2,
            lift_opposite_cos: -0.7,
            ..p
        };
        let text = serde_json::to_string(&odd).unwrap();
        assert_eq!(serde_json::from_str::<Params>(&text).unwrap(), odd);
    }

    /// A marked, clicked session with a sidecar: every section the summary
    /// and the trend row project from has something in it.
    fn marked_session(dir: &Path) -> PathBuf {
        use telemouse_core::recordings::{ExitReason, SessionMeta};

        let mut cfg = testutil::session_cfg();
        cfg.session_id = "s-marked".into();
        let markers = [("sens A", 400), ("sens B", 2_000)].map(|(label, ms)| {
            let mut m = testutil::marker_at(ms, label);
            m.session_id = cfg.session_id.clone();
            m
        });
        testutil::write_session(dir, &cfg, Some("cs2.exe"), &flicky_events(8), &markers, 128);
        let meta = SessionMeta {
            session_id: "s-marked".into(),
            exit: ExitReason::Running.as_str().into(),
            ..Default::default()
        };
        std::fs::write(
            dir.join("s-marked.meta.json"),
            meta.to_json_pretty().unwrap(),
        )
        .unwrap();
        dir.join("s-marked.jsonl")
    }

    /// The readers that leave the row tables on disk must give exactly what
    /// the full parse gives: the summary and the trend row, field for field.
    #[test]
    fn the_slim_cache_readers_match_the_full_parse() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let recording = marked_session(dir.path());
        let params = Params::default();

        let (built, why) = report_for(&recording, Some(&cache_dir), params).unwrap();
        assert_eq!(why, CacheOutcome::Missing);
        // The fixture has to exercise the tables the slim readers skip.
        assert!(!built.flicks.flicks.is_empty() && !built.per_second.is_empty());
        assert_eq!(built.markers.len(), 2);

        let cache = cache_path(&cache_dir, "s-marked");
        let (full, why) = report_for(&recording, Some(&cache_dir), params).unwrap();
        assert_eq!(why, CacheOutcome::Hit);

        let (summary, why) = read_cache_summary(&cache, &recording, params);
        assert_eq!(why, CacheOutcome::Hit);
        let summary = summary.unwrap();
        assert_eq!(summary, full.headline());
        assert!(summary.quality.unfinished && !summary.warnings.is_empty());
        assert_eq!(summary.segments.len(), 3);
        // Byte for byte what `report --summary` prints either way.
        assert_eq!(
            serde_json::to_string_pretty(&summary).unwrap(),
            serde_json::to_string_pretty(&full.headline()).unwrap()
        );
        let (via_for, why) = summary_for(&recording, Some(&cache_dir), params).unwrap();
        assert_eq!((via_for, why), (summary, CacheOutcome::Hit));

        let (slim_row, why) = read_cache_row(&cache, &recording, params);
        assert_eq!(why, CacheOutcome::Hit);
        assert_eq!(slim_row.unwrap(), row(&full, true, &[]));
        assert_eq!(
            row_for(&recording, Some(&cache_dir), params, &[]).unwrap(),
            row(&full, true, &[])
        );

        // A document with its keys in another order (here: sorted, as a
        // `Value` round trip leaves them, so the tables no longer come last)
        // reads the same.
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cache).unwrap()).unwrap();
        std::fs::write(&cache, serde_json::to_string(&v).unwrap()).unwrap();
        let (full, why) = report_for(&recording, Some(&cache_dir), params).unwrap();
        assert_eq!(why, CacheOutcome::Hit);
        let (reordered, why) = read_cache_summary(&cache, &recording, params);
        assert_eq!(why, CacheOutcome::Hit);
        assert_eq!(reordered.unwrap(), full.headline());
        assert_eq!(
            read_cache_row(&cache, &recording, params).0.unwrap(),
            row(&full, true, &[])
        );
    }

    /// The slim readers refuse a cache for the same reasons the full one
    /// does, and a summary asked of a refused cache is recomputed and equal.
    #[test]
    fn the_slim_cache_readers_refuse_what_the_full_reader_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        let recording = marked_session(dir.path());
        let params = Params::default();
        let (built, _) = report_for(&recording, Some(&cache_dir), params).unwrap();
        let cache = cache_path(&cache_dir, "s-marked");
        let outcomes = |params: Params| {
            (
                read_cache(&cache, &recording, params).1,
                read_cache_summary(&cache, &recording, params).1,
                read_cache_row(&cache, &recording, params).1,
            )
        };
        let all = |why: CacheOutcome| (why, why, why);

        let other = Params {
            flick_speed: 500_000.0,
            ..params
        };
        assert_eq!(outcomes(other), all(CacheOutcome::ParamsChanged));

        let original = std::fs::read_to_string(&cache).unwrap();
        let edited = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_str(&original).unwrap();
            edit(&mut v);
            std::fs::write(&cache, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        };
        edited(&|v| v["analyzer_version"] = "0.0.0-ancient".into());
        assert_eq!(outcomes(params), all(CacheOutcome::VersionChanged));
        edited(&|v| v["session"]["session_id"] = "s-other".into());
        assert_eq!(outcomes(params), all(CacheOutcome::Mismatch));
        // A section the summary needs is gone: unreadable to every reader,
        // even though the stamps are fine.
        edited(&|v| {
            v.as_object_mut().unwrap().remove("micro");
        });
        assert_eq!(outcomes(params), all(CacheOutcome::Unreadable));
        // A stale version stamp is the reason given even when the rest of
        // the document would not parse: it is judged before anything else.
        edited(&|v| {
            v.as_object_mut().unwrap().remove("micro");
            v["schema"] = "telemouse-analyze/2".into();
        });
        assert_eq!(outcomes(params), all(CacheOutcome::VersionChanged));
        std::fs::write(&cache, "{ not json").unwrap();
        assert_eq!(outcomes(params), all(CacheOutcome::Unreadable));

        // ...and asking for the summary anyway recomputes and republishes.
        let (summary, why) = summary_for(&recording, Some(&cache_dir), params).unwrap();
        assert_eq!(why, CacheOutcome::Unreadable);
        assert_eq!(summary, built.headline());
        assert_eq!(outcomes(params), all(CacheOutcome::Hit));
    }

    /// An idle worker takes the first recording that fits, not strictly the
    /// next one.
    #[test]
    fn the_scheduler_skips_ahead_to_a_recording_that_fits() {
        let sizes = [60, 50, 10, 200, 30];
        let none = [false; 5];
        // Nothing running: the first one, in order.
        assert_eq!(next_admissible(&sizes, &none, 0, 0, 100), Some(0));
        // 60 in flight: 50 does not fit, 10 does.
        let t = [true, false, false, false, false];
        assert_eq!(next_admissible(&sizes, &t, 60, 1, 100), Some(2));
        // 70 in flight: 50 and 200 do not fit, 30 does.
        let t = [true, false, true, false, false];
        assert_eq!(next_admissible(&sizes, &t, 70, 2, 100), Some(4));
        // 100 in flight and nothing fits: wait.
        let t = [true, false, true, false, true];
        assert_eq!(next_admissible(&sizes, &t, 100, 3, 100), None);
        // Everything that fits is done; the oversized one is admitted only
        // when nothing else is running.
        let t = [true, true, true, false, true];
        assert_eq!(next_admissible(&sizes, &t, 50, 1, 100), None);
        assert_eq!(next_admissible(&sizes, &t, 0, 0, 100), Some(3));
        // A smaller one is preferred to an oversized one ahead of it.
        let t = [true, true, true, false, false];
        assert_eq!(next_admissible(&sizes, &t, 0, 0, 100), Some(4));
        // Nothing left.
        assert_eq!(next_admissible(&sizes, &[true; 5], 0, 0, 100), None);
        assert_eq!(next_admissible(&[], &[], 0, 0, 100), None);
        // Sizes that would overflow the sum are simply too large.
        assert_eq!(
            next_admissible(&[u64::MAX, 1], &[false; 2], 5, 1, 100),
            Some(1)
        );
    }

    /// Every job runs exactly once and lands in its own slot, the budget is
    /// never exceeded except by a job running alone, and jobs larger than
    /// the whole budget are not starved.
    #[test]
    fn the_scheduler_runs_everything_within_the_budget() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        const BUDGET: u64 = 100;
        let sizes: Vec<u64> = (0..60u64)
            .map(|i| match i % 10 {
                0 => 250, // larger than the whole budget
                3 => 100, // exactly the budget
                7 => 0,
                k => 5 + k * 9,
            })
            .collect();
        let (in_flight, active) = (AtomicU64::new(0), AtomicUsize::new(0));
        let runs: Vec<AtomicUsize> = sizes.iter().map(|_| AtomicUsize::new(0)).collect();
        let (over_budget, max_active) = (AtomicUsize::new(0), AtomicUsize::new(0));

        let out = run_within_budget(&sizes, 4, BUDGET, |i| {
            let bytes = in_flight.fetch_add(sizes[i], Ordering::SeqCst) + sizes[i];
            let others = active.fetch_add(1, Ordering::SeqCst);
            max_active.fetch_max(others + 1, Ordering::SeqCst);
            if bytes > BUDGET && others > 0 {
                over_budget.fetch_add(1, Ordering::SeqCst);
            }
            runs[i].fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_micros(200 + 37 * (i as u64 % 5)));
            // Still alone at the end, if it was admitted over the budget.
            if in_flight.load(Ordering::SeqCst) > BUDGET && active.load(Ordering::SeqCst) > 1 {
                over_budget.fetch_add(1, Ordering::SeqCst);
            }
            active.fetch_sub(1, Ordering::SeqCst);
            in_flight.fetch_sub(sizes[i], Ordering::SeqCst);
            i * 3
        });

        assert_eq!(
            out,
            (0..sizes.len()).map(|i| Some(i * 3)).collect::<Vec<_>>()
        );
        assert!(runs.iter().all(|r| r.load(Ordering::SeqCst) == 1));
        assert_eq!(over_budget.load(Ordering::SeqCst), 0);
        assert!(max_active.load(Ordering::SeqCst) <= 4);

        // One worker, and no jobs at all, are both fine.
        assert_eq!(
            run_within_budget(&[500, 1], 1, BUDGET, |i| i),
            [Some(0), Some(1)]
        );
        assert!(run_within_budget(&[], 4, BUDGET, |i| i).is_empty());
    }

    #[test]
    fn a_missing_cache_file_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (hit, why) = read_cache(
            &dir.path().join("nope.json"),
            &dir.path().join("x.jsonl"),
            Params::default(),
        );
        assert!(hit.is_none());
        assert_eq!(why, CacheOutcome::Missing);
    }

    #[test]
    fn extra_metrics_are_resolved_by_dotted_path() {
        let dir = tempfile::tempdir().unwrap();
        two_sessions(dir.path());
        let metrics = vec![
            "micro.band_ratio_8_12".to_string(),
            "kinematics.moving_fraction".to_string(),
            "nope.not_here".to_string(),
        ];
        let rows = compute(dir.path(), None, Params::default(), &metrics).unwrap();
        assert_eq!(rows[0].extra.len(), 3);
        assert_eq!(rows[0].extra[0].0, "micro.band_ratio_8_12");
        assert!(rows[0].extra[0].1.is_some());
        assert!(rows[0].extra[1].1.unwrap() > 0.0);
        assert!(rows[0].extra[2].1.is_none());
        let csv = to_csv(&rows);
        assert!(
            csv.lines()
                .next()
                .unwrap()
                .contains("micro.band_ratio_8_12")
        );
    }

    #[test]
    fn an_empty_directory_yields_an_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let rows = compute(dir.path(), None, Params::default(), &[]).unwrap();
        assert!(rows.is_empty());
        assert_eq!(render(&rows), "no sessions found\n");
    }
}
