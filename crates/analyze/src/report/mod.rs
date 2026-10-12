//! Assembling every metric group into one report.
//!
//! Three renderings share one structure: the terminal summary (the user-facing
//! deliverable of the analysis phase, in `render`), the full JSON document,
//! and the CSVs that make up the plan's derived tables. `summary` holds the
//! few-KB headline projection (`report --summary`).
//!
//! The JSON carries its own provenance — analyzer version, generation time,
//! compute cost, grid geometry — because these reports get cached to disk and
//! compared across weeks, and a number without the code version that produced
//! it is not comparable to anything.

mod render;
mod summary;
#[cfg(test)]
mod tests;

use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use telemouse_core::now_utc_us;
use telemouse_core::recordings::SessionMeta;

use crate::clicks::{self, ClickReport};
use crate::error::AnalyzeError;
use crate::flicks::{self, FlickReport};
use crate::kinematics::{self, Kinematics};
use crate::lifts::{self, LiftReport};
use crate::load::LoadedSession;
use crate::markers::{self, MarkerRow, SegmentReport};
use crate::micro::{self, MicroReport};
use crate::per_minute::{self, MinuteRow};
use crate::per_second::{self, SecondRow};
use crate::quality::{self, QualityReport};
use crate::series::{Params, ParamsError, Prepared, prepare};
use crate::timefmt::format_utc_us;

pub use summary::{
    ClickHeadline, FlickHeadline, KinematicsHeadline, LiftHeadline, MAX_SUMMARY_LABEL_CHARS,
    MAX_SUMMARY_MARKERS, MicroHeadline, QualityHeadline, ReportSummary, SUMMARY_SCHEMA,
    SummaryMarker, SummarySegment,
};

/// Bumped whenever the JSON shape changes. `/3` added the load timing, the
/// polling-rate estimate, the sidecar summary, cm/360 and the unparseable-line
/// detail; every addition is a new field, but a cached `/2` document has none
/// of them, so it is recomputed rather than deserialized with defaults.
pub const SCHEMA: &str = "telemouse-analyze/3";
/// The build that produced a report. Cached reports are recomputed when this
/// no longer matches.
pub const ANALYZER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Milliseconds spent in one phase of the pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTiming {
    pub phase: String,
    pub ms: f64,
}

/// What the recording is: identity, span, counts, and the aim profile every
/// degree-valued number was converted with.
///
/// Embedded whole in [`ReportSummary`] as well as in the report. That means a
/// field added here changes the *summary's* shape too — `/3`'s `cm_per_360`
/// did, without a `SUMMARY_SCHEMA` bump. Adding a field here is a summary
/// schema change as much as a report one; bump both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub path: String,
    pub started_utc_us: i64,
    pub started_utc: String,
    pub duration_s: f64,
    /// Span actually analyzed; differs from `duration_s` only on a truncated
    /// grid, and is the denominator of every per-minute rate in this report.
    pub analysis_duration_s: f64,
    pub event_count: usize,
    pub batch_count: usize,
    pub marker_count: usize,
    pub mouse_cpi: f64,
    pub capture_version: String,
    /// Dominant foreground process, if the capture agent knew one.
    pub game: Option<String>,
    pub sens: f64,
    pub yaw_coeff: f64,
    pub pitch_coeff: f64,
    /// `sens * yaw_coeff` — the number every degree metric is scaled by.
    pub deg_per_count: f64,
    /// Centimetres of mousepad for a 360° turn: the number players actually
    /// compare sensitivities with. `None` when it cannot be computed (no CPI,
    /// or a degrees-per-count of zero); uncalibrated when
    /// `aim_profile_missing`, because it is derived from the same fallback.
    pub cm_per_360: Option<f64>,
    pub aim_profile_missing: bool,
}

/// `360 / deg_per_count` counts of travel, converted to centimetres.
fn cm_per_360(deg_per_count: f64, cpi: f64) -> Option<f64> {
    (deg_per_count.is_finite() && deg_per_count > 0.0 && cpi.is_finite() && cpi > 0.0)
        .then(|| 360.0 / deg_per_count / cpi * 2.54)
}

