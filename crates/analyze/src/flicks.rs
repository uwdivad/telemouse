//! Flick detection — the headline aim metric.
//!
//! Detection follows the plan literally: smoothed speed crossing a high
//! threshold opens a movement, which runs until speed has returned near zero
//! and *stayed* there for a hold window.
//!
//! Each detected flick is split into two phases:
//!
//! * **ballistic** — from where the movement left stillness to where speed
//!   first drops back below the still threshold. Amplitude, peak velocity and
//!   duration are measured here.
//! * **correction** — from the end of the ballistic phase to the settle point.
//!   Any displacement here *against* the flick direction is overshoot being
//!   walked back, which is what the overshoot ratio measures.
//!
//! Because a correction has to begin within `still_hold_ms` of the ballistic
//! end to be part of the same flick, a genuinely separate movement later on
//! becomes its own flick rather than being folded in as a correction.
//!
//! Displacement is taken from the *raw* binned velocity, so amplitudes are
//! exact count sums; thresholds are applied to the *smoothed* speed, so sensor
//! noise does not trigger detections.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::series::Prepared;
use crate::stats::{self, Summary};
use telemouse_core::event::buttons;

/// One detected flick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flick {
    pub index: usize,
    /// Seconds since session start.
    pub t_start_s: f64,
    /// End of the ballistic phase, seconds since session start.
    pub t_ballistic_end_s: f64,
    /// Settle point — where the movement is considered over.
    pub t_end_s: f64,
    /// Ballistic-phase length.
    pub duration_ms: f64,
    pub amplitude_deg: f64,
    pub amplitude_counts: f64,
    pub peak_velocity_deg_s: f64,
    pub peak_velocity_counts_s: f64,
    /// Correction distance against the flick direction ÷ amplitude.
    /// Zero when the post-flick movement did not reverse.
    pub overshoot_ratio: f64,
    pub correction_deg: f64,
    /// Ballistic end → velocity settled near zero.
    pub settle_ms: f64,
    /// Flick start → next button-down, if one lands inside the click window.
    pub time_to_click_ms: Option<f64>,
    /// Flick heading in aim space, degrees, `atan2(pitch, yaw)`.
    pub direction_deg: f64,
    /// Direction reversals inside the whole flick — extra corrective stutters.
    pub corrections: usize,
}

/// Borrow detector output as-is, or order a caller-provided flick list by its
/// start time when necessary. The detector already emits finite, ascending
/// starts, so report builds pay only one linear validation pass and allocate
/// nothing. Public aggregation helpers also accept hand-built or deserialized
/// lists; sorting that uncommon input preserves their historical semantics.
/// NaN starts never belonged to a half-open time interval and are discarded.
pub(crate) fn ordered_by_start(flicks: &[Flick]) -> Cow<'_, [Flick]> {
    let is_ordered = flicks.iter().all(|f| !f.t_start_s.is_nan())
        && flicks.windows(2).all(|w| w[0].t_start_s <= w[1].t_start_s);
    if is_ordered {
        return Cow::Borrowed(flicks);
    }

    let mut ordered: Vec<Flick> = flicks
        .iter()
        .filter(|f| !f.t_start_s.is_nan())
        .cloned()
        .collect();
    ordered.sort_by(|a, b| a.t_start_s.total_cmp(&b.t_start_s));
    Cow::Owned(ordered)
}

/// The detector settings a report was produced with, echoed for provenance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FlickParams {
    pub flick_speed_counts_s: f64,
    pub still_speed_counts_s: f64,
    pub still_hold_ms: u64,
    pub click_window_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlickReport {
    pub params: FlickParams,
    pub count: usize,
    pub per_minute: f64,
    pub amplitude_deg: Summary,
    pub peak_velocity_deg_s: Summary,
    pub duration_ms: Summary,
    pub overshoot_ratio: Summary,
    pub settle_ms: Summary,
    pub time_to_click_ms: Summary,
    /// Fraction of flicks followed by a button-down inside the click window.
    pub clicked_fraction: f64,
    pub flicks: Vec<Flick>,
}

