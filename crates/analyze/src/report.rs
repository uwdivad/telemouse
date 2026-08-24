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

/// Bumped whenever the JSON shape changes incompatibly.
pub const SCHEMA: &str = "telemouse-analyze/2";
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
    pub aim_profile_missing: bool,
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

fn now_utc_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Times one phase, logs it, and records it for the `--timing` table.
struct Phases {
    started: std::time::Instant,
    mark: std::time::Instant,
    out: Vec<PhaseTiming>,
}

impl Phases {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            mark: now,
            out: Vec::new(),
        }
    }

    fn lap(&mut self, phase: &str) -> f64 {
        let ms = self.mark.elapsed().as_secs_f64() * 1000.0;
        self.mark = std::time::Instant::now();
        self.out.push(PhaseTiming {
            phase: phase.to_string(),
            ms,
        });
        ms
    }

    fn total_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }
}

/// Run every metric group over a loaded recording.
///
/// Takes the session by value — [`prepare`] moves the event vector into the
/// prepared series rather than copying it.
pub fn build(session: LoadedSession, params: Params) -> Report {
    let mut t = Phases::new();
    let p = prepare(session, params);
    let ms = t.lap("prepare");
    tracing::info!(
        events = p.events().len(),
        grid_cells = p.grid.len(),
        grid_stored = p.grid.stored_cells(),
        duration_s = format_args!("{:.1}", p.duration_s),
        elapsed_ms = format_args!("{ms:.1}"),
        "prepared series"
    );

    let quality = quality::compute(&p);
    // Computed once: `warnings()` walks and formats every check, and the old
    // code called it twice — once to log, once to put in the report.
    let warnings = quality.warnings();
    for w in &warnings {
        tracing::warn!("{w}");
    }
    let ms = t.lap("quality");
    tracing::info!(elapsed_ms = format_args!("{ms:.1}"), "data quality");

    let kin = kinematics::compute(&p);
    let ms = t.lap("kinematics");
    tracing::info!(
        segments = kin.segment_count,
        distance_m = format_args!("{:.2}", kin.total_distance_m),
        elapsed_ms = format_args!("{ms:.1}"),
        "kinematics"
    );

    let fl = flicks::compute(&p);
    let ms = t.lap("flicks");
    tracing::info!(
        flicks = fl.count,
        median_amplitude_deg = format_args!("{:.1}", fl.amplitude_deg.median),
        elapsed_ms = format_args!("{ms:.1}"),
        "detected flicks"
    );

    let (mi, tremor) = micro::compute_full(&p);
    let ms = t.lap("micro");
    tracing::info!(
        corrections = mi.total_corrections,
        blocks = mi.analyzed_blocks,
        band_ratio_8_12 = format_args!("{:.2}", mi.band_ratio_8_12),
        elapsed_ms = format_args!("{ms:.1}"),
        "sub-movements and tremor"
    );

    let cl = clicks::compute(&p);
    let ms = t.lap("clicks");
    tracing::info!(
        clicks = cl.total_clicks,
        elapsed_ms = format_args!("{ms:.1}"),
        "trigger discipline"
    );

    let lift = lifts::compute(&p);
    let ms = t.lap("lifts");
    tracing::info!(
        lifts = lift.count,
        elapsed_ms = format_args!("{ms:.1}"),
        "repositioning lifts"
    );

    let marker_rows = markers::rows(&p);
    let intervals = markers::intervals(&p, &marker_rows);
    let segments = markers::segment_reports(&p, &fl.flicks, &cl, &tremor, &intervals);
    let ms = t.lap("markers");
    tracing::info!(
        markers = marker_rows.len(),
        intervals = segments.len(),
        elapsed_ms = format_args!("{ms:.1}"),
        "marker segmentation"
    );

    let rows = per_second::compute(&p, &fl.flicks, &intervals);
    let ms = t.lap("per_second");
    tracing::info!(
        rows = rows.len(),
        elapsed_ms = format_args!("{ms:.1}"),
        "per-second aggregates"
    );

    let minutes = per_minute::compute(&p, &rows, &fl.flicks, &tremor);
    let ms = t.lap("per_minute");
    tracing::info!(
        rows = minutes.len(),
        elapsed_ms = format_args!("{ms:.1}"),
        "per-minute aggregates"
    );

    Report {
        schema: SCHEMA.to_string(),
        analyzer_version: ANALYZER_VERSION.to_string(),
        generated_utc_us: now_utc_us(),
        compute_ms: t.total_ms(),
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
        aim_profile_missing: p.aim_fallback,
    }
}

impl Report {
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
        o.push_str(&format!("  {:<16} {:>9.1} ms\n", "total", total));
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
        kv(o, "mouse", &format!("{:.0} CPI", s.mouse_cpi));
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
                "v{}   {} grid cells ({} stored)   {:.0} ms",
                self.analyzer_version,
                commas(self.grid_cells as u64),
                commas(self.grid_stored_cells as u64),
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
                "{:.1}% ≤1ms   median {:.2}ms   p99 {:.2}ms",
                q.pct_within_1ms, q.median_interval_ms, q.p99_interval_ms
            ),
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
        kv(o, "unparseable lines", &flag(q.bad_lines as u64));
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
        o.push_str("   #   from(s)     to(s)   flicks   overshoot   tremor   path eff   clicks/m\n");
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
        if self.warnings.is_empty() {
            o.push_str("  no data-quality problems detected\n");
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
    fn the_timing_table_covers_every_phase() {
        let mut r = build(demo_session(), Params::default());
        r.push_timing("render", 1.5);
        let t = r.timing_table();
        assert!(t.contains("prepare"), "{t}");
        assert!(t.contains("per_minute"), "{t}");
        assert!(t.contains("render"), "{t}");
        assert!(t.contains("total"), "{t}");
    }
}