/// Every metric group over one recording, with its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: String,
    /// Version of `telemouse-analyze` that produced this document.
    pub analyzer_version: String,
    /// When it was produced, UTC µs.
    pub generated_utc_us: i64,
    /// Wall-clock cost of [`build`], milliseconds.
    pub compute_ms: f64,
    /// Wall-clock cost of reading the recording off disk, milliseconds —
    /// measured by the loader, because on a 570 MB recording it is the larger
    /// half of the wait and used to be invisible.
    pub load_ms: f64,
    /// `load_ms + compute_ms`: what the user actually waited for.
    pub total_ms: f64,
    /// Identity of the recording this was computed from, stamped when the
    /// report is published to a cache directory. [`crate::trend`] refuses a
    /// cached report whose recording no longer matches this — mtime alone
    /// cannot tell a restored backup from the file it was computed from.
    #[serde(default)]
    pub recording_signature: Option<crate::load::FileSignature>,
    /// Grid geometry the metrics were computed on.
    pub grid_cells: usize,
    /// Cells the sparse grid actually materialized.
    pub grid_stored_cells: usize,
    pub grid_dt_us: i64,

    pub params: Params,
    pub session: SessionSummary,
    pub quality: QualityReport,
    pub kinematics: Kinematics,
    pub flicks: FlickReport,
    pub micro: MicroReport,
    pub clicks: ClickReport,
    pub lifts: LiftReport,
    pub markers: Vec<MarkerRow>,
    /// One entry per marker interval; always present, rendered on
    /// `--split-by-marker`.
    pub segments: Vec<SegmentReport>,
    pub per_second: Vec<SecondRow>,
    pub per_minute: Vec<MinuteRow>,
    /// Everything worth a second look, mirrored from the quality report.
    pub warnings: Vec<String>,
    /// Per-phase cost, for `--timing` and the benches.
    pub timings: Vec<PhaseTiming>,
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let started = std::time::Instant::now();
    let value = f();
    (value, started.elapsed().as_secs_f64() * 1000.0)
}

// Below this point, creating scoped OS threads costs as much as the work they
// save. Large reports benefit from coarse parallel groups without imposing a
// fixed startup penalty on short recordings and unit tests.
const PARALLEL_MIN_EVENTS: usize = 250_000;
const PARALLEL_MIN_CPUS: usize = 4;

fn parallel_worthwhile(events: usize, cpus: usize) -> bool {
    events >= PARALLEL_MIN_EVENTS && cpus >= PARALLEL_MIN_CPUS
}

/// Run `a` and `b`, on a scoped worker and this thread when `parallel`, one
/// after the other otherwise. Results do not depend on which: every metric is
/// a pure function of the prepared series. A worker's panic is re-raised here.
fn fork<A: Send, B>(parallel: bool, a: impl FnOnce() -> A + Send, b: impl FnOnce() -> B) -> (A, B) {
    if !parallel {
        let a = a();
        return (a, b());
    }
    std::thread::scope(|scope| {
        let worker = scope.spawn(a);
        let b = b();
        let a = worker
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        (a, b)
    })
}

/// Every metric group's result, and what each cost.
struct MetricResults {
    quality: (QualityReport, Vec<String>),
    kin: Kinematics,
    fl: FlickReport,
    mi: MicroReport,
    cl: ClickReport,
    lift: LiftReport,
    marker_rows: Vec<MarkerRow>,
    segments: Vec<SegmentReport>,
    rows: Vec<SecondRow>,
    minutes: Vec<MinuteRow>,
    costs: MetricCosts,
}

#[derive(Default)]
struct MetricCosts {
    quality: f64,
    kinematics: f64,
    flicks: f64,
    micro: f64,
    clicks: f64,
    lifts: f64,
    markers: f64,
    per_second: f64,
    per_minute: f64,
}