/// Microseconds since session start of every button-down transition, ascending.
///
/// Sorted rather than merely file-ordered so the two-pointer match below is
/// correct even on a recording with a timestamp inversion in it.
pub(crate) fn click_times_us(p: &Prepared) -> Vec<i64> {
    let mut v: Vec<i64> = p
        .analysed_events()
        .iter()
        .zip(p.analysed_event_us())
        .filter(|(e, _)| e.buttons & buttons::ANY_DOWN != 0)
        .map(|(_, us)| *us)
        .collect();
    v.sort_unstable();
    v
}

/// Detect flicks over the prepared series.
pub fn detect(p: &Prepared) -> Vec<Flick> {
    let g = &p.grid;
    let n = g.len();
    let prm = &p.params;
    let hold = ((prm.still_hold_ms as f64 / 1000.0) / g.dt)
        .round()
        .max(1.0) as usize;
    let clicks = click_times_us(p);
    let (kx, ky) = p.aim_scale();
    let dt_ms = p.dt_ms();

    let mut out: Vec<Flick> = Vec::new();
    // Flick starts are non-decreasing, and so are `clicks`, so the "first
    // button-down at or after this flick started" walks forward once across
    // the whole session instead of rescanning the click vector per flick.
    let mut ci = 0usize;
    let mut i = 0usize;
    while i < n {
        let Some((_, r)) = g.run_from(i) else { break };
        let mut j = i.max(r.start) - r.start;
        let mut next_i = r.end();
        while j < r.len() {
            if r.speed[j] < prm.flick_speed {
                j += 1;
                continue;
            }

            // Rewind to where this movement left stillness. A movement cannot
            // leave its run: every cell outside one is exactly zero.
            let mut ls = j;
            while ls > 0 && r.speed[ls - 1] > prm.still_speed {
                ls -= 1;
            }
            let mut le = j;
            while le < r.len() && r.speed[le] > prm.still_speed {
                le += 1;
            }
            let (start, ballistic_end) = (r.start + ls, r.start + le);

            // Settle: first run of `hold` consecutive quiet cells at or after
            // the ballistic end. Corrective sub-movements push this later.
            let settle_at = g.quiet_start_after(ballistic_end, hold, prm.still_speed);

            let (ax, ay) = g.displacement(start, ballistic_end);
            let amplitude_counts = stats::mag(ax, ay);
            if amplitude_counts <= f64::EPSILON {
                j = le.max(j + 1);
                continue;
            }
            let (ux, uy) = (ax / amplitude_counts, ay / amplitude_counts);

            // Correction: displacement after the ballistic phase, projected
            // back onto the flick direction. Only a reversal counts as
            // overshoot.
            let (cx, cy) = g.displacement(ballistic_end, settle_at);
            let proj = cx * ux + cy * uy;
            let correction_counts = if proj < 0.0 { -proj } else { 0.0 };

            let start_us = p.cell_start_us(start);
            while ci < clicks.len() && clicks[ci] < start_us {
                ci += 1;
            }
            let time_to_click_ms = clicks.get(ci).and_then(|&c| {
                let d = (c - start_us) as f64 / 1000.0;
                (d <= prm.click_window_ms).then_some(d)
            });

            let (adx, ady) = p.to_deg(ax, ay);
            out.push(Flick {
                index: out.len(),
                t_start_s: g.t(start),
                t_ballistic_end_s: g.t(ballistic_end),
                t_end_s: g.t(settle_at),
                duration_ms: (ballistic_end - start) as f64 * dt_ms,
                amplitude_deg: stats::mag(adx, ady),
                amplitude_counts,
                peak_velocity_deg_s: p.peak_aim_speed(start, ballistic_end),
                peak_velocity_counts_s: g.peak_speed(start, ballistic_end),
                overshoot_ratio: correction_counts / amplitude_counts,
                correction_deg: stats::mag(
                    correction_counts * ux * kx,
                    correction_counts * uy * ky,
                ),
                settle_ms: (settle_at.saturating_sub(ballistic_end)) as f64 * dt_ms,
                time_to_click_ms,
                direction_deg: ady.atan2(adx).to_degrees(),
                corrections: reversals(p, start, settle_at, ux, uy),
            });

            next_i = settle_at.max(r.start + j + 1);
            break;
        }
        i = next_i.max(i + 1);
    }
    out
}

