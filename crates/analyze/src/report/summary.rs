//! The headline projection of a [`Report`]: `report --summary`, the MCP
//! server's `session_summary`, and the control panel's cached
//! `<id>.summary.json`.

use serde::{Deserialize, Serialize};

use super::{Report, SessionSummary};

/// The headline numbers of a [`Report`], small enough to paste into a chat
/// or hand to a tool: `report --summary`. Every field is a projection of the
/// full report — nothing here is computed differently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSummary {
    pub schema: String,
    pub analyzer_version: String,
    /// The report's own [`SessionSummary`], embedded whole — so its shape is
    /// part of this one's (see the note on [`SessionSummary`]).
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
/// [`crate::markers::MarkerRow`] without the absolute timestamp — the
/// summary's reader wants "which stretch is this", not a wall clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryMarker {
    /// Seconds from the start of the session.
    pub t_s: f64,
    pub label: String,
}

/// The few numbers of a [`crate::markers::SegmentReport`] worth comparing
/// between two marked stretches, with the labels that bound the stretch.
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

/// Longest label the summary keeps, in characters: the limit the capture
/// agent, the panel and the MCP server already apply when a label is made, so
/// this only bites on a hand-written recording.
pub const MAX_SUMMARY_LABEL_CHARS: usize = telemouse_core::session::MAX_MARKER_LABEL_CHARS;

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

impl Report {
    /// The headline numbers, with the per-sink losses the report already
    /// carries from the sidecar it was built (or refreshed) with.
    pub fn headline(&self) -> ReportSummary {
        let losses = self
            .quality
            .sidecar
            .as_ref()
            .map(|s| s.sink_losses.clone())
            .unwrap_or_default();
        self.summary(losses)
    }

    /// The headline numbers, see [`ReportSummary`], with `sink_losses` as
    /// given — `load::read_sidecar(..).losses()`, or an empty list when there
    /// is no sidecar. [`Report::headline`] takes them from the report itself.
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
}
