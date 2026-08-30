//! Kinematics: velocity / acceleration / jerk, distance travelled, and path
//! efficiency.
//!
//! Derivatives come from Savitzky–Golay *differentiating* operators applied to
//! the raw binned velocity, so smoothing and differentiation happen in one
//! least-squares step instead of differencing an already-smoothed signal.
//!
//! Speed, acceleration and jerk summaries cover only **moving** cells (smoothed
//! speed above the still threshold). A gaming session is mostly stillness; a
//! median taken over every millisecond would just report zero.
//!
//! The derivative pass is fused with the moving-cell filter: a run's four
//! derivative lanes are computed into scratch buffers and immediately reduced
//! to the handful of scalars each moving cell contributes, so nothing
//! session-length is ever materialized.

use serde::{Deserialize, Serialize};
use telemouse_core::units;

use crate::savgol::SavGol;
use crate::series::{Prepared, SgScratch, sg_run_into};
use crate::stats::{self, Summary};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kinematics {
    /// Speed over moving cells, counts/s.
    pub speed_counts_per_s: Summary,
    /// Same cells, hand speed in cm/s.
    pub speed_cm_per_s: Summary,
    /// Same cells, aim-space speed in deg/s.
    pub speed_deg_per_s: Summary,
    /// |acceleration| over moving cells, counts/s².
    pub accel_counts_per_s2: Summary,
    /// The same acceleration in hand and aim space.
    pub accel_cm_per_s2: Summary,
    pub accel_deg_per_s2: Summary,
    /// |jerk| over moving cells, counts/s³.
    pub jerk_counts_per_s3: Summary,
    pub jerk_cm_per_s3: Summary,
    pub jerk_deg_per_s3: Summary,

    pub total_distance_counts: f64,
    pub total_distance_cm: f64,
    pub total_distance_m: f64,
    /// Aim-space path length, degrees.
    pub total_distance_deg: f64,
    /// Signed net turn, unwrapped.
    pub net_yaw_deg: f64,
    pub net_pitch_deg: f64,
    /// Net yaw folded into (-180, 180].
    pub net_yaw_wrapped_deg: f64,

    pub distance_cm_per_min: f64,
    pub distance_deg_per_min: f64,

    pub moving_time_s: f64,
    pub moving_fraction: f64,
    pub segment_count: usize,
    /// Net displacement ÷ path length, one sample per movement segment.
    pub path_efficiency: Summary,
    /// Path-length-weighted efficiency across the whole session.
    pub path_efficiency_weighted: f64,
    /// True when `Params::locked_only` restricted the degree-valued totals to
    /// pointer-locked spans.
    pub degrees_locked_only: bool,
}