/// The metric phases. The schedule is the same either way; `parallel` only
/// decides whether each `fork` runs its two sides concurrently:
///
/// * quality and kinematics, the two longest independent groups, run beside
///   everything else — kinematics is joined last, so the dependent
///   aggregation chain overlaps nearly all of it;
/// * micro runs beside the three short event-stream passes (flicks, clicks,
///   lifts);
/// * then the marker, per-second and per-minute aggregation, which needs the
///   flicks, the clicks and the tremor series.
fn compute_metrics(p: &Prepared, parallel: bool) -> MetricResults {
    let (((quality, quality_ms), (kin, kinematics_ms)), rest) = fork(
        parallel,
        || {
            fork(
                parallel,
                || timed(|| quality::compute_with_warnings(p)),
                || timed(|| kinematics::compute(p)),
            )
        },
        || {
            let (((mi, tremor), micro_ms), ((fl, flicks_ms), (cl, clicks_ms), (lift, lifts_ms))) =
                fork(
                    parallel,
                    || timed(|| micro::compute_full(p)),
                    || {
                        (
                            timed(|| flicks::compute(p)),
                            timed(|| clicks::compute(p)),
                            timed(|| lifts::compute(p)),
                        )
                    },
                );
            let ((marker_rows, intervals), marker_setup_ms) = timed(|| {
                let marker_rows = markers::rows(p);
                let intervals = markers::intervals(p, &marker_rows);
                (marker_rows, intervals)
            });
            let ((segments, segment_ms), ((rows, per_second_ms), (minutes, per_minute_ms))) = fork(
                parallel,
                || timed(|| markers::segment_reports(p, &fl.flicks, &cl, &tremor, &intervals)),
                || {
                    let rows = timed(|| per_second::compute(p, &fl.flicks, &intervals));
                    let minutes = timed(|| per_minute::compute(p, &rows.0, &fl.flicks, &tremor));
                    (rows, minutes)
                },
            );
            let costs = MetricCosts {
                micro: micro_ms,
                flicks: flicks_ms,
                clicks: clicks_ms,
                lifts: lifts_ms,
                markers: marker_setup_ms + segment_ms,
                per_second: per_second_ms,
                per_minute: per_minute_ms,
                ..Default::default()
            };
            (
                mi,
                fl,
                cl,
                lift,
                marker_rows,
                segments,
                rows,
                minutes,
                costs,
            )
        },
    );
    let (mi, fl, cl, lift, marker_rows, segments, rows, minutes, costs) = rest;
    MetricResults {
        quality,
        kin,
        fl,
        mi,
        cl,
        lift,
        marker_rows,
        segments,
        rows,
        minutes,
        costs: MetricCosts {
            quality: quality_ms,
            kinematics: kinematics_ms,
            ..costs
        },
    }
}

/// Run every metric group over a loaded recording.
///
/// Takes the session by value — [`prepare`] moves the event vector into the
/// prepared series rather than copying it. Fails only when `params` does not
/// validate.
pub fn build(session: LoadedSession, params: Params) -> Result<Report, ParamsError> {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1);
    let parallel = parallel_worthwhile(session.events.len(), cpus);
    build_with_parallelism(session, params, parallel)
}

/// Record one phase's cost for `--timing` and log it with its headline
/// numbers: `phase!(timings, "name", ms, "log message", field = value, ...)`.
macro_rules! phase {
    ($timings:expr, $name:literal, $ms:expr, $msg:literal $(, $k:ident = $v:expr)* $(,)?) => {{
        let ms: f64 = $ms;
        $timings.push(PhaseTiming {
            phase: $name.to_string(),
            ms,
        });
        tracing::info!($($k = $v,)* elapsed_ms = format_args!("{ms:.1}"), $msg);
    }};
}

fn build_with_parallelism(
    session: LoadedSession,
    params: Params,
    parallel: bool,
) -> Result<Report, ParamsError> {
    let started = std::time::Instant::now();
    let mut timings: Vec<PhaseTiming> = Vec::new();
    // The load happened before `build` was called; the loader timed it, and
    // the timing table is the one place a user compares the two.
    let load_ms = session.load_ms;
    timings.push(PhaseTiming {
        phase: "load".to_string(),
        ms: load_ms,
    });
    let (p, prepare_ms) = timed(|| prepare(session, params));
    let p = p?;
    phase!(
        timings,
        "prepare",
        prepare_ms,
        "prepared series",
        events = p.events().len(),
        grid_cells = p.grid.len(),
        grid_stored = p.grid.stored_cells(),
        duration_s = format_args!("{:.1}", p.duration_s),
    );

    let MetricResults {
        quality: (quality, warnings),
        kin,
        fl,
        mi,
        cl,
        lift,
        marker_rows,
        segments,
        rows,
        minutes,
        costs,
    } = compute_metrics(&p, parallel);

    // The warnings came back with the quality report, formatted once; the
    // same list sets `quality.clean`, is logged here and goes in the report.
    for w in &warnings {
        tracing::warn!("{w}");
    }
    phase!(timings, "quality", costs.quality, "data quality");
    phase!(
        timings,
        "kinematics",
        costs.kinematics,
        "kinematics",
        segments = kin.segment_count,
        distance_m = format_args!("{:.2}", kin.total_distance_m),
    );
    phase!(
        timings,
        "flicks",
        costs.flicks,
        "detected flicks",
        flicks = fl.count,
        median_amplitude_deg = format_args!("{:.1}", fl.amplitude_deg.median),
    );
    phase!(
        timings,
        "micro",
        costs.micro,
        "sub-movements and tremor",
        corrections = mi.total_corrections,
        blocks = mi.analyzed_blocks,
        band_ratio_8_12 = format_args!("{:.2}", mi.band_ratio_8_12),
    );
    phase!(
        timings,
        "clicks",
        costs.clicks,
        "trigger discipline",
        clicks = cl.total_clicks
    );
    phase!(
        timings,
        "lifts",
        costs.lifts,
        "repositioning lifts",
        lifts = lift.count
    );
    phase!(
        timings,
        "markers",
        costs.markers,
        "marker segmentation",
        markers = marker_rows.len(),
        intervals = segments.len(),
    );
    phase!(
        timings,
        "per_second",
        costs.per_second,
        "per-second aggregates",
        rows = rows.len()
    );
    phase!(
        timings,
        "per_minute",
        costs.per_minute,
        "per-minute aggregates",
        rows = minutes.len()
    );

    let compute_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(Report {
        schema: SCHEMA.to_string(),
        analyzer_version: ANALYZER_VERSION.to_string(),
        generated_utc_us: now_utc_us(),
        compute_ms,
        load_ms,
        total_ms: load_ms + compute_ms,
        recording_signature: None,
        grid_cells: p.grid.len(),
        grid_stored_cells: p.grid.stored_cells(),
        grid_dt_us: p.grid_dt_us,
        params,
        session: session_summary(&p),
        quality,
        kinematics: kin,
        flicks: fl,
        micro: mi,
        clicks: cl,
        lifts: lift,
        markers: marker_rows,
        segments,
        per_second: rows,
        per_minute: minutes,
        warnings,
        timings,
    })
}

