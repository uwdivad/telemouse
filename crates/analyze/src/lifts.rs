//! Repositioning lifts, inferred.
//!
//! From the plan: *"true lifts are invisible to HID, but slow sustained
//! one-direction drift followed by fast opposite movement ≈ running out of
//! pad."* That is the whole heuristic, and it is worth being explicit that it
//! is a heuristic — the mouse reports nothing at all while it is in the air, so
//! a lift is only ever inferred from the shape of the movement around it.
//!
//! The pattern is matched over consecutive [`Segment`]s:
//!
//! 1. a **drift**: at least `lift_drift_min_ms` long, peak speed under
//!    `lift_drift_max_speed`, and covering at least `lift_drift_min_counts` of
//!    net displacement — the hand crossing the pad, not a micro-adjustment;
//! 2. a **gap** of at most `lift_max_gap_ms` — the airborne beat. There is
//!    always some gap, because the segments are separated by stillness;
//! 3. a **return**: peak speed over `lift_return_min_speed`, heading opposed to
//!    the drift by at least `lift_opposite_cos` — the hand being slapped back
//!    to the other side of the pad.
//!
//! Every threshold is a [`crate::series::Params`] field, so a different grip or
//! pad size is a flag away rather than a recompile.

use serde::{Deserialize, Serialize};

use crate::series::{Prepared, Segment};

/// One inferred repositioning lift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lift {
    pub index: usize,
    /// Start of the drift, seconds since session start.
    pub t_start_s: f64,
    /// End of the return sweep.
    pub t_end_s: f64,
    pub t_utc_us: i64,
    /// How long the drift ran, ms.
    pub drift_ms: f64,
    /// Net drift displacement, counts and cm.
    pub drift_counts: f64,
    pub drift_cm: f64,
    /// Stillness between drift and return, ms — the airborne beat.
    pub gap_ms: f64,
    /// Net return displacement, counts and cm.
    pub return_counts: f64,
    pub return_cm: f64,
    pub return_peak_counts_s: f64,
    /// Cosine of the angle between drift and return headings; −1 = reversed.
    pub opposition: f64,
    /// Drift heading in aim space, degrees.
    pub direction_deg: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiftReport {
    pub count: usize,
    pub per_minute: f64,
    /// Mean hand travel of the drifts that preceded a lift, cm.
    pub mean_drift_cm: f64,
    pub lifts: Vec<Lift>,
}

/// Everything one segment contributes to the match.
struct Shape {
    seg: Segment,
    dx: f64,
    dy: f64,
    mag: f64,
    peak: f64,
}

fn shape(p: &Prepared, seg: Segment) -> Shape {
    let (dx, dy) = p.grid.displacement(seg.start, seg.end);
    Shape {
        seg,
        dx,
        dy,
        mag: dx.hypot(dy),
        peak: p.grid.peak_speed(seg.start, seg.end),
    }
}

/// Match the drift → gap → snap-back pattern over the movement segments.
pub fn detect(p: &Prepared) -> Vec<Lift> {
    let prm = &p.params;
    let dt_ms = p.dt_ms();
    let segs = p.segments();
    let mut out = Vec::new();
    if segs.len() < 2 {
        return out;
    }

    let mut i = 0usize;
    while i + 1 < segs.len() {
        let a = shape(p, segs[i]);
        let drift_ms = a.seg.len() as f64 * dt_ms;
        if drift_ms < prm.lift_drift_min_ms as f64
            || a.peak > prm.lift_drift_max_speed
            || a.mag < prm.lift_drift_min_counts
        {
            i += 1;
            continue;
        }

        let b = shape(p, segs[i + 1]);
        let gap_ms = (b.seg.start.saturating_sub(a.seg.end)) as f64 * dt_ms;
        if gap_ms > prm.lift_max_gap_ms as f64 || b.peak < prm.lift_return_min_speed || b.mag <= 0.0
        {
            i += 1;
            continue;
        }

        let opposition = (a.dx * b.dx + a.dy * b.dy) / (a.mag * b.mag);
        if opposition > prm.lift_opposite_cos {
            i += 1;
            continue;
        }

        let (adx, ady) = p.to_deg(a.dx, a.dy);
        out.push(Lift {
            index: out.len(),
            t_start_s: p.grid.t(a.seg.start),
            t_end_s: p.grid.t(b.seg.end),
            t_utc_us: p.t0_utc_us + p.cell_start_us(a.seg.start),
            drift_ms,
            drift_counts: a.mag,
            drift_cm: p.counts_to_cm(a.mag),
            gap_ms,
            return_counts: b.mag,
            return_cm: p.counts_to_cm(b.mag),
            return_peak_counts_s: b.peak,
            opposition,
            direction_deg: ady.atan2(adx).to_degrees(),
        });
        // The return sweep cannot also be the next lift's drift.
        i += 2;
    }
    out
}

