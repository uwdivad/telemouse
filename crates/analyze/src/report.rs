//! Assembling every metric group into one report, and rendering it.
//!
//! Three renderings share one structure: the terminal summary (the user-facing
//! deliverable of the analysis phase), the full JSON document, and the CSVs
//! that make up the plan's derived tables.
//!
//! The JSON carries its own provenance — analyzer version, generation time,
//! compute cost, grid geometry — because these reports get cached to disk and
//! compared across weeks, and a number without the code version that produced
//! it is not comparable to anything.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::clicks::{self, ClickReport};
use crate::flicks::{self, FlickReport};
use crate::kinematics::{self, Kinematics};
use crate::lifts::{self, LiftReport};
use crate::load::LoadedSession;
use crate::markers::{self, MarkerRow, SegmentReport};
use crate::micro::{self, MicroReport};
use crate::per_minute::{self, MinuteRow};
use crate::per_second::{self, SecondRow};
use crate::quality::{self, QualityReport};
use crate::series::{Params, Prepared, prepare};
use crate::stats::Summary;
use crate::timefmt::{format_duration, format_utc_us};
use telemouse_core::now_utc_us;

/// Bumped whenever the JSON shape changes. `/3` added the load timing, the
/// polling-rate estimate, the sidecar summary, cm/360 and the unparseable-line
/// detail; every addition is a new field, but a cached `/2` document has none
/// of them, so it is recomputed rather than deserialized with defaults.
pub const SCHEMA: &str = "telemouse-analyze/3";
/// The build that produced a report. Cached reports are recomputed when this
/// no longer matches.
pub const ANALYZER_VERSION: &str = env!("CARGO_PKG_VERSION");

const WIDTH: usize = 78;
const LABEL: usize = 28;

/// Milliseconds spent in one phase of the pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTiming {
    pub phase: String,
    pub ms: f64,
}

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

/// The headline numbers of a [`Report`], small enough to paste into a chat
/// or hand to a tool: `report --summary`. Every field is a projection of the
/// full report — nothing here is computed differently — plus the per-sink
/// losses from the sidecar, which the report itself does not carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSummary {
    pub schema: String,
    pub analyzer_version: String,
    pub session: SessionSummary,
    pub quality: QualityHeadline,
    pub flicks: FlickHeadline,
    pub micro: MicroHeadline,
    pub clicks: ClickHeadline,
    pub kinematics: KinematicsHeadline,
    pub lifts: LiftHeadline,
    /// The markers, oldest first, at most [`MAX_SUMMARY_MARKERS`] of them —
    /// without their labels an experiment's "sens A" / "sens B" stretches
    /// cannot be told apart without opening the full report.
    pub markers: Vec<SummaryMarker>,
    /// How many markers the recording holds; larger than `markers.len()`
    /// when the list was truncated.
    pub markers_total: usize,
    /// One entry per marker interval, oldest first, at most
    /// [`MAX_SUMMARY_MARKERS`] of them. Empty when the recording has no
    /// markers: the single interval would only repeat the headline numbers.
    pub segments: Vec<SummarySegment>,
    /// How many intervals the markers cut the session into; larger than
    /// `segments.len()` when the list was truncated.
    pub segments_total: usize,
    pub warnings: Vec<String>,
}

/// One marker, placed on the analysis timeline. A projection of
/// [`MarkerRow`] without the absolute timestamp — the summary's reader wants
/// "which stretch is this", not a wall clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryMarker {
    /// Seconds from the start of the session.
    pub t_s: f64,
    pub label: String,
}

/// The few numbers of a [`SegmentReport`] worth comparing between two
/// marked stretches, with the labels that bound the stretch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummarySegment {
    pub index: usize,
    /// The marker that opened this stretch; empty before the first marker.
    pub label: String,
    /// The marker that closes it; empty for the stretch that runs to the end
    /// of the session.
    pub next_label: String,
    pub t_start_s: f64,
    pub t_end_s: f64,
    pub flicks: usize,
    pub flicks_per_min: f64,
    pub overshoot_median: f64,
    pub settle_median_ms: f64,
    pub tremor_rms_counts_s: f64,
    pub path_efficiency: f64,
    pub clicks_per_min: f64,
}

/// How many markers, and how many marker intervals, the summary lists before
/// it stops. The point of the summary is that it stays a few KB whatever the
/// recording holds; a session marked once a round would otherwise carry
/// hundreds of rows. `markers_total` / `segments_total` say what was cut.
pub const MAX_SUMMARY_MARKERS: usize = 12;

/// Longest label the summary keeps, in characters. The capture agent and the
/// panel both refuse a longer one (`MAX_MARKER_LABEL_CHARS`), so this only
/// bites on a hand-written recording.
pub const MAX_SUMMARY_LABEL_CHARS: usize = 120;

