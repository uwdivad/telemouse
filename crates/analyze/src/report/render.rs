//! The terminal renderings of a [`Report`]: the summary `report` prints and
//! the `--timing` table.

use super::Report;
use crate::stats::Summary;
use crate::timefmt::format_duration;

/// Width of the report, in characters.
const WIDTH: usize = 78;
/// Width of the label column of a key/value line.
const LABEL: usize = 28;
/// Flicks listed one per line before "… and N more".
const MAX_FLICK_ROWS: usize = 10;
/// Lifts listed one per line before "… and N more".
const MAX_LIFT_ROWS: usize = 8;
/// Minutes of the fatigue curve printed before pointing at the CSV.
const MAX_MINUTE_ROWS: usize = 20;
/// Cells of the share bar in the `--timing` table.
const TIMING_BAR: usize = 20;
/// Cells of the share bar in a histogram row.
const HIST_BAR: usize = 24;

impl Report {
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
                "  {:<16} {:>9.1} ms  {} {:>5.1}%\n",
                t.phase,
                t.ms,
                bar(share, TIMING_BAR),
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
        for fl in f.flicks.iter().take(MAX_FLICK_ROWS) {
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
        and_more(o, f.flicks.len(), MAX_FLICK_ROWS, "");
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
        for x in l.lifts.iter().take(MAX_LIFT_ROWS) {
            o.push_str(&format!(
                "  {:>3}  {:>8.2}  {:>9.2}  {:>9.0}  {:>10.2}  {:>11.2}\n",
                x.index, x.t_start_s, x.drift_cm, x.drift_ms, x.return_cm, x.opposition
            ));
        }
        and_more(o, l.lifts.len(), MAX_LIFT_ROWS, "");
    }

    fn render_minutes(&self, o: &mut String) {
        if self.per_minute.len() < 2 {
            return;
        }
        section(o, "Per minute (fatigue curve)");
        o.push_str("  min   dist cm   flicks   overshoot   settle ms   tremor   path eff\n");
        for m in self.per_minute.iter().take(MAX_MINUTE_ROWS) {
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
        and_more(
            o,
            self.per_minute.len(),
            MAX_MINUTE_ROWS,
            " (see per_minute.csv)",
        );
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

/// A `width`-cell bar, `█` for the share `fraction` of it and `·` for the
/// rest. Out-of-range and NaN fractions draw an empty or a full bar, never a
/// panic.
pub(super) fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction * width as f64).round().max(0.0) as usize).min(width);
    format!("{}{}", "█".repeat(filled), "·".repeat(width - filled))
}

fn hist_row(o: &mut String, label: &str, fraction: f64, count: usize) {
    o.push_str(&format!(
        "  {:<22} {} {:>5.1}%  {:>10}\n",
        label,
        bar(fraction, HIST_BAR),
        fraction * 100.0,
        commas(count as u64),
    ));
}

/// The tail of a capped list: `"       … and 3 more{suffix}\n"`, or nothing
/// when everything was shown.
fn and_more(o: &mut String, total: usize, shown: usize, suffix: &str) {
    if total > shown {
        o.push_str(&format!("       … and {} more{suffix}\n", total - shown));
    }
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
pub(super) fn commas(n: u64) -> String {
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