pub fn compute(p: &Prepared) -> Kinematics {
    let g = &p.grid;
    let dt = g.dt;
    let n = g.len();
    let (kx, ky) = p.aim_scale();
    let still = p.params.still_speed;

    let d1 = SavGol::new(p.params.sg_half, p.params.sg_order, 1);
    let d2 = SavGol::new(p.params.sg_half, p.params.sg_order, 2);

    // One push per moving cell; nothing here is ever session-length. The
    // moving-cell count is taken up front (one cheap pass over the speed
    // lane) so the sample vectors are allocated exactly once instead of
    // doubling their way up from a guess.
    let moving_total: usize = g
        .runs
        .iter()
        .map(|r| r.speed.iter().filter(|&&s| s > still).count())
        .sum();
    let mut speeds = Vec::with_capacity(moving_total);
    let mut speeds_deg = Vec::with_capacity(moving_total);
    let mut accel = Vec::with_capacity(moving_total);
    let mut jerk = Vec::with_capacity(moving_total);
    let mut moving = 0usize;

    // Four derivative lanes per run, each into a reused scratch pair; the
    // per-cell reduction below only needs one run's lanes at a time.
    let mut sx1 = SgScratch::default();
    let mut sy1 = SgScratch::default();
    let mut sx2 = SgScratch::default();
    let mut sy2 = SgScratch::default();
    let half = p.params.sg_half;
    for r in &g.runs {
        let (ax, ox) = sg_run_into(&d1, &r.vx, r.start, n, half, dt, &mut sx1);
        let (ay, oy) = sg_run_into(&d1, &r.vy, r.start, n, half, dt, &mut sy1);
        let (jx, ojx) = sg_run_into(&d2, &r.vx, r.start, n, half, dt, &mut sx2);
        let (jy, ojy) = sg_run_into(&d2, &r.vy, r.start, n, half, dt, &mut sy2);
        let len = r.len();
        let (ax, ay) = (&ax[ox..ox + len], &ay[oy..oy + len]);
        let (jx, jy) = (&jx[ojx..ojx + len], &jy[ojy..ojy + len]);
        for j in 0..len {
            if r.speed[j] <= still {
                continue;
            }
            moving += 1;
            speeds.push(r.speed[j]);
            accel.push(ax[j].hypot(ay[j]));
            jerk.push(jx[j].hypot(jy[j]));
            if p.aim_cell_ok(r.start + j) {
                speeds_deg.push((r.vxs[j] * kx).hypot(r.vys[j] * ky));
            }
        }
    }
    debug_assert_eq!(moving, moving_total);

    // One selection pass per count-space sample; the cm and degree variants
    // are the same sample rescaled, so they are derived from the summary
    // rather than re-summarized (`Summary::scaled`). `counts_to_cm` is linear
    // in counts.
    let cm_per_count = p.counts_to_cm(1.0);
    let speed_summary = Summary::of(&speeds);
    let accel_summary = Summary::of(&accel);
    let jerk_summary = Summary::of(&jerk);

    // Distances from the raw events, so the totals are exact counts rather
    // than a re-integration of the grid.
    let mut dist_counts = 0.0;
    let mut dist_deg = 0.0;
    let mut net_x = 0i64;
    let mut net_y = 0i64;
    for (i, e) in p.events().iter().enumerate() {
        let (dx, dy) = (e.dx as f64, e.dy as f64);
        dist_counts += dx.hypot(dy);
        if p.aim_event_ok(i) {
            dist_deg += (dx * kx).hypot(dy * ky);
            net_x += e.dx as i64;
            net_y += e.dy as i64;
        }
    }
    let total_cm = p.counts_to_cm(dist_counts);
    let net_yaw = units::counts_to_yaw_deg(net_x as f64, &p.aim);
    let net_pitch = units::counts_to_pitch_deg(net_y as f64, &p.aim);

    let segs = p.segments();
    let mut effs = Vec::with_capacity(segs.len());
    let mut sum_net = 0.0;
    let mut sum_path = 0.0;
    for s in segs {
        let (dx, dy) = g.displacement(s.start, s.end);
        let path = g.path_length(s.start, s.end);
        if path <= 0.0 {
            continue;
        }
        let net = dx.hypot(dy);
        effs.push((net / path).min(1.0));
        sum_net += net;
        sum_path += path;
    }

    let minutes = p.minutes();
    let moving_time_s = moving as f64 * dt;
    let span = p.analysis_duration_s;

    Kinematics {
        speed_counts_per_s: speed_summary,
        speed_cm_per_s: speed_summary.scaled(cm_per_count),
        speed_deg_per_s: Summary::of(&speeds_deg),
        accel_counts_per_s2: accel_summary,
        accel_cm_per_s2: accel_summary.scaled(cm_per_count),
        accel_deg_per_s2: accel_summary.scaled(kx),
        jerk_counts_per_s3: jerk_summary,
        jerk_cm_per_s3: jerk_summary.scaled(cm_per_count),
        jerk_deg_per_s3: jerk_summary.scaled(kx),
        total_distance_counts: dist_counts,
        total_distance_cm: total_cm,
        total_distance_m: total_cm / 100.0,
        total_distance_deg: dist_deg,
        net_yaw_deg: net_yaw,
        net_pitch_deg: net_pitch,
        net_yaw_wrapped_deg: units::wrap_yaw_deg(net_yaw),
        distance_cm_per_min: total_cm / minutes,
        distance_deg_per_min: dist_deg / minutes,
        moving_time_s,
        moving_fraction: if span > 0.0 {
            moving_time_s / span
        } else {
            0.0
        },
        segment_count: segs.len(),
        path_efficiency: Summary::of(&effs),
        path_efficiency_weighted: if sum_path > 0.0 {
            sum_net / sum_path
        } else {
            0.0
        },
        degrees_locked_only: p.params.locked_only,
    }
}

/// Mean speed (counts/s) over the time window `[t0, t1)` seconds — used by the
/// click metrics for pre-click stability.
pub fn mean_speed_in_window(p: &Prepared, t0: f64, t1: f64) -> Option<f64> {
    if p.grid.is_empty() || t1 <= t0 {
        return None;
    }
    let a = (t0 / p.grid.dt).ceil().max(0.0) as usize;
    let b = ((t1 / p.grid.dt).ceil().max(0.0) as usize).min(p.grid.len());
    if a >= b {
        return None;
    }
    Some(p.grid.speed_sum(a, b) / (b - a) as f64)
}