/// The label as the summary carries it: at most [`MAX_SUMMARY_LABEL_CHARS`]
/// characters, with an ellipsis when something was cut.
fn clip_label(label: &str) -> String {
    match label
        .char_indices()
        .nth(MAX_SUMMARY_LABEL_CHARS)
        .map(|(i, _)| i)
    {
        Some(cut) => format!("{}…", &label[..cut]),
        None => label.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityHeadline {
    pub events: usize,
    pub ring_drops: u64,
    pub lost_batches: u64,
    pub seq_gaps: usize,
    pub monotonicity_violations: usize,
    pub bad_lines: usize,
    pub pct_within_1ms: f64,
    pub median_interval_ms: f64,
    pub p99_interval_ms: f64,
    pub gaps_over_10ms: usize,
    pub poll_hz: Option<f64>,
    pub locked_fraction: f64,
    /// No data-quality warning fired: `warnings` is empty.
    pub clean: bool,
    /// The sidecar's own flag: every capture thread joined without
    /// panicking. `None` without a sidecar.
    pub threads_clean: Option<bool>,
    /// The sidecar's exit reason, when there is a sidecar.
    pub exit: Option<String>,
    /// The run had not finished when the sidecar was last written.
    pub unfinished: bool,
    pub capture_profile: Option<String>,
    /// Envelopes each sink failed to deliver, `[["kafka", 400], ...]`.
    pub sink_losses: Vec<(String, u64)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlickHeadline {
    pub count: usize,
    pub per_minute: f64,
    pub amplitude_deg_median: f64,
    pub peak_velocity_deg_s_median: f64,
    pub duration_ms_median: f64,
    pub overshoot_ratio_median: f64,
    pub overshoot_ratio_p90: f64,
    pub settle_ms_median: f64,
    pub settle_ms_p90: f64,
    pub time_to_click_ms_median: f64,
    pub clicked_fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicroHeadline {
    pub total_corrections: usize,
    pub clean_segment_fraction: f64,
    pub tremor_rms_counts_s: f64,
    pub tremor_rms_cm_s: f64,
    pub band_ratio_8_12: f64,
    pub dominant_hz: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickHeadline {
    pub total: usize,
    pub per_minute: f64,
    pub still_click_fraction: f64,
    pub click_to_still_ms_median: f64,
    pub hold_ms_median: f64,
    pub double_clicks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KinematicsHeadline {
    pub total_distance_m: f64,
    pub distance_cm_per_min: f64,
    pub moving_fraction: f64,
    pub path_efficiency_weighted: f64,
    pub speed_cm_per_s_median: f64,
    pub speed_cm_per_s_p99: f64,
    pub speed_deg_per_s_p99: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiftHeadline {
    pub count: usize,
    pub per_minute: f64,
    pub mean_drift_cm: f64,
}

/// Schema tag of [`ReportSummary`]; bump when a field changes meaning — and
/// when a field is *added*, because the control panel keeps a
/// `<id>.summary.json` per recording and a recording never changes after its
/// run, so the tag is the only thing that can retire a cache written before
/// the field existed. `/2` added `markers`, `markers_total`, `segments` and
/// `segments_total`.
pub const SUMMARY_SCHEMA: &str = "telemouse-report-summary/2";

/// Times one phase, logs it, and records it for the `--timing` table.
struct Phases {
    started: std::time::Instant,
    out: Vec<PhaseTiming>,
}

impl Phases {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            out: Vec::new(),
        }
    }

    fn record(&mut self, phase: &str, ms: f64) {
        self.out.push(PhaseTiming {
            phase: phase.to_string(),
            ms,
        });
    }

    fn total_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
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

struct MetricResults {
    quality: QualityReport,
    warnings: Vec<String>,
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

fn compute_metrics_sequential(p: &Prepared) -> MetricResults {
    let ((quality, warnings), quality_ms) = timed(|| {
        let quality = quality::compute(p);
        let warnings = quality.warnings();
        (quality, warnings)
    });
    let (kin, kinematics_ms) = timed(|| kinematics::compute(p));
    let (fl, flicks_ms) = timed(|| flicks::compute(p));
    let ((mi, tremor), micro_ms) = timed(|| micro::compute_full(p));
    let (cl, clicks_ms) = timed(|| clicks::compute(p));
    let (lift, lifts_ms) = timed(|| lifts::compute(p));
    let ((marker_rows, intervals), marker_setup_ms) = timed(|| {
        let marker_rows = markers::rows(p);
        let intervals = markers::intervals(p, &marker_rows);
        (marker_rows, intervals)
    });
    let (segments, segment_ms) =
        timed(|| markers::segment_reports(p, &fl.flicks, &cl, &tremor, &intervals));
    let (rows, per_second_ms) = timed(|| per_second::compute(p, &fl.flicks, &intervals));
    let (minutes, per_minute_ms) = timed(|| per_minute::compute(p, &rows, &fl.flicks, &tremor));

    MetricResults {
        quality,
        warnings,
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
            flicks: flicks_ms,
            micro: micro_ms,
            clicks: clicks_ms,
            lifts: lifts_ms,
            markers: marker_setup_ms + segment_ms,
            per_second: per_second_ms,
            per_minute: per_minute_ms,
        },
    }
}

fn compute_metrics_parallel(p: &Prepared) -> MetricResults {
    std::thread::scope(|scope| {
        let quality = scope.spawn(|| {
            timed(|| {
                let quality = quality::compute(p);
                let warnings = quality.warnings();
                (quality, warnings)
            })
        });
        let kin = scope.spawn(|| timed(|| kinematics::compute(p)));
        let micro = scope.spawn(|| timed(|| micro::compute_full(p)));

        // These three short event-stream passes share one worker (the scope
        // thread) while the larger grid/quality groups run alongside it.
        let (fl, flicks_ms) = timed(|| flicks::compute(p));
        let (cl, clicks_ms) = timed(|| clicks::compute(p));
        let (lift, lifts_ms) = timed(|| lifts::compute(p));
        let ((mi, tremor), micro_ms) = micro.join().expect("micro worker panicked");

        let ((marker_rows, intervals), marker_setup_ms) = timed(|| {
            let marker_rows = markers::rows(p);
            let intervals = markers::intervals(p, &marker_rows);
            (marker_rows, intervals)
        });
        let ((segments, segment_ms), (rows, per_second_ms), (minutes, per_minute_ms)) =
            std::thread::scope(|aggregate_scope| {
                let segments = aggregate_scope.spawn(|| {
                    timed(|| markers::segment_reports(p, &fl.flicks, &cl, &tremor, &intervals))
                });
                let (rows, per_second_ms) =
                    timed(|| per_second::compute(p, &fl.flicks, &intervals));
                let minutes = timed(|| per_minute::compute(p, &rows, &fl.flicks, &tremor));
                (
                    segments.join().expect("marker worker panicked"),
                    (rows, per_second_ms),
                    minutes,
                )
            });

        // Kinematics is the longest independent phase, so joining it last
        // lets the dependent aggregation chain overlap nearly all of its work.
        let ((quality, warnings), quality_ms) = quality.join().expect("quality worker panicked");
        let (kin, kinematics_ms) = kin.join().expect("kinematics worker panicked");

        MetricResults {
            quality,
            warnings,
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
                flicks: flicks_ms,
                micro: micro_ms,
                clicks: clicks_ms,
                lifts: lifts_ms,
                markers: marker_setup_ms + segment_ms,
                per_second: per_second_ms,
                per_minute: per_minute_ms,
            },
        }
    })
}

/// Run every metric group over a loaded recording.
///
/// Takes the session by value — [`prepare`] moves the event vector into the
/// prepared series rather than copying it.
pub fn build(session: LoadedSession, params: Params) -> Report {
    let cpus = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1);
    let parallel = parallel_worthwhile(session.events.len(), cpus);
    build_with_parallelism(session, params, parallel)
}

fn build_with_parallelism(session: LoadedSession, params: Params, parallel: bool) -> Report {
    let mut t = Phases::new();
    // The load happened before `build` was called; the loader timed it, and
    // the timing table is the one place a user compares the two.
    let load_ms = session.load_ms;
    t.record("load", load_ms);
    let (p, ms) = timed(|| prepare(session, params));
    t.record("prepare", ms);
    tracing::info!(
        events = p.events().len(),
        grid_cells = p.grid.len(),
        grid_stored = p.grid.stored_cells(),
        duration_s = format_args!("{:.1}", p.duration_s),
        elapsed_ms = format_args!("{ms:.1}"),
        "prepared series"
    );

    let MetricResults {
        quality,
        warnings,
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
    } = if parallel {
        compute_metrics_parallel(&p)
    } else {
        compute_metrics_sequential(&p)
    };

    // Computed once with quality: `warnings()` walks and formats every check,
    // and the old code called it twice — once to log, once for the report.
    for w in &warnings {
        tracing::warn!("{w}");
    }
    t.record("quality", costs.quality);
    tracing::info!(
        elapsed_ms = format_args!("{:.1}", costs.quality),
        "data quality"
    );

    t.record("kinematics", costs.kinematics);
    tracing::info!(
        segments = kin.segment_count,
        distance_m = format_args!("{:.2}", kin.total_distance_m),
        elapsed_ms = format_args!("{:.1}", costs.kinematics),
        "kinematics"
    );

    t.record("flicks", costs.flicks);
    tracing::info!(
        flicks = fl.count,
        median_amplitude_deg = format_args!("{:.1}", fl.amplitude_deg.median),
        elapsed_ms = format_args!("{:.1}", costs.flicks),
        "detected flicks"
    );

    t.record("micro", costs.micro);
    tracing::info!(
        corrections = mi.total_corrections,
        blocks = mi.analyzed_blocks,
        band_ratio_8_12 = format_args!("{:.2}", mi.band_ratio_8_12),
        elapsed_ms = format_args!("{:.1}", costs.micro),
        "sub-movements and tremor"
    );

    t.record("clicks", costs.clicks);
    tracing::info!(
        clicks = cl.total_clicks,
        elapsed_ms = format_args!("{:.1}", costs.clicks),
        "trigger discipline"
    );

    t.record("lifts", costs.lifts);
    tracing::info!(
        lifts = lift.count,
        elapsed_ms = format_args!("{:.1}", costs.lifts),
        "repositioning lifts"
    );

    t.record("markers", costs.markers);
    tracing::info!(
        markers = marker_rows.len(),
        intervals = segments.len(),
        elapsed_ms = format_args!("{:.1}", costs.markers),
        "marker segmentation"
    );

    t.record("per_second", costs.per_second);
    tracing::info!(
        rows = rows.len(),
        elapsed_ms = format_args!("{:.1}", costs.per_second),
        "per-second aggregates"
    );

    t.record("per_minute", costs.per_minute);
    tracing::info!(
        rows = minutes.len(),
        elapsed_ms = format_args!("{:.1}", costs.per_minute),
        "per-minute aggregates"
    );

    let compute_ms = t.total_ms();
    Report {
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
        session: summary(&p),
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
        timings: t.out,
    }
}

fn summary(p: &Prepared) -> SessionSummary {
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
    /// The headline numbers, see [`ReportSummary`]. `sink_losses` comes from
    /// the sidecar (`load::read_sidecar(..).losses()`), which the report does
    /// not carry; pass an empty list when there is none.
    pub fn summary(&self, sink_losses: Vec<(String, u64)>) -> ReportSummary {
        let q = &self.quality;
        let sc = q.sidecar.as_ref();
        ReportSummary {
            schema: SUMMARY_SCHEMA.to_string(),
            analyzer_version: self.analyzer_version.clone(),
            session: self.session.clone(),
            quality: QualityHeadline {
                events: q.event_count,
                ring_drops: q.ring_drops,
                lost_batches: q.lost_batches,
                seq_gaps: q.seq_gaps,
                monotonicity_violations: q.monotonicity_violations,
                bad_lines: q.bad_lines,
                pct_within_1ms: q.pct_within_1ms,
                median_interval_ms: q.median_interval_ms,
                p99_interval_ms: q.p99_interval_ms,
                gaps_over_10ms: q.gaps_over_10ms,
                poll_hz: q.polling.hz,
                locked_fraction: q.locked_fraction,
                clean: q.clean,
                threads_clean: sc.map(|s| s.clean),
                exit: sc.map(|s| s.exit.clone()),
                unfinished: sc.is_some_and(|s| s.unfinished),
                capture_profile: sc.map(|s| s.capture_profile.clone()),
                sink_losses,
            },
            flicks: FlickHeadline {
                count: self.flicks.count,
                per_minute: self.flicks.per_minute,
                amplitude_deg_median: self.flicks.amplitude_deg.median,
                peak_velocity_deg_s_median: self.flicks.peak_velocity_deg_s.median,
                duration_ms_median: self.flicks.duration_ms.median,
                overshoot_ratio_median: self.flicks.overshoot_ratio.median,
                overshoot_ratio_p90: self.flicks.overshoot_ratio.p90,
                settle_ms_median: self.flicks.settle_ms.median,
                settle_ms_p90: self.flicks.settle_ms.p90,
                time_to_click_ms_median: self.flicks.time_to_click_ms.median,
                clicked_fraction: self.flicks.clicked_fraction,
            },
            micro: MicroHeadline {
                total_corrections: self.micro.total_corrections,
                clean_segment_fraction: self.micro.clean_segment_fraction,
                tremor_rms_counts_s: self.micro.tremor_rms_counts_s,
                tremor_rms_cm_s: self.micro.tremor_rms_cm_s,
                band_ratio_8_12: self.micro.band_ratio_8_12,
                dominant_hz: self.micro.dominant_hz,
            },
            clicks: ClickHeadline {
                total: self.clicks.total_clicks,
                per_minute: self.clicks.clicks_per_min,
                still_click_fraction: self.clicks.still_click_fraction,
                click_to_still_ms_median: self.clicks.click_to_still_ms.median,
                hold_ms_median: self.clicks.hold_ms.median,
                double_clicks: self.clicks.double_clicks,
            },
            kinematics: KinematicsHeadline {
                total_distance_m: self.kinematics.total_distance_m,
                distance_cm_per_min: self.kinematics.distance_cm_per_min,
                moving_fraction: self.kinematics.moving_fraction,
                path_efficiency_weighted: self.kinematics.path_efficiency_weighted,
                speed_cm_per_s_median: self.kinematics.speed_cm_per_s.median,
                speed_cm_per_s_p99: self.kinematics.speed_cm_per_s.p99,
                speed_deg_per_s_p99: self.kinematics.speed_deg_per_s.p99,
            },
            lifts: LiftHeadline {
                count: self.lifts.count,
                per_minute: self.lifts.per_minute,
                mean_drift_cm: self.lifts.mean_drift_cm,
            },
            markers: self
                .markers
                .iter()
                .take(MAX_SUMMARY_MARKERS)
                .map(|m| SummaryMarker {
                    t_s: m.t_s,
                    label: clip_label(&m.label),
                })
                .collect(),
            markers_total: self.markers.len(),
            segments: self.summary_segments(),
            segments_total: if self.markers.is_empty() {
                0
            } else {
                self.segments.len()
            },
            warnings: self.warnings.clone(),
        }
    }

    /// The per-interval rows of the summary. An unmarked session has exactly
    /// one interval covering everything, which would only repeat the
    /// headline numbers, so it contributes nothing.
    fn summary_segments(&self) -> Vec<SummarySegment> {
        if self.markers.is_empty() {
            return Vec::new();
        }
        self.segments
            .iter()
            .take(MAX_SUMMARY_MARKERS)
            .enumerate()
            .map(|(i, s)| SummarySegment {
                index: s.index,
                label: clip_label(&s.label),
                // The marker that ends this stretch, so "sens A" is readable
                // as the span between two labels rather than a start time.
                next_label: self
                    .segments
                    .get(i + 1)
                    .map_or_else(String::new, |n| clip_label(&n.label)),
                t_start_s: s.t_start_s,
                t_end_s: s.t_end_s,
                flicks: s.flicks,
                flicks_per_min: s.flicks_per_min,
                overshoot_median: s.overshoot_median,
                settle_median_ms: s.settle_median_ms,
                tremor_rms_counts_s: s.tremor_rms_counts_s,
                path_efficiency: s.path_efficiency,
                clicks_per_min: s.clicks_per_min,
            })
            .collect()
    }

    pub fn to_json_pretty(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Stream the JSON document into `path`, creating parent directories.
    /// A three-hour session's report is tens of megabytes of `per_second`
    /// rows; `to_string` then `write` holds two copies of that in memory for
    /// no reason.
    pub fn write_json(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        serde_json::to_writer_pretty(&mut w, self)?;
        w.write_all(b"\n")?;
        w.flush()?;
        Ok(())
    }

    /// Record a phase timed outside [`build`] (render, JSON, CSV).
    pub fn push_timing(&mut self, phase: &str, ms: f64) {
        self.timings.push(PhaseTiming {
            phase: phase.to_string(),
            ms,
        });
    }

    /// The `--timing` table.
    pub fn timing_table(&self) -> String {
        let mut o = String::new();
        section(&mut o, "Timing");
        // Phase durations are measured inside their workers. Their sum can be
        // larger than build wall time when large reports run in parallel.
        let total: f64 = self.timings.iter().map(|t| t.ms).sum();
        for t in &self.timings {
            let share = if total > 0.0 { t.ms / total } else { 0.0 };
            o.push_str(&format!(
                "  {:<16} {:>9.1} ms  {}{} {:>5.1}%\n",
                t.phase,
                t.ms,
                "█".repeat(((share * 20.0).round() as usize).min(20)),
                "·".repeat(20 - ((share * 20.0).round() as usize).min(20)),
                share * 100.0
            ));
        }
        o.push_str(&format!(
            "  {:<16} {:>9.1} ms\n",
            "phase elapsed sum", total
        ));
        o.push_str(&format!(
            "  {:<16} {:>9.1} ms\n",
            "build wall", self.compute_ms
        ));
        // What the user waited for: the load is not part of build wall, and on
        // a large recording it is the larger half.
        o.push_str(&format!(
            "  {:<16} {:>9.1} ms\n",
            "load + build", self.total_ms
        ));
        o
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

    /// The terminal summary.
    pub fn render(&self) -> String {
        let mut o = String::new();
        self.render_header(&mut o);
        self.render_quality(&mut o);
        self.render_kinematics(&mut o);
        self.render_flicks(&mut o);
        self.render_micro(&mut o);
        self.render_clicks(&mut o);
        self.render_lifts(&mut o);
        self.render_minutes(&mut o);
        if self.params.split_by_marker {
            self.render_segments(&mut o);
        }
        self.render_markers(&mut o);
        self.render_warnings(&mut o);
        o
    }

    fn render_header(&self, o: &mut String) {
        let s = &self.session;
        o.push_str(&format!("{}\n", "═".repeat(WIDTH)));
        o.push_str(&format!(" telemouse session  {}\n", s.session_id));
        o.push_str(&format!("{}\n", "═".repeat(WIDTH)));
        kv(o, "file", &s.path);
        kv(o, "started", &s.started_utc);
        kv(
            o,
            "duration",
            &if self.quality.grid_truncated {
                format!(
                    "{}   (analyzed {})",
                    format_duration(s.duration_s),
                    format_duration(s.analysis_duration_s)
                )
            } else {
                format_duration(s.duration_s)
            },
        );
        kv(
            o,
            "game",
            &match &s.game {
                Some(g) => format!("{g}  (sens {:.3} × {:.4})", s.sens, s.yaw_coeff),
                None => "unknown".to_string(),
            },
        );
        kv(
            o,
            "aim scale",
            &format!(
                "{:.5} °/count{}",
                s.deg_per_count,
                if s.aim_profile_missing {
                    "   [FALLBACK — uncalibrated]"
                } else {
                    ""
                }
            ),
        );
        kv(
            o,
            "mouse",
            &format!(
                "{:.0} CPI{}",
                s.mouse_cpi,
                match s.cm_per_360 {
                    Some(cm) => format!("   {cm:.1} cm/360°"),
                    None => String::new(),
                }
            ),
        );
        kv(
            o,
            "events",
            &format!(
                "{} in {} batches   ({:.1}/s)",
                commas(s.event_count as u64),
                commas(s.batch_count as u64),
                self.quality.events_per_s
            ),
        );
        kv(
            o,
            "analyzer",
            &format!(
                "v{}   {} grid cells ({} stored)   {:.0} ms load + {:.0} ms compute",
                self.analyzer_version,
                commas(self.grid_cells as u64),
                commas(self.grid_stored_cells as u64),
                self.load_ms,
                self.compute_ms
            ),
        );
    }

    fn render_quality(&self, o: &mut String) {
        let q = &self.quality;
        section(o, "Data quality");
        kv(
            o,
            "inter-event intervals",
            &format!(
                "median {:.2}ms   p99 {:.2}ms   {:.1}% of them ≤1ms",
                q.median_interval_ms, q.p99_interval_ms, q.pct_within_1ms
            ),
        );
        kv(
            o,
            "polling rate",
            &match q.polling.hz {
                Some(hz) => format!(
                    "{hz:.0} Hz   {:.0}% steady{}",
                    q.polling.stability * 100.0,
                    match (q.polling.hz_min, q.polling.hz_max) {
                        (Some(lo), Some(hi)) if hi - lo >= 1.0 =>
                            format!("   per minute {lo:.0}–{hi:.0} Hz"),
                        _ => String::new(),
                    }
                ),
                None => "—  (nothing moved)".to_string(),
            },
        );
        kv(
            o,
            "gaps >10ms",
            &format!(
                "{}   (max {:.1}ms)",
                commas(q.gaps_over_10ms as u64),
                q.max_interval_ms
            ),
        );
        kv(o, "ring drops", &flag(q.ring_drops));
        kv(o, "lost batches", &flag(q.lost_batches));
        kv(
            o,
            "monotonicity violations",
            &flag(q.monotonicity_violations as u64),
        );
        kv(
            o,
            "unparseable lines",
            &format!(
                "{}{}",
                flag(q.bad_lines as u64),
                match (q.bad_lines, q.bad_line_first, q.bad_line_last) {
                    (0, _, _) => String::new(),
                    (_, Some(a), Some(b)) => format!(
                        "   {} {}",
                        if a == b {
                            format!("line {a}")
                        } else {
                            format!("lines {a}–{b}")
                        },
                        if q.bad_lines_tail_only {
                            "(truncated tail)"
                        } else {
                            "(corrupt body)"
                        }
                    ),
                    _ => String::new(),
                }
            ),
        );
        kv(o, "absolute-motion frames", &flag(q.abs_frames));
        kv(
            o,
            "events per batch",
            &format!("{:.1}", q.mean_events_per_batch),
        );
        kv(
            o,
            "batch latency",
            &if q.batch_latency_ms.is_empty() {
                "—".to_string()
            } else {
                format!(
                    "p50 {:.1}ms   p99 {:.1}ms   max {:.1}ms",
                    q.batch_latency_ms.median, q.batch_latency_ms.p99, q.batch_latency_ms.max
                )
            },
        );
        kv(
            o,
            "pointer locked",
            &format!(
                "{:.1}% of events{}",
                q.locked_fraction * 100.0,
                if q.locked_only {
                    "   [degree metrics restricted to it]"
                } else {
                    ""
                }
            ),
        );
        kv(
            o,
            "foreground process",
            &match &q.dominant_game {
                Some(g) => format!("{g}   ({:.0}% of events)", q.dominant_game_share * 100.0),
                None => "unknown".to_string(),
            },
        );
        kv(
            o,
            "devices",
            &format!(
                "{} in config, {} seen{}",
                q.devices.len(),
                q.device_indices.len(),
                match q.anchor_uncertainty_us {
                    Some(u) => format!("   anchor ±{u}µs"),
                    None => String::new(),
                }
            ),
        );
        if let Some(s) = &q.sidecar {
            kv(
                o,
                "capture run",
                &format!(
                    "exit {}{}   {} build{}",
                    s.exit,
                    if s.unfinished {
                        "   ← unfinished: the run did not stop cleanly"
                    } else {
                        ""
                    },
                    if s.capture_profile.is_empty() {
                        "unknown"
                    } else {
                        &s.capture_profile
                    },
                    match s.poll_hz {
                        Some(hz) => format!("   mouse reported {hz:.0} Hz"),
                        None => String::new(),
                    }
                ),
            );
            kv(
                o,
                "events (agent vs file)",
                &if s.events == q.event_count as u64 {
                    format!("{}   (match)", commas(s.events))
                } else {
                    format!(
                        "{} vs {}   ← problem",
                        commas(s.events),
                        commas(q.event_count as u64)
                    )
                },
            );
        }
        o.push('\n');
        for b in &q.interval_histogram {
            if b.count == 0 {
                continue;
            }
            hist_row(o, &b.label, b.fraction, b.count);
        }
    }

    fn render_kinematics(&self, o: &mut String) {
        let k = &self.kinematics;
        section(o, "Kinematics");
        kv(
            o,
            "hand travel",
            &format!(
                "{:.2} m   ({:.1} cm/min)",
                k.total_distance_m, k.distance_cm_per_min
            ),
        );
        kv(
            o,
            "aim travel",
            &format!(
                "{:.0}°   ({:.0}°/min){}",
                k.total_distance_deg,
                k.distance_deg_per_min,
                if k.degrees_locked_only {
                    "   [locked only]"
                } else {
                    ""
                }
            ),
        );
        kv(
            o,
            "net turn",
            &format!(
                "yaw {:+.1}°   pitch {:+.1}°",
                k.net_yaw_deg, k.net_pitch_deg
            ),
        );
        kv(
            o,
            "moving",
            &format!(
                "{} ({:.1}% of session) in {} segments",
                format_duration(k.moving_time_s),
                k.moving_fraction * 100.0,
                commas(k.segment_count as u64)
            ),
        );
        o.push('\n');
        kv(o, "speed (cm/s)", &sum_line(&k.speed_cm_per_s, 1));
        kv(o, "speed (°/s)", &sum_line(&k.speed_deg_per_s, 0));
        kv(o, "speed (counts/s)", &sum_line(&k.speed_counts_per_s, 0));
        kv(o, "|accel| (cm/s²)", &sum_line(&k.accel_cm_per_s2, 0));
        kv(o, "|accel| (°/s²)", &sum_line(&k.accel_deg_per_s2, 0));
        kv(
            o,
            "|accel| (counts/s²)",
            &sum_line(&k.accel_counts_per_s2, 0),
        );
        kv(o, "|jerk| (cm/s³)", &sum_line(&k.jerk_cm_per_s3, 0));
        kv(o, "|jerk| (°/s³)", &sum_line(&k.jerk_deg_per_s3, 0));
        kv(o, "|jerk| (counts/s³)", &sum_line(&k.jerk_counts_per_s3, 0));
        o.push('\n');
        kv(
            o,
            "path efficiency",
            &format!(
                "{:.3} weighted   median {:.3}   p90 {:.3}",
                k.path_efficiency_weighted, k.path_efficiency.median, k.path_efficiency.p90
            ),
        );
    }

    fn render_flicks(&self, o: &mut String) {
        let f = &self.flicks;
        section(o, "Flicks");
        kv(
            o,
            "detector",
            &format!(
                ">{:.0} counts/s, settled at <{:.0} for {}ms",
                f.params.flick_speed_counts_s,
                f.params.still_speed_counts_s,
                f.params.still_hold_ms
            ),
        );
        kv(
            o,
            "detected",
            &format!("{}   ({:.1}/min)", commas(f.count as u64), f.per_minute),
        );
        if f.count == 0 {
            return;
        }
        kv(o, "amplitude (°)", &sum_line(&f.amplitude_deg, 1));
        kv(
            o,
            "peak velocity (°/s)",
            &sum_line(&f.peak_velocity_deg_s, 0),
        );
        kv(o, "duration (ms)", &sum_line(&f.duration_ms, 1));
        kv(o, "overshoot ratio", &sum_line(&f.overshoot_ratio, 3));
        kv(o, "settle (ms)", &sum_line(&f.settle_ms, 1));
        kv(
            o,
            "time to click (ms)",
            &format!(
                "{}   [{:.0}% of flicks]",
                sum_line(&f.time_to_click_ms, 1),
                f.clicked_fraction * 100.0
            ),
        );

        o.push('\n');
        o.push_str("   #      t(s)    amp°   peak°/s   dur ms   over   settle   click ms\n");
        for fl in f.flicks.iter().take(10) {
            o.push_str(&format!(
                "  {:>3}  {:>8.2}  {:>6.1}  {:>8.0}  {:>7.1}  {:>5.3}  {:>7.1}  {:>9}\n",
                fl.index,
                fl.t_start_s,
                fl.amplitude_deg,
                fl.peak_velocity_deg_s,
                fl.duration_ms,
                fl.overshoot_ratio,
                fl.settle_ms,
                fl.time_to_click_ms
                    .map_or("—".to_string(), |v| format!("{v:.0}")),
            ));
        }
        if f.count > 10 {
            o.push_str(&format!("       … and {} more\n", f.count - 10));
        }
    }

    fn render_micro(&self, o: &mut String) {
        let m = &self.micro;
        section(o, "Sub-movements & micro-control");
        kv(
            o,
            "corrections / segment",
            &format!(
                "median {:.1}   p90 {:.1}   max {:.0}   ({} total)",
                m.corrections_per_segment.median,
                m.corrections_per_segment.p90,
                m.corrections_per_segment.max,
                commas(m.total_corrections as u64)
            ),
        );
        kv(
            o,
            "clean segments",
            &format!("{:.1}%  (≤1 correction)", m.clean_segment_fraction * 100.0),
        );
        kv(
            o,
            "tremor RMS (high-passed)",
            &format!(
                "{:.1} cm/s   ({:.0} counts/s)   over {:.1}% of samples",
                m.tremor_rms_cm_s,
                m.tremor_rms_counts_s,
                m.tremor_sample_fraction * 100.0
            ),
        );
        if m.analyzed_blocks > 0 {
            kv(
                o,
                "8–12Hz band share",
                &format!(
                    "{:.1}%   peak at {:.0} Hz   ({} blocks @ {:.0}Hz)",
                    m.band_ratio_8_12 * 100.0,
                    m.dominant_hz,
                    m.analyzed_blocks,
                    m.spectrum_fs_hz
                ),
            );
        } else {
            kv(o, "8–12Hz band share", "n/a (session too short)");
        }
        o.push('\n');
        o.push_str("  micro-adjustment sizes (counts / degrees)\n");
        for b in &m.micro_adjustments {
            if b.count == 0 {
                continue;
            }
            let label = match b.hi_deg {
                Some(hi) => format!("{} ({:.1}–{:.1}°)", b.label, b.lo_deg, hi),
                None => format!("{} ({:.1}°+)", b.label, b.lo_deg),
            };
            hist_row(o, &label, b.fraction, b.count);
        }
    }

    fn render_clicks(&self, o: &mut String) {
        let c = &self.clicks;
        section(o, "Trigger discipline");
        kv(
            o,
            "clicks",
            &format!(
                "{}   ({:.1}/min)",
                commas(c.total_clicks as u64),
                c.clicks_per_min
            ),
        );
        if c.total_clicks == 0 {
            return;
        }
        kv(
            o,
            "pre-click speed (cm/s)",
            &sum_line(&c.pre_click_speed_cm_s, 2),
        );
        kv(
            o,
            "shots taken still",
            &format!("{:.1}%", c.still_click_fraction * 100.0),
        );
        kv(o, "click-to-still (ms)", &sum_line(&c.click_to_still_ms, 1));
        kv(o, "hold duration (ms)", &sum_line(&c.hold_ms, 1));
        kv(
            o,
            "double-click gap (ms)",
            &if c.double_clicks == 0 {
                "none".to_string()
            } else {
                format!(
                    "{}   ({} pairs)",
                    sum_line(&c.double_click_interval_ms, 1),
                    c.double_clicks
                )
            },
        );
        if c.unmatched_downs > 0 {
            kv(o, "unmatched downs", &commas(c.unmatched_downs as u64));
        }
        o.push('\n');
        o.push_str("  button      downs     ups   hold median ms\n");
        for b in &c.per_button {
            o.push_str(&format!(
                "  {:<10} {:>6}  {:>6}   {:>13.1}\n",
                b.button, b.downs, b.ups, b.hold_ms.median
            ));
        }
    }

    fn render_lifts(&self, o: &mut String) {
        let l = &self.lifts;
        section(o, "Repositioning lifts (inferred)");
        kv(
            o,
            "detected",
            &format!(
                "{}   ({:.2}/min)   mean drift {:.1} cm",
                commas(l.count as u64),
                l.per_minute,
                l.mean_drift_cm
            ),
        );
        if l.count == 0 {
            return;
        }
        o.push('\n');
        o.push_str("   #      t(s)   drift cm   drift ms   return cm   opposition\n");
        for x in l.lifts.iter().take(8) {
            o.push_str(&format!(
                "  {:>3}  {:>8.2}  {:>9.2}  {:>9.0}  {:>10.2}  {:>11.2}\n",
                x.index, x.t_start_s, x.drift_cm, x.drift_ms, x.return_cm, x.opposition
            ));
        }
        if l.count > 8 {
            o.push_str(&format!("       … and {} more\n", l.count - 8));
        }
    }

    fn render_minutes(&self, o: &mut String) {
        if self.per_minute.len() < 2 {
            return;
        }
        section(o, "Per minute (fatigue curve)");
        o.push_str("  min   dist cm   flicks   overshoot   settle ms   tremor   path eff\n");
        for m in self.per_minute.iter().take(20) {
            o.push_str(&format!(
                "  {:>3}  {:>8.1}  {:>7}  {:>10.3}  {:>10.1}  {:>7.0}  {:>9.3}\n",
                m.minute,
                m.distance_cm,
                m.flicks,
                m.overshoot_median,
                m.settle_median_ms,
                m.tremor_rms_counts_s,
                m.path_efficiency,
            ));
        }
        if self.per_minute.len() > 20 {
            o.push_str(&format!(
                "       … and {} more (see per_minute.csv)\n",
                self.per_minute.len() - 20
            ));
        }
    }

    fn render_segments(&self, o: &mut String) {
        if self.segments.is_empty() {
            return;
        }
        section(o, "Between markers");
        o.push_str(
            "   #   from(s)     to(s)   flicks   overshoot   tremor   path eff   clicks/m\n",
        );
        for s in &self.segments {
            o.push_str(&format!(
                "  {:>3}  {:>8.1}  {:>8.1}  {:>7}  {:>10.3}  {:>7.0}  {:>9.3}  {:>9.1}\n",
                s.index,
                s.t_start_s,
                s.t_end_s,
                s.flicks,
                s.overshoot_median,
                s.tremor_rms_counts_s,
                s.path_efficiency,
                s.clicks_per_min,
            ));
        }
        for s in &self.segments {
            let label = if s.label.is_empty() {
                "(before the first marker)"
            } else {
                &s.label
            };
            o.push_str(&format!("  {:>3}  {label}\n", s.index));
        }
    }

    fn render_markers(&self, o: &mut String) {
        if self.markers.is_empty() {
            return;
        }
        section(o, "Markers");
        for m in &self.markers {
            o.push_str(&format!("  {:>9.2}s   {}\n", m.t_s, m.label));
        }
    }

    fn render_warnings(&self, o: &mut String) {
        section(o, "Notes");
        for c in self.quality.caveats() {
            for (i, line) in wrap(&c, WIDTH - 6).into_iter().enumerate() {
                o.push_str(&format!("  {} {line}\n", if i == 0 { "i" } else { " " }));
            }
        }
        if self.warnings.is_empty() {
            o.push_str(if self.quality.event_count == 0 {
                "  no events to judge data quality on\n"
            } else {
                "  no data-quality problems detected\n"
            });
        } else {
            for w in &self.warnings {
                for (i, line) in wrap(w, WIDTH - 6).into_iter().enumerate() {
                    o.push_str(&format!("  {} {line}\n", if i == 0 { "!" } else { " " }));
                }
            }
        }
        o.push_str(&format!(
            "  per-second rows: {}   (use --csv-dir for the derived tables)\n",
            commas(self.per_second.len() as u64)
        ));
    }
}

fn kv(o: &mut String, label: &str, value: &str) {
    o.push_str(&format!(" {:<width$}{}\n", label, value, width = LABEL));
}

fn section(o: &mut String, title: &str) {
    let head = format!("── {title} ");
    let pad = WIDTH.saturating_sub(head.chars().count());
    o.push_str(&format!("\n{head}{}\n", "─".repeat(pad)));
}

fn flag(n: u64) -> String {
    if n == 0 {
        "0".to_string()
    } else {
        format!("{}   ← problem", commas(n))
    }
}

fn hist_row(o: &mut String, label: &str, fraction: f64, count: usize) {
    let width = 24usize;
    let filled = ((fraction * width as f64).round() as usize).min(width);
    o.push_str(&format!(
        "  {:<22} {}{} {:>5.1}%  {:>10}\n",
        label,
        "█".repeat(filled),
        "·".repeat(width - filled),
        fraction * 100.0,
        commas(count as u64),
    ));
}

/// `median / p90 / max (n)` — the same shape everywhere so the eye can scan it.
fn sum_line(s: &Summary, prec: usize) -> String {
    if s.is_empty() {
        return "—".to_string();
    }
    format!(
        "median {:.*}   p90 {:.*}   max {:.*}   (n={})",
        prec, s.median, prec, s.p90, prec, s.max, s.n
    )
}

/// Greedy word wrap, so a long warning does not blow past the report width.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Thousands separators, because event counts run into the millions.
fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{StreamBuilder, loaded_from, marker_at};
    use telemouse_core::event::buttons;

    fn demo_session() -> LoadedSession {
        let mut b = StreamBuilder::new();
        b.idle_ms(200);
        for _ in 0..3 {
            b.move_ms(25, 60, 0)
                .idle_ms(10)
                .move_ms(10, -10, 0)
                .idle_ms(5)
                .button(buttons::LEFT_DOWN)
                .idle_ms(39)
                .button(buttons::LEFT_UP) // 40ms hold
                .idle_ms(500);
        }
        loaded_from(b.into_events(), Some("cs2.exe"))
    }

    #[test]
    fn a_full_report_has_every_section_populated() {
        let r = build(demo_session(), Params::default());
        assert_eq!(r.schema, SCHEMA);
        assert_eq!(r.analyzer_version, ANALYZER_VERSION);
        assert!(r.generated_utc_us > 1_700_000_000_000_000);
        assert!(r.compute_ms >= 0.0);
        assert!(r.grid_cells > 0);
        assert!(r.grid_stored_cells <= r.grid_cells);
        assert_eq!(r.grid_dt_us, 1000);
        assert_eq!(r.session.session_id, "s-test");
        assert_eq!(r.session.game.as_deref(), Some("cs2.exe"));
        assert!(!r.session.aim_profile_missing);
        assert_eq!(r.flicks.count, 3);
        assert_eq!(r.clicks.total_clicks, 3);
        assert!(r.kinematics.total_distance_counts > 0.0);
        assert!(!r.per_second.is_empty());
        assert_eq!(r.per_minute.len(), 1);
        assert_eq!(r.segments.len(), 1, "unmarked session is one interval");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        // Every build phase is timed.
        assert!(r.timings.len() >= 8, "{:?}", r.timings);
        assert!(r.timings.iter().any(|t| t.phase == "prepare"));
    }

    #[test]
    fn parallel_and_sequential_builds_have_identical_results() {
        let mut session = demo_session();
        session.markers.push(marker_at(1_000, "round-2"));
        session.total_drops = 2;
        session.batches[0].drops_since_last = 2;
        let sequential = build_with_parallelism(session.clone(), Params::default(), false);
        let parallel = build_with_parallelism(session, Params::default(), true);

        let mut sequential = serde_json::to_value(sequential).unwrap();
        let mut parallel = serde_json::to_value(parallel).unwrap();
        for report in [&mut sequential, &mut parallel] {
            let report = report.as_object_mut().unwrap();
            report.remove("generated_utc_us");
            report.remove("compute_ms");
            report.remove("total_ms");
            report.remove("timings");
        }
        // Section by section, so a mismatch names the section instead of
        // dumping two whole reports.
        let (p, s) = (
            parallel.as_object().unwrap(),
            sequential.as_object().unwrap(),
        );
        assert_eq!(p.keys().collect::<Vec<_>>(), s.keys().collect::<Vec<_>>());
        for (k, pv) in p {
            assert_eq!(
                pv, &s[k],
                "section {k} differs between parallel and sequential"
            );
        }
    }

    #[test]
    fn report_parallelism_requires_enough_work_and_cpus() {
        assert!(!parallel_worthwhile(PARALLEL_MIN_EVENTS - 1, 32));
        assert!(!parallel_worthwhile(
            PARALLEL_MIN_EVENTS,
            PARALLEL_MIN_CPUS - 1
        ));
        assert!(parallel_worthwhile(PARALLEL_MIN_EVENTS, PARALLEL_MIN_CPUS));
    }

    /// Release-only manual probe used by `docs/PERFORMANCE-2026-09.md`.
    /// Kept ignored because it consumes a real, machine-local recording.
    fn real_session_perf_probe(parallel: bool) {
        let default = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("recordings")
            .join("s-20260904-042856-5006.jsonl");
        let path = std::env::var_os("TELEMOUSE_BENCH_SESSION")
            .map(std::path::PathBuf::from)
            .unwrap_or(default);
        if !path.is_file() {
            eprintln!(
                "skipping: benchmark recording not found at {}",
                path.display()
            );
            return;
        }

        let overall_started = std::time::Instant::now();
        let load_started = std::time::Instant::now();
        let session = crate::load::load_session(&path).unwrap();
        let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;
        let events = session.events.len();
        let started = std::time::Instant::now();
        let report = build_with_parallelism(session, Params::default(), parallel);
        let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
        let overall_ms = overall_started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "PERF mode={} events={} load_ms={:.1} wall_ms={:.1} build_ms={:.1} overall_ms={:.1} peak_working_set_mib={:.1}",
            if parallel { "parallel" } else { "sequential" },
            events,
            load_ms,
            wall_ms,
            report.compute_ms,
            overall_ms,
            peak_working_set_mib(),
        );
    }

    #[cfg(windows)]
    fn peak_working_set_mib() -> f64 {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;

        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        // SAFETY: GetCurrentProcess returns a process pseudo-handle valid for
        // this call, and `counters` is initialized with its exact byte size.
        unsafe {
            GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb).unwrap();
        }
        counters.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
    }

    #[cfg(not(windows))]
    fn peak_working_set_mib() -> f64 {
        0.0
    }

    #[test]
    #[ignore = "manual release benchmark over a real recording"]
    fn real_session_sequential_perf_probe() {
        real_session_perf_probe(false);
    }

    #[test]
    #[ignore = "manual release benchmark over a real recording"]
    fn real_session_parallel_perf_probe() {
        real_session_perf_probe(true);
    }

    #[test]
    fn the_rendered_report_reads_like_a_report() {
        let r = build(demo_session(), Params::default());
        let text = r.render();
        for needle in [
            "telemouse session  s-test",
            "Data quality",
            "Kinematics",
            "Flicks",
            "Sub-movements & micro-control",
            "Trigger discipline",
            "Repositioning lifts",
            "Notes",
            "path efficiency",
            "overshoot ratio",
            "pointer locked",
            "batch latency",
            "no data-quality problems detected",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        // Every line stays inside a sane terminal width.
        for line in text.lines() {
            assert!(
                line.chars().count() <= 100,
                "line too wide ({}): {line}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn json_round_trips_through_serde() {
        let r = build(demo_session(), Params::default());
        let js = r.to_json_pretty().unwrap();
        let back: Report = serde_json::from_str(&js).unwrap();
        // Compared field-wise with a tolerance rather than by `==`:
        // serde_json's float *parser* is up to 1 ULP off on some values
        // (std's `str::parse` is exact), so a byte-perfect f64 round-trip is
        // not something the format guarantees.
        assert_eq!(back.schema, r.schema);
        assert_eq!(back.analyzer_version, r.analyzer_version);
        assert_eq!(back.generated_utc_us, r.generated_utc_us);
        assert_eq!(back.grid_cells, r.grid_cells);
        assert_eq!(back.session.session_id, r.session.session_id);
        assert_eq!(back.flicks.count, r.flicks.count);
        assert_eq!(back.clicks.total_clicks, r.clicks.total_clicks);
        assert_eq!(back.per_second.len(), r.per_second.len());
        assert_eq!(back.warnings, r.warnings);
        assert!((back.kinematics.total_distance_m - r.kinematics.total_distance_m).abs() < 1e-9);
        assert!(
            (back.flicks.flicks[0].amplitude_deg - r.flicks.flicks[0].amplitude_deg).abs() < 1e-9
        );

        // And the shape a consumer would index into.
        let v: serde_json::Value = serde_json::from_str(&js).unwrap();
        assert_eq!(v["schema"], SCHEMA);
        assert!(v["flicks"]["flicks"].as_array().unwrap().len() == 3);
        assert!(!v["per_second"].as_array().unwrap().is_empty());
        assert!(v["kinematics"]["total_distance_m"].as_f64().unwrap() > 0.0);
        assert!(v["clicks"]["clicks"][0]["button"] == "left");
    }

    #[test]
    fn csvs_land_in_the_requested_directory() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("derived");
        let r = build(demo_session(), Params::default());
        let written = r.write_csvs(&out).unwrap();
        assert_eq!(written.len(), 3);

        let ps = std::fs::read_to_string(out.join("per_second.csv")).unwrap();
        assert_eq!(ps.lines().count(), r.per_second.len() + 1);
        let fl = std::fs::read_to_string(out.join("flicks.csv")).unwrap();
        assert_eq!(fl.lines().count(), 4); // header + 3 flicks
        let pm = std::fs::read_to_string(out.join("per_minute.csv")).unwrap();
        assert_eq!(pm.lines().count(), r.per_minute.len() + 1);
    }

    #[test]
    fn json_streams_to_a_file_identically_to_the_string_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("r.json");
        let r = build(demo_session(), Params::default());
        r.write_json(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim_end(), r.to_json_pretty().unwrap());
        let back: Report = serde_json::from_str(&text).unwrap();
        assert_eq!(back.flicks.count, r.flicks.count);
    }

    #[test]
    fn warnings_reach_the_rendered_output() {
        let mut s = demo_session();
        s.total_drops = 5;
        s.batches[0].drops_since_last = 5;
        let r = build(s, Params::default());
        assert!(!r.warnings.is_empty());
        let text = r.render();
        assert!(text.contains("ring-buffer drops"), "{text}");
        assert!(text.contains("← problem"), "{text}");
    }

    #[test]
    fn an_empty_recording_still_renders() {
        let r = build(loaded_from(Vec::new(), None), Params::default());
        let text = r.render();
        assert!(text.contains("telemouse session"));
        assert!(text.contains("unknown"));
        assert_eq!(r.flicks.count, 0);
        assert!(r.to_json_pretty().is_ok());
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(1_234_567), "1,234,567");
    }

    #[test]
    fn markers_are_placed_on_the_session_timeline() {
        let mut s = demo_session();
        s.markers.push(marker_at(1500, "clutch"));
        let r = build(s, Params::default());
        assert_eq!(r.markers.len(), 1);
        assert!((r.markers[0].t_s - 1.5).abs() < 1e-6);
        assert!(r.render().contains("clutch"));
    }

    #[test]
    fn split_by_marker_renders_the_per_interval_table() {
        let mut s = demo_session();
        s.markers.push(marker_at(1000, "round-2"));
        let params = Params {
            split_by_marker: true,
            ..Default::default()
        };
        let r = build(s, params);
        assert_eq!(r.segments.len(), 2);
        let text = r.render();
        assert!(text.contains("Between markers"), "{text}");
        assert!(text.contains("round-2"), "{text}");
        assert!(text.contains("(before the first marker)"), "{text}");
        // Without the flag the JSON still carries them, the terminal does not.
        let plain = build(
            {
                let mut s = demo_session();
                s.markers.push(marker_at(1000, "round-2"));
                s
            },
            Params::default(),
        );
        assert_eq!(plain.segments.len(), 2);
        assert!(!plain.render().contains("Between markers"));
    }

    #[test]
    fn the_summary_names_the_stretch_every_segment_covers() {
        let mut s = demo_session();
        s.markers.push(marker_at(500, "sens A"));
        s.markers.push(marker_at(1_000, "sens B"));
        let sum = build(s, Params::default()).summary(Vec::new());

        assert_eq!(sum.schema, SUMMARY_SCHEMA);
        assert_eq!(
            sum.markers
                .iter()
                .map(|m| m.label.as_str())
                .collect::<Vec<_>>(),
            ["sens A", "sens B"]
        );
        assert!((sum.markers[0].t_s - 0.5).abs() < 1e-6);
        assert!((sum.markers[1].t_s - 1.0).abs() < 1e-6);
        assert_eq!(sum.markers_total, 2);

        // Three stretches: the warmup, then one per marker. Each says which
        // labels bound it, so "sens A" can be compared against "sens B"
        // without opening the full report.
        assert_eq!(sum.segments_total, 3);
        let bounds: Vec<(&str, &str)> = sum
            .segments
            .iter()
            .map(|s| (s.label.as_str(), s.next_label.as_str()))
            .collect();
        assert_eq!(
            bounds,
            [("", "sens A"), ("sens A", "sens B"), ("sens B", "")]
        );
        assert!((sum.segments[1].t_start_s - 0.5).abs() < 1e-6);
        assert!((sum.segments[1].t_end_s - 1.0).abs() < 1e-6);
        assert_eq!(
            sum.segments.iter().map(|s| s.flicks).sum::<usize>(),
            sum.flicks.count
        );

        // And it stays the few-KB document its callers paste into a chat.
        let json = serde_json::to_string_pretty(&sum).unwrap();
        assert!(json.len() < 8_000, "summary grew to {} bytes", json.len());
        // The marker rows survive the round-trip (the float-heavy headline
        // fields are compared field-wise elsewhere: serde_json's parser can
        // land 1 ULP off, so `==` over the whole document is not a promise
        // the format makes).
        let back: ReportSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema, sum.schema);
        assert_eq!(back.markers_total, sum.markers_total);
        assert_eq!(back.segments_total, sum.segments_total);
        let labels = |s: &ReportSummary| -> Vec<String> {
            s.markers
                .iter()
                .map(|m| m.label.clone())
                .chain(s.segments.iter().map(|g| g.next_label.clone()))
                .collect()
        };
        assert_eq!(labels(&back), labels(&sum));
    }

    #[test]
    fn a_session_with_no_markers_carries_no_marker_rows() {
        let sum = build(demo_session(), Params::default()).summary(Vec::new());
        assert!(sum.markers.is_empty());
        assert_eq!(sum.markers_total, 0);
        // The lone interval would only repeat the headline numbers.
        assert!(sum.segments.is_empty());
        assert_eq!(sum.segments_total, 0);
    }

    #[test]
    fn the_summary_caps_the_marker_and_segment_lists() {
        let mut s = demo_session();
        for i in 0..(MAX_SUMMARY_MARKERS as u64 + 5) {
            s.markers.push(marker_at(50 + i * 50, &format!("m{i}")));
        }
        let long = "x".repeat(MAX_SUMMARY_LABEL_CHARS + 40);
        s.markers.push(marker_at(20, &long));
        let sum = build(s, Params::default()).summary(Vec::new());

        assert_eq!(sum.markers.len(), MAX_SUMMARY_MARKERS);
        assert_eq!(sum.markers_total, MAX_SUMMARY_MARKERS + 6);
        assert_eq!(sum.segments.len(), MAX_SUMMARY_MARKERS);
        assert_eq!(sum.segments_total, MAX_SUMMARY_MARKERS + 7);
        // Oldest first, so the truncation drops the tail.
        assert_eq!(sum.markers[1].label, "m0");
        // A label longer than the rest of the system accepts is clipped, not
        // carried whole.
        assert_eq!(
            sum.markers[0].label.chars().count(),
            MAX_SUMMARY_LABEL_CHARS + 1
        );
        assert!(sum.markers[0].label.ends_with('…'));
        let json = serde_json::to_string_pretty(&sum).unwrap();
        assert!(json.len() < 8_000, "summary grew to {} bytes", json.len());
    }

    #[test]
    fn unlabelled_hotkey_markers_are_told_apart_by_their_offsets() {
        let mut s = demo_session();
        s.markers.push(marker_at(400, "hotkey"));
        s.markers.push(marker_at(1_000, "hotkey"));
        let sum = build(s, Params::default()).summary(Vec::new());

        assert_eq!(sum.markers_total, 2);
        assert!(sum.markers.iter().all(|m| m.label == "hotkey"));
        assert!(sum.markers[0].t_s < sum.markers[1].t_s);
        // The segments still line up with the timeline even when nothing
        // distinguishes the labels.
        assert_eq!(sum.segments.len(), 3);
        assert!((sum.segments[1].t_start_s - 0.4).abs() < 1e-6);
        assert!((sum.segments[2].t_start_s - 1.0).abs() < 1e-6);
        assert_eq!(sum.segments[2].next_label, "");
    }

    #[test]
    fn the_timing_table_covers_every_phase() {
        let mut r = build(demo_session(), Params::default());
        r.push_timing("render", 1.5);
        let t = r.timing_table();
        assert!(t.contains("prepare"), "{t}");
        assert!(t.contains("per_minute"), "{t}");
        assert!(t.contains("render"), "{t}");
        assert!(t.contains("phase elapsed sum"), "{t}");
        assert!(t.contains("build wall"), "{t}");
    }
}