pub fn compute(p: &Prepared) -> LiftReport {
    let lifts = detect(p);
    let mean_drift_cm = if lifts.is_empty() {
        0.0
    } else {
        lifts.iter().map(|l| l.drift_cm).sum::<f64>() / lifts.len() as f64
    };
    LiftReport {
        count: lifts.len(),
        per_minute: lifts.len() as f64 / p.minutes(),
        mean_drift_cm,
        lifts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{StreamBuilder, prep};

    /// The synthetic signature: 500 ms of slow drift right (1500 counts ≈
    /// 2.4 cm of pad at 1600 CPI, well under the flick threshold), a 60 ms beat
    /// of stillness, then a 15 000 counts/s sweep back left.
    #[test]
    fn a_drift_then_snap_back_reads_as_a_lift() {
        let mut b = StreamBuilder::new();
        b.idle_ms(200);
        b.lift(500, 3_000.0, 60, 15_000.0);
        b.idle_ms(500);
        let p = prep(b.into_events());
        let lifts = detect(&p);
        assert_eq!(lifts.len(), 1, "{lifts:#?}");
        let l = &lifts[0];
        assert!((l.drift_ms - 500.0).abs() <= 10.0, "{}", l.drift_ms);
        assert!((l.drift_counts - 1500.0).abs() < 20.0, "{}", l.drift_counts);
        assert!((l.drift_cm - p.counts_to_cm(1500.0)).abs() < 0.1);
        assert!(l.gap_ms > 0.0 && l.gap_ms <= 80.0, "gap {}", l.gap_ms);
        assert!(l.opposition < -0.9, "opposition {}", l.opposition);
        assert!(l.return_peak_counts_s > 8_000.0);
        assert!(l.direction_deg.abs() < 1.0, "drift heads +x");

        let r = compute(&p);
        assert_eq!(r.count, 1);
        assert!(r.mean_drift_cm > 2.0);
    }

    #[test]
    fn several_lifts_are_counted_separately() {
        let mut b = StreamBuilder::new();
        for _ in 0..3 {
            b.idle_ms(300);
            b.lift(400, 3_500.0, 50, 20_000.0);
        }
        b.idle_ms(400);
        let r = compute(&prep(b.into_events()));
        assert_eq!(r.count, 3, "{:#?}", r.lifts);
        // Times are in order and distinct.
        for w in r.lifts.windows(2) {
            assert!(w[1].t_start_s > w[0].t_end_s);
        }
    }

    /// The three ways the pattern can fail, one at a time.
    #[test]
    fn ordinary_aiming_is_not_a_lift() {
        // A flick and its correction: fast out, slow back — the wrong way round.
        let mut flick = StreamBuilder::new();
        flick
            .idle_ms(100)
            .move_ms(25, 60, 0)
            .idle_ms(10)
            .move_ms(10, -10, 0)
            .idle_ms(400);
        assert!(detect(&prep(flick.into_events())).is_empty());

        // Slow drift, fast return — but the return goes the same way.
        let mut same = StreamBuilder::new();
        same.idle_ms(100)
            .move_at_ms(500, 3_000.0, 0.0)
            .idle_ms(60)
            .move_at_ms(100, 15_000.0, 0.0)
            .idle_ms(400);
        assert!(detect(&prep(same.into_events())).is_empty());

        // Right shape, but the drift is a 3 mm nudge rather than a pad crossing.
        let mut tiny = StreamBuilder::new();
        tiny.idle_ms(100);
        tiny.lift(500, 300.0, 60, 15_000.0);
        tiny.idle_ms(400);
        assert!(detect(&prep(tiny.into_events())).is_empty());

        // Right shape, but the hand was down the whole time: no airborne beat,
        // and the "gap" exceeds the threshold instead.
        let mut slow = StreamBuilder::new();
        slow.idle_ms(100);
        slow.lift(500, 3_000.0, 900, 15_000.0);
        slow.idle_ms(400);
        assert!(detect(&prep(slow.into_events())).is_empty());
    }

    #[test]
    fn thresholds_are_configurable() {
        let mut b = StreamBuilder::new();
        b.idle_ms(200);
        b.lift(500, 3_000.0, 60, 15_000.0);
        b.idle_ms(500);
        let evs = b.into_events();
        assert_eq!(detect(&prep(evs.clone())).len(), 1);

        // Demand a much longer pad crossing and the same session has none.
        let params = crate::series::Params {
            lift_drift_min_counts: 10_000.0,
            ..Default::default()
        };
        let p = crate::testutil::prepared_with(evs, Some("cs2.exe"), params);
        assert!(detect(&p).is_empty());
    }

    #[test]
    fn an_empty_session_has_no_lifts() {
        let r = compute(&prep(Vec::new()));
        assert_eq!(r.count, 0);
        assert_eq!(r.mean_drift_cm, 0.0);
    }
}
