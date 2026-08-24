//! Markers, and the session segmentation they induce.
//!
//! The plan's hotkey markers ("round start / clutch / tilt") exist so a session
//! can be cut into pieces before any game API does it for you. Printing their
//! timestamps is the least useful thing to do with them: what you actually want
//! is the headline metrics *per interval*, so a marked stretch can be compared
//! against the rest of the session.
//!
//! An interval runs from one marker to the next. Everything before the first
//! marker is its own unlabelled interval rather than being dropped — it is
//! usually the warmup.

use serde::{Deserialize, Serialize};

use crate::clicks::ClickReport;
use crate::flicks::Flick;
use crate::kinematics;
use crate::micro::TremorSeries;
use crate::series::Prepared;
use crate::stats;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkerRow {
    pub t_s: f64,
    pub t_utc_us: i64,
    pub label: String,
}

/// Every marker in the recording, placed on the analysis timeline.
pub fn rows(p: &Prepared) -> Vec<MarkerRow> {
    let anchor = &p.session.config.anchor;
    let mut out: Vec<MarkerRow> = p
        .session
        .markers
        .iter()
        .map(|m| {
            let utc = anchor.qpc_to_utc_us(m.ts_qpc);
            MarkerRow {
                t_s: (utc - p.t0_utc_us) as f64 / 1e6,
                t_utc_us: utc,
                label: m.label.clone(),
            }
        })
        .collect();
    out.sort_by_key(|m| m.t_utc_us);
    out
}

/// One stretch of session between consecutive markers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkerInterval {
    pub index: usize,
    /// The marker that opened this interval; empty for the stretch before the
    /// first marker.
    pub label: String,
    pub t_start_s: f64,
    pub t_end_s: f64,
    /// Grid cells `[start_cell, end_cell)`.
    pub start_cell: usize,
    pub end_cell: usize,
}

/// Cut the session at every marker. Always returns at least one interval, so
/// callers never need an "unmarked session" special case.
pub fn intervals(p: &Prepared, markers: &[MarkerRow]) -> Vec<MarkerInterval> {
    let n = p.grid.len();
    let dt = p.grid.dt;
    let end_s = n as f64 * dt;
    let cell = |t: f64| -> usize { ((t.max(0.0) / dt).floor() as usize).min(n) };

    let mut cuts: Vec<(f64, String)> = vec![(0.0, String::new())];
    for m in markers {
        if m.t_s > 0.0 && m.t_s < end_s {
            cuts.push((m.t_s, m.label.clone()));
        } else if m.t_s <= 0.0 {
            // A marker at the very start relabels the opening interval.
            cuts[0].1 = m.label.clone();
        }
    }

    let mut out = Vec::with_capacity(cuts.len());
    for i in 0..cuts.len() {
        let t_start = cuts[i].0;
        let t_end = cuts.get(i + 1).map_or(end_s, |c| c.0);
        out.push(MarkerInterval {
            index: i,
            label: cuts[i].1.clone(),
            t_start_s: t_start,
            t_end_s: t_end,
            start_cell: cell(t_start),
            end_cell: cell(t_end),
        });
    }
    out
}

/// The headline metrics over one marker interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentReport {
    pub index: usize,
    pub label: String,
    pub t_start_s: f64,
    pub t_end_s: f64,
    pub duration_s: f64,
    pub flicks: usize,
    pub flicks_per_min: f64,
    /// Median overshoot ratio of the flicks that start in this interval.
    pub overshoot_median: f64,
    pub settle_median_ms: f64,
    /// Length-weighted path efficiency of the movement segments starting here.
    pub path_efficiency: f64,
    pub tremor_rms_counts_s: f64,
    pub tremor_rms_cm_s: f64,
    pub clicks: usize,
    pub clicks_per_min: f64,
    pub distance_cm: f64,
    pub distance_deg: f64,
}