/// The [`SessionSummary`] of a prepared recording.
fn session_summary(p: &Prepared) -> SessionSummary {
    let c = &p.session.config;
    SessionSummary {
        session_id: c.session_id.clone(),
        path: p.session.path.display().to_string(),
        started_utc_us: c.started_utc_us,
        started_utc: format_utc_us(c.started_utc_us),
        duration_s: p.duration_s,
        analysis_duration_s: p.analysis_duration_s,
        event_count: p.events().len(),
        batch_count: p.session.batches.len(),
        marker_count: p.session.markers.len(),
        mouse_cpi: c.mouse_cpi,
        capture_version: c.capture_version.clone(),
        game: p.game.clone(),
        sens: p.aim.sens,
        yaw_coeff: p.aim.yaw_coeff,
        pitch_coeff: p.aim.pitch_coeff,
        deg_per_count: p.aim_scale().0,
        cm_per_360: cm_per_360(p.aim_scale().0, c.mouse_cpi),
        aim_profile_missing: p.aim_fallback,
    }
}

impl Report {
    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// The pretty JSON document and a trailing newline, streamed into `w`.
    pub(crate) fn write_json_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut *w, self).map_err(io::Error::other)?;
        w.write_all(b"\n")
    }

    /// Stream the JSON document into `path`, creating parent directories.
    /// A three-hour session's report is tens of megabytes of `per_second`
    /// rows; `to_string` then `write` holds two copies of that in memory for
    /// no reason.
    pub fn write_json(&self, path: &Path) -> Result<(), AnalyzeError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(AnalyzeError::io("creating", parent))?;
        }
        let file = std::fs::File::create(path).map_err(AnalyzeError::io("creating", path))?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        self.write_json_to(&mut w)
            .and_then(|()| w.flush())
            .map_err(AnalyzeError::io("writing", path))
    }

    /// Record a phase timed outside [`build`] (render, JSON, CSV).
    pub fn push_timing(&mut self, phase: &str, ms: f64) {
        self.timings.push(PhaseTiming {
            phase: phase.to_string(),
            ms,
        });
    }

    /// Bring the sidecar-derived fields up to date with `meta`: the quality
    /// report's sidecar summary and verdict, and the warnings list. See
    /// [`QualityReport::set_sidecar`] for why a cached report needs this.
    pub fn refresh_sidecar(&mut self, meta: Option<&SessionMeta>) {
        self.warnings = self.quality.set_sidecar(meta);
    }

    /// Write the derived tables into `dir`, creating it first.
    pub fn write_csvs(&self, dir: &Path) -> io::Result<Vec<std::path::PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let mut out = Vec::new();

        let a = dir.join("per_second.csv");
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&a)?);
        per_second::write_csv(&mut w, &self.per_second)?;
        w.flush()?;
        out.push(a);

        let b = dir.join("flicks.csv");
        let mut w = BufWriter::with_capacity(1 << 16, std::fs::File::create(&b)?);
        per_second::write_flicks_csv(&mut w, &self.flicks.flicks)?;
        w.flush()?;
        out.push(b);

        let c = dir.join("per_minute.csv");
        let mut w = BufWriter::with_capacity(1 << 16, std::fs::File::create(&c)?);
        per_minute::write_csv(&mut w, &self.per_minute)?;
        w.flush()?;
        out.push(c);

        Ok(out)
    }
}