/// Direction changes of velocity projected on `(ux, uy)` over `[a, b)`.
///
/// A reversal only counts once the opposite sign has held for
/// `Params::min_reversal_ms` above the still-speed deadband — see that field
/// for why the sustain requirement is not optional.
pub(crate) fn reversals(p: &Prepared, a: usize, b: usize, ux: f64, uy: f64) -> usize {
    let g = &p.grid;
    let b = b.min(g.len());
    if b <= a {
        return 0;
    }
    let min_run = p.params.min_reversal_ms.max(1);
    let dead = p.params.still_speed;

    /// Sign-run state: how long the current sign has held, and the last sign
    /// that held long enough to qualify as a real direction.
    struct Sustain {
        count: usize,
        last_qualified: i8,
        cur: i8,
        run: usize,
        min_run: usize,
        dead: f64,
    }

    impl Sustain {
        fn step(&mut self, v: f64) {
            let s = if v > self.dead {
                1i8
            } else if v < -self.dead {
                -1i8
            } else {
                0
            };
            if s != 0 && s == self.cur {
                self.run += 1;
            } else {
                self.cur = s;
                self.run = usize::from(s != 0);
            }
            if self.run == self.min_run {
                if self.last_qualified != 0 && self.cur != self.last_qualified {
                    self.count += 1;
                }
                self.last_qualified = self.cur;
            }
        }
    }

    let mut st = Sustain {
        count: 0,
        last_qualified: 0,
        cur: 0,
        run: 0,
        min_run,
        dead,
    };
    let mut seen_to = a;
    for r in g.runs_in(a, b) {
        // Silence between runs is a zero-signed stretch: one visit is enough
        // to reset the sustain counter, which is all a zero cell ever does.
        if r.start > seen_to {
            st.step(0.0);
        }
        let (lo, hi) = r.clip(a, b);
        for j in lo..hi {
            st.step(r.vxs[j] * ux + r.vys[j] * uy);
        }
        seen_to = r.end();
    }
    st.count
}

/// Detect and aggregate.
pub fn compute(p: &Prepared) -> FlickReport {
    let flicks = detect(p);
    summarize(p, flicks)
}