/// Per-interval headline metrics, reusing the already-computed flick, click and
/// tremor series rather than re-running the detectors.
pub fn segment_reports(
    p: &Prepared,
    flicks: &[Flick],
    clicks: &ClickReport,
    tremor: &TremorSeries,
    intervals: &[MarkerInterval],
) -> Vec<SegmentReport> {
    let (kx, ky) = p.aim_scale();
    intervals
        .iter()
        .map(|iv| {
            let dur = (iv.t_end_s - iv.t_start_s).max(0.0);
            let minutes = (dur / 60.0).max(f64::MIN_POSITIVE);
            let in_range = |t: f64| t >= iv.t_start_s && t < iv.t_end_s;

            let os: Vec<f64> = flicks
                .iter()
                .filter(|f| in_range(f.t_start_s))
                .map(|f| f.overshoot_ratio)
                .collect();
            let settle: Vec<f64> = flicks
                .iter()
                .filter(|f| in_range(f.t_start_s))
                .map(|f| f.settle_ms)
                .collect();
            let n_clicks = clicks.clicks.iter().filter(|c| in_range(c.t_s)).count();
            let (eff, _) = kinematics::path_efficiency_in(p, iv.start_cell, iv.end_cell);

            // Distances come off the grid so an interval boundary lands on a
            // cell rather than needing an event-index search.
            let path = p.grid.path_length(iv.start_cell, iv.end_cell);
            let (dx, dy) = p.grid.displacement(iv.start_cell, iv.end_cell);
            let _ = (dx, dy);
            let deg = {
                let mut acc = 0.0;
                for r in p.grid.runs_in(iv.start_cell, iv.end_cell) {
                    let (lo, hi) = r.clip(iv.start_cell, iv.end_cell);
                    for j in lo..hi {
                        acc += (r.vx[j] * kx).hypot(r.vy[j] * ky) * p.grid.dt;
                    }
                }
                acc
            };

            let sec_a = (iv.t_start_s.max(0.0)) as usize;
            let sec_b = (iv.t_end_s.max(0.0).ceil()) as usize;
            let rms = tremor.rms_seconds(sec_a, sec_b);

            SegmentReport {
                index: iv.index,
                label: iv.label.clone(),
                t_start_s: iv.t_start_s,
                t_end_s: iv.t_end_s,
                duration_s: dur,
                flicks: os.len(),
                flicks_per_min: os.len() as f64 / minutes,
                overshoot_median: stats::median(&os).unwrap_or(0.0),
                settle_median_ms: stats::median(&settle).unwrap_or(0.0),
                path_efficiency: eff,
                tremor_rms_counts_s: rms,
                tremor_rms_cm_s: p.counts_to_cm(rms),
                clicks: n_clicks,
                clicks_per_min: n_clicks as f64 / minutes,
                distance_cm: p.counts_to_cm(path),
                distance_deg: deg,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{StreamBuilder, loaded_from, marker_at};
    use crate::{clicks, flicks, micro};
    use telemouse_core::event::buttons;

    fn session_with_markers(labels: &[(u64, &str)]) -> Prepared {
        let mut b = StreamBuilder::new();
        for _ in 0..6 {
            b.move_ms(25, 60, 0)
                .idle_ms(10)
                .move_ms(10, -10, 0)
                .idle_ms(5)
                .button(buttons::LEFT_DOWN)
                .idle_ms(39)
                .button(buttons::LEFT_UP)
                .idle_ms(400);
        }
        let mut s = loaded_from(b.into_events(), Some("cs2.exe"));
        s.markers = labels.iter().map(|(ms, l)| marker_at(*ms, l)).collect();
        crate::series::prepare(s, crate::series::Params::default())
    }

    #[test]
    fn markers_cut_the_session_at_their_timestamps() {
        let p = session_with_markers(&[(1000, "round-1"), (2000, "round-2")]);
        let ms = rows(&p);
        assert_eq!(ms.len(), 2);
        let iv = intervals(&p, &ms);
        assert_eq!(iv.len(), 3);
        assert_eq!(iv[0].label, "");
        assert_eq!(iv[1].label, "round-1");
        assert_eq!(iv[2].label, "round-2");
        assert!((iv[0].t_start_s - 0.0).abs() < 1e-9);
        assert!((iv[0].t_end_s - 1.0).abs() < 1e-9);
        assert!((iv[1].t_start_s - 1.0).abs() < 1e-9);
        assert!((iv[1].t_end_s - 2.0).abs() < 1e-9);
        assert_eq!(iv[0].start_cell, 0);
        assert_eq!(iv[0].end_cell, 1000);
        assert_eq!(iv[1].start_cell, 1000);
        assert_eq!(iv[2].end_cell, p.grid.len());
        // The intervals tile the session with no gaps and no overlap.
        for w in iv.windows(2) {
            assert_eq!(w[0].end_cell, w[1].start_cell);
        }
    }

    #[test]
    fn an_unmarked_session_is_one_interval() {
        let p = session_with_markers(&[]);
        let iv = intervals(&p, &rows(&p));
        assert_eq!(iv.len(), 1);
        assert_eq!(iv[0].start_cell, 0);
        assert_eq!(iv[0].end_cell, p.grid.len());
    }

    #[test]
    fn per_interval_metrics_split_the_flicks_and_clicks() {
        let p = session_with_markers(&[(1500, "second-half")]);
        let fl = flicks::detect(&p);
        let cl = clicks::compute(&p);
        let (_, tremor) = micro::compute_full(&p);
        let iv = intervals(&p, &rows(&p));
        let reps = segment_reports(&p, &fl, &cl, &tremor, &iv);

        assert_eq!(reps.len(), 2);
        // Every flick and click belongs to exactly one interval.
        assert_eq!(reps.iter().map(|r| r.flicks).sum::<usize>(), fl.len());
        assert_eq!(
            reps.iter().map(|r| r.clicks).sum::<usize>(),
            cl.total_clicks
        );
        assert!(reps[0].flicks > 0 && reps[1].flicks > 0, "{reps:#?}");
        assert!(reps[0].path_efficiency > 0.5);
        assert!(reps[1].tremor_rms_counts_s > 0.0);
        assert_eq!(reps[1].label, "second-half");
    }

    /// A marker past the end of the grid cannot open an empty trailing
    /// interval, and one at zero relabels the opening stretch.
    #[test]
    fn out_of_range_markers_do_not_create_empty_intervals() {
        let p = session_with_markers(&[(0, "start"), (9_000_000, "way-past-the-end")]);
        let iv = intervals(&p, &rows(&p));
        assert_eq!(iv.len(), 1);
        assert_eq!(iv[0].label, "start");
    }
}
