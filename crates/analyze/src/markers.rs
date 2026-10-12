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
///
/// The cuts are placed on the integer µs timeline (each marker's UTC minus
/// the session zero) and floored to a cell there; the `f64` seconds in the
/// result are for reading, not for deciding anything.
pub fn intervals(p: &Prepared, markers: &[MarkerRow]) -> Vec<MarkerInterval> {
    let n = p.grid.len();
    let end_us = p.cell_start_us(n);
    let cell = |us: i64| -> usize { ((us.max(0) / p.grid_dt_us) as usize).min(n) };

    let mut cuts: Vec<(i64, String)> = vec![(0, String::new())];
    for m in markers {
        let us = m.t_utc_us - p.t0_utc_us;
        if us > 0 && us < end_us {
            cuts.push((us, m.label.clone()));
        } else if us <= 0 {
            // A marker at the very start relabels the opening interval.
            cuts[0].1 = m.label.clone();
        }
    }

    let mut out = Vec::with_capacity(cuts.len());
    for i in 0..cuts.len() {
        let start_us = cuts[i].0;
        let end = cuts.get(i + 1).map_or(end_us, |c| c.0);
        out.push(MarkerInterval {
            index: i,
            label: cuts[i].1.clone(),
            t_start_s: start_us as f64 / 1e6,
            t_end_s: end as f64 / 1e6,
            start_cell: cell(start_us),
            end_cell: cell(end),
        });
    }
    out
}