fn summarize(p: &Prepared, flicks: Vec<Flick>) -> FlickReport {
    let amp: Vec<f64> = flicks.iter().map(|f| f.amplitude_deg).collect();
    let pv: Vec<f64> = flicks.iter().map(|f| f.peak_velocity_deg_s).collect();
    let dur: Vec<f64> = flicks.iter().map(|f| f.duration_ms).collect();
    let os: Vec<f64> = flicks.iter().map(|f| f.overshoot_ratio).collect();
    let st: Vec<f64> = flicks.iter().map(|f| f.settle_ms).collect();
    let ttc: Vec<f64> = flicks.iter().filter_map(|f| f.time_to_click_ms).collect();

    FlickReport {
        params: FlickParams {
            flick_speed_counts_s: p.params.flick_speed,
            still_speed_counts_s: p.params.still_speed,
            still_hold_ms: p.params.still_hold_ms,
            click_window_ms: p.params.click_window_ms,
        },
        count: flicks.len(),
        per_minute: flicks.len() as f64 / p.minutes(),
        amplitude_deg: Summary::of_vec(amp),
        peak_velocity_deg_s: Summary::of_vec(pv),
        duration_ms: Summary::of_vec(dur),
        overshoot_ratio: Summary::of_vec(os),
        settle_ms: Summary::of_vec(st),
        clicked_fraction: if flicks.is_empty() {
            0.0
        } else {
            ttc.len() as f64 / flicks.len() as f64
        },
        time_to_click_ms: Summary::of_vec(ttc),
        flicks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FIXTURE_DEG_PER_COUNT, StreamBuilder, prep};

    /// The canonical fixture: a 1500-count pull, a 100-count reverse
    /// correction, stillness, then a click.
    ///
    /// * amplitude 1500 counts = 66.0°
    /// * overshoot 100 / 1500 = 0.0667
    /// * click lands 50 ms after the flick starts
    fn one_flick_stream() -> Vec<telemouse_core::RawEvent> {
        let mut b = StreamBuilder::new();
        b.move_ms(25, 60, 0) // ballistic: 25ms at 60000 counts/s
            .idle_ms(10) // brief pause
            .move_ms(10, -10, 0) // correction: -100 counts
            .idle_ms(5);
        assert!((b.now_ms() - 50.0).abs() < 1e-9, "click lands at 50ms");
        b.button(buttons::LEFT_DOWN).idle_ms(400);
        b.into_events()
    }

    #[test]
    fn one_synthetic_flick_is_detected_with_the_right_numbers() {
        let p = prep(one_flick_stream());
        let fs = detect(&p);
        assert_eq!(fs.len(), 1, "expected exactly one flick, got {fs:?}");
        let f = &fs[0];

        assert_eq!(f.t_start_s, 0.0);
        assert!(
            (f.amplitude_counts - 1500.0).abs() < 1e-6,
            "{}",
            f.amplitude_counts
        );
        assert!(
            (f.amplitude_deg - 1500.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-6,
            "{}",
            f.amplitude_deg
        );
        // Ballistic phase is 25ms plus a few ms of smoothing smear.
        assert!(
            (f.duration_ms - 25.0).abs() <= 5.0,
            "duration {}",
            f.duration_ms
        );
        // Peak speed is read off the smoothed signal, which overshoots the
        // 60_000 counts/s plateau by up to 2/21 where the pull stops.
        assert!(
            (f.peak_velocity_counts_s - 60_000.0).abs() < 60_000.0 * 0.12,
            "{}",
            f.peak_velocity_counts_s
        );
        assert!(
            (f.peak_velocity_deg_s - f.peak_velocity_counts_s * FIXTURE_DEG_PER_COUNT).abs() < 1e-6,
            "{}",
            f.peak_velocity_deg_s
        );
        // Overshoot: the 100-count reversal against a 1500-count flick.
        assert!(
            (f.overshoot_ratio - 100.0 / 1500.0).abs() < 1e-6,
            "overshoot {}",
            f.overshoot_ratio
        );
        assert!((f.correction_deg - 100.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-6);
        // The corrective sub-movement is a direction reversal.
        assert_eq!(f.corrections, 1);
        // Settle covers the pause + correction + ring-down.
        assert!(
            f.settle_ms > 15.0 && f.settle_ms < 60.0,
            "settle {}",
            f.settle_ms
        );
        // Click 50ms after the flick started.
        let ttc = f.time_to_click_ms.expect("click inside the window");
        assert!((ttc - 50.0).abs() < 1e-6, "time to click {ttc}");
        // Flick heading is +x.
        assert!(f.direction_deg.abs() < 1e-9);
    }

    #[test]
    fn aggregates_match_the_single_flick() {
        let p = prep(one_flick_stream());
        let r = compute(&p);
        assert_eq!(r.count, 1);
        assert_eq!(r.clicked_fraction, 1.0);
        assert!((r.amplitude_deg.median - 66.0).abs() < 1e-6);
        assert!((r.overshoot_ratio.median - 100.0 / 1500.0).abs() < 1e-6);
        assert_eq!(r.params.flick_speed_counts_s, 800.0);
    }

    #[test]
    fn two_separated_flicks_are_counted_separately() {
        let mut b = StreamBuilder::new();
        b.move_ms(20, 50, 0)
            .idle_ms(300)
            .move_ms(20, -50, 0)
            .idle_ms(300);
        let fs = detect(&prep(b.into_events()));
        assert_eq!(fs.len(), 2, "{fs:?}");
        assert!((fs[0].amplitude_counts - 1000.0).abs() < 1e-6);
        assert!((fs[1].amplitude_counts - 1000.0).abs() < 1e-6);
        // Opposite headings.
        assert!(fs[0].direction_deg.abs() < 1e-6);
        assert!((fs[1].direction_deg.abs() - 180.0).abs() < 1e-6);
        // Far apart, so neither counts as the other's correction.
        assert_eq!(fs[0].overshoot_ratio, 0.0);
    }

    #[test]
    fn a_slow_drag_below_threshold_is_not_a_flick() {
        // 250 counts/s for two seconds — plenty of travel, never fast.
        // (At 1 kHz a single count already reads as 1000 counts/s raw, so a
        // "slow" drag is slow in the *smoothed* signal, not the raw one.)
        let mut b = StreamBuilder::new();
        b.idle_ms(100).move_at_ms(2000, 250.0, 0.0).idle_ms(100);
        let p = prep(b.into_events());
        assert!(
            p.grid.peak_speed(0, p.grid.len()) < 800.0,
            "peak {}",
            p.grid.peak_speed(0, p.grid.len())
        );
        assert!(detect(&p).is_empty());
        let r = compute(&p);
        assert_eq!(r.count, 0);
        assert_eq!(r.clicked_fraction, 0.0);
        assert!(r.amplitude_deg.is_empty());
    }

    #[test]
    fn threshold_is_configurable() {
        let mut b = StreamBuilder::new();
        b.move_at_ms(200, 1200.0, 0.0).idle_ms(200); // peak 1200 counts/s
        let evs = b.into_events();

        assert_eq!(detect(&prep(evs.clone())).len(), 1);

        let params = crate::series::Params {
            flick_speed: 5000.0,
            ..Default::default()
        };
        let p = crate::testutil::prepared_with(evs, Some("cs2.exe"), params);
        assert!(detect(&p).is_empty());
    }

    #[test]
    fn a_flick_without_a_click_reports_no_time_to_click() {
        let mut b = StreamBuilder::new();
        b.move_ms(20, 50, 0).idle_ms(500).button(buttons::LEFT_DOWN);
        let fs = detect(&prep(b.into_events()));
        assert_eq!(fs.len(), 1);
        // The click is 520ms out, well past the 300ms window.
        assert_eq!(fs[0].time_to_click_ms, None);
    }

    #[test]
    fn no_overshoot_when_the_correction_continues_in_the_same_direction() {
        let mut b = StreamBuilder::new();
        b.move_ms(20, 50, 0) // 1000 counts +x
            .idle_ms(5)
            .move_ms(10, 10, 0) // 100 more counts, same direction
            .idle_ms(300);
        let fs = detect(&prep(b.into_events()));
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].overshoot_ratio, 0.0);
        assert_eq!(fs[0].correction_deg, 0.0);
    }

    #[test]
    fn empty_session_detects_nothing() {
        let r = compute(&prep(Vec::new()));
        assert_eq!(r.count, 0);
        assert!(r.flicks.is_empty());
    }

    /// The monotonic two-pointer must agree with the quadratic scan it
    /// replaced, on a session with many flicks and many clicks — including
    /// flicks with no click in range and clicks with no flick.
    #[test]
    fn two_pointer_click_matching_matches_the_old_linear_scan() {
        let mut b = StreamBuilder::new();
        b.idle_ms(100);
        for i in 0..8 {
            b.move_ms(20, 50, 0).idle_ms(10);
            if i % 3 != 2 {
                // Most flicks are followed by a click inside the window...
                b.button(buttons::LEFT_DOWN)
                    .idle_ms(20)
                    .button(buttons::LEFT_UP);
            }
            // ...and a stray click far from any flick lands in the rest gap.
            b.idle_ms(200);
            if i % 4 == 1 {
                b.button(buttons::LEFT_DOWN)
                    .idle_ms(10)
                    .button(buttons::LEFT_UP);
            }
            b.idle_ms(400);
        }
        let p = prep(b.into_events());
        let fs = detect(&p);
        assert!(fs.len() >= 6, "{} flicks", fs.len());

        // The pre-fix implementation: rescan the whole click vector per flick.
        let clicks = click_times_us(&p);
        for f in &fs {
            let start_us = (f.t_start_s * 1e6).round() as i64;
            let want = clicks
                .iter()
                .map(|&c| (c - start_us) as f64 / 1000.0)
                .find(|&d| (0.0..=p.params.click_window_ms).contains(&d));
            assert_eq!(f.time_to_click_ms, want, "flick {}", f.index);
        }
        assert!(fs.iter().any(|f| f.time_to_click_ms.is_some()));
        assert!(fs.iter().any(|f| f.time_to_click_ms.is_none()));
    }
}