/// Path efficiency over the movement segments falling inside `[a, b)` cells:
/// the length-weighted ratio, and the per-segment median.
pub fn path_efficiency_in(p: &Prepared, a: usize, b: usize) -> (f64, f64) {
    let mut sum_net = 0.0;
    let mut sum_path = 0.0;
    let mut effs = Vec::new();
    for s in p.segments() {
        if s.start < a || s.start >= b {
            continue;
        }
        let (dx, dy) = p.grid.displacement(s.start, s.end);
        let path = p.grid.path_length(s.start, s.end);
        if path <= 0.0 {
            continue;
        }
        let net = dx.hypot(dy);
        effs.push((net / path).min(1.0));
        sum_net += net;
        sum_path += path;
    }
    (
        if sum_path > 0.0 { sum_net / sum_path } else { 0.0 },
        stats::median(&effs).unwrap_or(0.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FIXTURE_DEG_PER_COUNT, StreamBuilder, prep};

    /// The headline sanity check from the brief: a constant-velocity pull.
    #[test]
    fn constant_velocity_segment_recovers_velocity_with_zero_jerk() {
        // 8 counts/ms for 300ms = 8000 counts/s, 2400 counts total.
        let mut b = StreamBuilder::new();
        b.move_ms(300, 8, 0);
        let p = prep(b.into_events());
        let k = compute(&p);

        assert!(
            (k.speed_counts_per_s.median - 8000.0).abs() < 1.0,
            "{:?}",
            k.speed_counts_per_s
        );
        // 8000 counts/s at 1600 CPI = 12.7 cm/s.
        assert!((k.speed_cm_per_s.median - 12.7).abs() < 0.01);
        // ...and 8000 * 0.044 = 352 deg/s.
        assert!((k.speed_deg_per_s.median - 352.0).abs() < 0.05);

        // Jerk on a constant velocity is zero except for the start/stop edges,
        // so check the interior of the pull directly.
        let d2 = SavGol::new(p.params.sg_half, p.params.sg_order, 2);
        let jx = d2.apply(&p.grid.dense(|r, j| r.vx[j]), p.grid.dt);
        for j in &jx[10..290] {
            assert!(j.abs() < 1e-3, "interior jerk {j} should be ~0");
        }

        // Straight line: path efficiency 1.
        assert!(
            (k.path_efficiency_weighted - 1.0).abs() < 1e-6,
            "{}",
            k.path_efficiency_weighted
        );
        assert!((k.path_efficiency.median - 1.0).abs() < 1e-6);
        assert_eq!(k.segment_count, 1);
    }

    #[test]
    fn distance_totals_convert_counts_correctly() {
        let mut b = StreamBuilder::new();
        b.move_ms(100, 3, 4); // 5 counts/ms magnitude, 500 counts of path
        let p = prep(b.into_events());
        let k = compute(&p);

        assert!((k.total_distance_counts - 500.0).abs() < 1e-9);
        // 500 counts at 1600 CPI.
        assert!((k.total_distance_cm - 500.0 / 1600.0 * 2.54).abs() < 1e-9);
        assert!((k.total_distance_m - k.total_distance_cm / 100.0).abs() < 1e-12);
        // 500 counts of path at 0.044 deg/count.
        assert!((k.total_distance_deg - 500.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-9);
        assert!((k.net_yaw_deg - 300.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-9);
        assert!((k.net_pitch_deg - 400.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-9);
    }

    #[test]
    fn a_wobbly_path_scores_below_a_straight_one() {
        let mut straight = StreamBuilder::new();
        straight.move_ms(200, 6, 0);
        let straight_eff = compute(&prep(straight.into_events())).path_efficiency_weighted;

        // Same net travel, but zig-zagging in y the whole way.
        let mut wobble = StreamBuilder::new();
        for i in 0..200 {
            wobble.push(6, if i % 2 == 0 { 6 } else { -6 }, 0, 0);
        }
        let wobble_eff = compute(&prep(wobble.into_events())).path_efficiency_weighted;

        assert!((straight_eff - 1.0).abs() < 1e-6);
        assert!(
            wobble_eff < 0.85,
            "wobble {wobble_eff} should be well under straight {straight_eff}"
        );
    }

    #[test]
    fn acceleration_shows_up_on_a_ramp_and_not_on_a_cruise() {
        // Ramp 0 -> 20000 counts/s over 200ms => ~100_000 counts/s^2.
        let mut b = StreamBuilder::new();
        for i in 0..200 {
            b.move_at_ms(1, i as f64 * 100.0, 0.0);
        }
        let p = prep(b.into_events());
        let k = compute(&p);
        assert!(
            (k.accel_counts_per_s2.median - 100_000.0).abs() < 20_000.0,
            "{:?}",
            k.accel_counts_per_s2
        );
        // The cm and degree variants are the same number, rescaled.
        assert!(
            (k.accel_cm_per_s2.median - p.counts_to_cm(k.accel_counts_per_s2.median)).abs() < 1e-6
        );
        assert!(
            (k.accel_deg_per_s2.median - k.accel_counts_per_s2.median * FIXTURE_DEG_PER_COUNT).abs()
                < 1e-6
        );

        let mut c = StreamBuilder::new();
        c.move_ms(300, 8, 0);
        let cruise = compute(&prep(c.into_events()));
        assert!(cruise.accel_counts_per_s2.median < 1.0);
    }

    #[test]
    fn moving_fraction_reflects_idle_time() {
        let mut b = StreamBuilder::new();
        b.move_ms(100, 5, 0).idle_ms(300).move_ms(100, 5, 0);
        let p = prep(b.into_events());
        let k = compute(&p);
        assert_eq!(k.segment_count, 2);
        assert!(
            k.moving_fraction > 0.3 && k.moving_fraction < 0.55,
            "{}",
            k.moving_fraction
        );
    }

    #[test]
    fn mean_speed_window_reads_the_right_cells() {
        let mut b = StreamBuilder::new();
        b.move_ms(50, 10, 0).idle_ms(50).move_ms(50, 10, 0);
        let p = prep(b.into_events());
        // 0-50ms is the 10_000 counts/s pull.
        let moving = mean_speed_in_window(&p, 0.005, 0.045).unwrap();
        assert!((moving - 10_000.0).abs() < 50.0, "{moving}");
        // 55-95ms is idle.
        let still = mean_speed_in_window(&p, 0.055, 0.095).unwrap();
        assert!(still < 1.0, "{still}");
        assert!(mean_speed_in_window(&p, 0.05, 0.05).is_none());
    }

    /// `--locked-only` must drop desktop-mode movement from the degree-valued
    /// totals while leaving the count-space ones — which are still real hand
    /// travel — exactly as they were.
    #[test]
    fn locked_only_excludes_unlocked_spans_from_the_degree_metrics() {
        use crate::series::{Params, prepare};
        use crate::testutil::{batch_meta, loaded_with_batches};

        let mut b = StreamBuilder::new();
        b.move_ms(200, 5, 0); // 1000 counts, half of it "in the game"
        let evs = b.into_events();
        let batches = || {
            vec![
                batch_meta(0, Some("cs2.exe"), true, 100),
                batch_meta(1, Some("cs2.exe"), false, 100),
            ]
        };

        let all = compute(&prepare(
            loaded_with_batches(evs.clone(), batches()),
            Params::default(),
        ));
        let locked = compute(&prepare(
            loaded_with_batches(evs, batches()),
            Params {
                locked_only: true,
                ..Default::default()
            },
        ));

        // Hand travel is unchanged: the mouse really did move that far.
        assert!((all.total_distance_counts - 1000.0).abs() < 1e-9);
        assert!((locked.total_distance_counts - all.total_distance_counts).abs() < 1e-9);
        // Aim travel halves, because only half of it was aim.
        assert!((all.total_distance_deg - 1000.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-9);
        assert!((locked.total_distance_deg - 500.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-9);
        assert!((locked.net_yaw_deg - 500.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-9);
        // ...as does the sample count behind the deg/s summary.
        assert_eq!(all.speed_deg_per_s.n, all.speed_counts_per_s.n);
        assert!(locked.speed_deg_per_s.n < all.speed_deg_per_s.n);
        assert_eq!(locked.speed_counts_per_s.n, all.speed_counts_per_s.n);
        assert!(locked.degrees_locked_only);
        assert!(!all.degrees_locked_only);
    }

    /// A truncated grid must divide its rates by what it analyzed, not by a
    /// session span it never looked at.
    #[test]
    fn truncation_clamps_the_rate_denominators_to_grid_coverage() {
        use crate::series::Params;

        let mut b = StreamBuilder::new();
        b.move_ms(4000, 5, 0); // 4 s of steady travel
        let evs = b.into_events();

        let full = compute(&prep(evs.clone()));
        let cut = compute(&crate::testutil::prepared_with(
            evs,
            Some("cs2.exe"),
            Params {
                max_grid_cells: 2000, // only the first 2 s
                ..Default::default()
            },
        ));

        // Half the session analyzed, so half the distance...
        assert!((cut.total_distance_counts / full.total_distance_counts - 1.0).abs() < 1e-9);
        // ...but the *rate* is per analyzed minute, so it does not halve: the
        // distance total still covers every event, and dividing it by the full
        // four seconds would understate the pace of what was actually read.
        assert!(cut.distance_cm_per_min > full.distance_cm_per_min * 1.9);
        // And the moving fraction stays a fraction rather than exceeding 1.
        assert!(cut.moving_fraction <= 1.0, "{}", cut.moving_fraction);
        assert!(cut.moving_fraction > 0.9, "{}", cut.moving_fraction);
    }

    #[test]
    fn empty_session_is_all_zeros() {
        let k = compute(&prep(Vec::new()));
        assert_eq!(k.segment_count, 0);
        assert_eq!(k.total_distance_counts, 0.0);
        assert!(k.speed_counts_per_s.is_empty());
        assert_eq!(k.path_efficiency_weighted, 0.0);
    }
}