/// Hand and aim travel per interval, summed per event exactly as the session
/// totals in [`crate::kinematics`] are — `locked_only` included — so the
/// intervals add up to the totals. Each analysed event is attributed to the
/// interval holding its cell; `intervals` must be [`intervals`]' output
/// (ascending, tiling the grid).
fn travel_per_interval(p: &Prepared, intervals: &[MarkerInterval]) -> Vec<(f64, f64)> {
    let mut out = vec![(0.0, 0.0); intervals.len()];
    let mut k = 0usize;
    for (i, (e, &us)) in p
        .analysed_events()
        .iter()
        .zip(p.analysed_event_us())
        .enumerate()
    {
        if us < 0 {
            continue;
        }
        let cell = (us / p.grid_dt_us) as usize;
        // Events are near-monotonic, so the interval the last one landed in
        // almost always holds this one too; search only when it does not.
        let holds = |iv: &MarkerInterval| iv.start_cell <= cell && cell < iv.end_cell;
        if !intervals.get(k).is_some_and(holds) {
            match intervals.partition_point(|iv| iv.start_cell <= cell) {
                0 => continue,
                j if holds(&intervals[j - 1]) => k = j - 1,
                _ => continue,
            }
        }
        out[k].0 += stats::mag(e.dx as f64, e.dy as f64);
        if let Some(deg) = p.event_aim_deg(i) {
            out[k].1 += deg;
        }
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
    let flicks = crate::flicks::ordered_by_start(flicks);
    let travel = travel_per_interval(p, intervals);
    // ClickReport intentionally preserves source-event order so hold and
    // double-click pairing remain faithful even when a recording contains a
    // timestamp inversion. Sort only the sparse timestamps used for interval
    // attribution; NaNs did not match the former range predicate either.
    let mut click_times: Vec<f64> = clicks
        .clicks
        .iter()
        .map(|c| c.t_s)
        .filter(|t| !t.is_nan())
        .collect();
    click_times.sort_by(f64::total_cmp);
    intervals
        .iter()
        .zip(travel)
        .map(|(iv, (path_counts, deg))| {
            let dur = (iv.t_end_s - iv.t_start_s).max(0.0);
            let minutes = (dur / 60.0).max(f64::MIN_POSITIVE);
            // Restrict each ordered timeline with two logarithmic searches.
            let flick_a = flicks.partition_point(|f| f.t_start_s < iv.t_start_s);
            let flick_b = flicks.partition_point(|f| f.t_start_s < iv.t_end_s);
            let interval_flicks = &flicks[flick_a..flick_b];
            let click_a = click_times.partition_point(|&t| t < iv.t_start_s);
            let click_b = click_times.partition_point(|&t| t < iv.t_end_s);
            let n_clicks = click_b - click_a;
            let os: Vec<f64> = interval_flicks.iter().map(|f| f.overshoot_ratio).collect();
            let settle: Vec<f64> = interval_flicks.iter().map(|f| f.settle_ms).collect();
            let (eff, _) = kinematics::path_efficiency_in(p, iv.start_cell, iv.end_cell);

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
                distance_cm: p.counts_to_cm(path_counts),
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
            b.flick_cycle(1, 0, 400);
        }
        let mut s = loaded_from(b.into_events(), Some("cs2.exe"));
        s.markers = labels.iter().map(|(ms, l)| marker_at(*ms, l)).collect();
        crate::series::prepare(s, crate::series::Params::default()).unwrap()
    }

    /// Interval travel is summed per event, like the session totals, so the
    /// intervals add up to them — with `locked_only` honoured in both.
    #[test]
    fn interval_distances_add_up_to_the_session_totals() {
        use crate::series::{Params, prepare};
        use crate::testutil::{batch_meta, loaded_with_batches};

        let mut b = StreamBuilder::new();
        for _ in 0..6 {
            b.flick_cycle(1, 3, 400);
        }
        let events = b.into_events();
        let half = events.len() / 2;
        let batches = vec![
            batch_meta(0, Some("cs2.exe"), true, half),
            batch_meta(1, Some("cs2.exe"), false, events.len() - half),
        ];
        for locked_only in [false, true] {
            let mut s = loaded_with_batches(events.clone(), batches.clone());
            s.markers = vec![marker_at(700, "a"), marker_at(1_900, "b")];
            let p = prepare(
                s,
                Params {
                    locked_only,
                    ..Default::default()
                },
            )
            .unwrap();
            let k = crate::kinematics::compute(&p);
            let iv = intervals(&p, &rows(&p));
            let reps =
                segment_reports(&p, &[], &clicks::compute(&p), &TremorSeries::default(), &iv);
            assert_eq!(reps.len(), 3);
            let cm: f64 = reps.iter().map(|r| r.distance_cm).sum();
            let deg: f64 = reps.iter().map(|r| r.distance_deg).sum();
            assert!(
                (cm - k.total_distance_cm).abs() < 1e-9,
                "{cm} vs {}",
                k.total_distance_cm
            );
            assert!(
                (deg - k.total_distance_deg).abs() < 1e-9,
                "locked_only={locked_only}: {deg} vs {}",
                k.total_distance_deg
            );
            assert!(reps.iter().all(|r| r.distance_cm > 0.0));
            if locked_only {
                // The last stretch is all unlocked: no aim travel in it.
                assert_eq!(reps[2].distance_deg, 0.0);
            }
        }
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

    #[test]
    fn interval_attribution_handles_inverted_click_timestamps_and_boundaries() {
        let mut b = StreamBuilder::new();
        // File order is deliberately unrelated to timestamp order. Clicks at
        // exactly 1 s and 2 s belong to the intervals those markers open.
        b.push_at_us(2_000_000, 0, 0, buttons::LEFT_DOWN)
            .push_at_us(500_000, 0, 0, buttons::RIGHT_DOWN)
            .push_at_us(1_000_000, 0, 0, buttons::MIDDLE_DOWN)
            .push_at_us(999_000, 0, 0, buttons::X1_DOWN)
            .push_at_us(2_500_000, 0, 0, buttons::X2_DOWN);
        let mut s = loaded_from(b.into_events(), Some("cs2.exe"));
        s.markers = vec![marker_at(1000, "one"), marker_at(2000, "two")];
        let p = crate::series::prepare(s, crate::series::Params::default()).unwrap();
        let cl = clicks::compute(&p);
        let iv = intervals(&p, &rows(&p));
        let reps = segment_reports(&p, &[], &cl, &TremorSeries::default(), &iv);

        assert_eq!(
            reps.iter().map(|r| r.clicks).collect::<Vec<_>>(),
            vec![2, 1, 2]
        );
        assert_eq!(
            reps.iter().map(|r| r.clicks).sum::<usize>(),
            cl.total_clicks
        );
    }

    #[test]
    fn interval_metrics_accept_unsorted_flick_input() {
        let p = session_with_markers(&[(1500, "second-half")]);
        let fl = flicks::detect(&p);
        let cl = clicks::compute(&p);
        let (_, tremor) = micro::compute_full(&p);
        let iv = intervals(&p, &rows(&p));
        let expected = segment_reports(&p, &fl, &cl, &tremor, &iv);
        let mut reversed = fl;
        reversed.reverse();

        assert_eq!(segment_reports(&p, &reversed, &cl, &tremor, &iv), expected);
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
