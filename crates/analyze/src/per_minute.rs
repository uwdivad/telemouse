//! Per-minute aggregates — the substrate for the plan's fatigue and warmup
//! curves.
//!
//! "Overshoot ratio, tremor RMS, path efficiency vs. minutes-into-session"
//! needs a per-minute series of exactly those, and "the same metrics over the
//! first N minutes across sessions" needs the same table sliced from the front.
//! Both fall out of one row per minute, so that is what this builds.
//!
//! Rows are rolled up from the per-second table where the quantity is additive,
//! and computed from the flick / tremor series where it is not.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::flicks::Flick;
use crate::kinematics;
use crate::micro::TremorSeries;
use crate::per_second::SecondRow;
use crate::series::Prepared;
use crate::stats;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MinuteRow {
    /// Whole minutes since session start.
    pub minute: u64,
    pub t_utc_us: i64,
    /// Seconds this row actually covers — the last one is usually partial.
    pub covered_s: f64,
    pub events: u64,
    pub distance_cm: f64,
    pub distance_deg: f64,
    /// Mean smoothed speed over the minute, counts/s and cm/s.
    pub mean_speed_counts_s: f64,
    pub mean_speed_cm_s: f64,
    pub max_speed_counts_s: f64,
    pub max_speed_cm_s: f64,
    pub clicks: u64,
    pub clicks_per_min: f64,
    pub flicks: u64,
    pub flicks_per_min: f64,
    /// Median overshoot ratio of the flicks starting in this minute.
    pub overshoot_median: f64,
    pub settle_median_ms: f64,
    pub tremor_rms_counts_s: f64,
    pub tremor_rms_cm_s: f64,
    /// Length-weighted path efficiency of the movement segments starting here.
    pub path_efficiency: f64,
    pub moving_fraction: f64,
}

/// Roll the per-second table up into minutes.
pub fn compute(
    p: &Prepared,
    seconds: &[SecondRow],
    flicks: &[Flick],
    tremor: &TremorSeries,
) -> Vec<MinuteRow> {
    if seconds.is_empty() {
        return Vec::new();
    }
    let flicks = crate::flicks::ordered_by_start(flicks);
    let per_sec = (1.0 / p.grid.dt).round() as usize;
    let n_min = seconds.len().div_ceil(60);
    let mut out = Vec::with_capacity(n_min);
    let mut flick_from = 0usize;

    for m in 0..n_min {
        let sa = m * 60;
        let sb = ((m + 1) * 60).min(seconds.len());
        let slice = &seconds[sa..sb];
        let covered_s = slice.len() as f64;
        let minutes = (covered_s / 60.0).max(f64::MIN_POSITIVE);

        let events: u64 = slice.iter().map(|r| r.events).sum();
        let distance_cm: f64 = slice.iter().map(|r| r.distance_cm).sum();
        let distance_deg: f64 = slice.iter().map(|r| r.distance_deg).sum();
        let clicks: u64 = slice.iter().map(|r| r.clicks).sum();
        let n_flicks: u64 = slice.iter().map(|r| r.flicks).sum();
        let moving_ms: u64 = slice.iter().map(|r| r.moving_ms).sum();
        let mean_speed = slice.iter().map(|r| r.mean_speed_counts_s).sum::<f64>() / covered_s;
        let max_speed = slice
            .iter()
            .fold(0.0f64, |a, r| a.max(r.max_speed_counts_s));

        let (t0, t1) = (sa as f64, sb as f64);
        while flick_from < flicks.len() && flicks[flick_from].t_start_s < t0 {
            flick_from += 1;
        }
        let flick_to = flicks[flick_from..].partition_point(|f| f.t_start_s < t1) + flick_from;
        let minute_flicks = &flicks[flick_from..flick_to];
        flick_from = flick_to;
        let os: Vec<f64> = minute_flicks.iter().map(|f| f.overshoot_ratio).collect();
        let settle: Vec<f64> = minute_flicks.iter().map(|f| f.settle_ms).collect();

        let (eff, _) = kinematics::path_efficiency_in(p, sa * per_sec, sb * per_sec);
        let rms = tremor.rms_seconds(sa, sb);

        out.push(MinuteRow {
            minute: m as u64,
            t_utc_us: p.t0_utc_us + (m as i64) * 60_000_000,
            covered_s,
            events,
            distance_cm,
            distance_deg,
            mean_speed_counts_s: mean_speed,
            mean_speed_cm_s: p.counts_to_cm(mean_speed),
            max_speed_counts_s: max_speed,
            max_speed_cm_s: p.counts_to_cm(max_speed),
            clicks,
            clicks_per_min: clicks as f64 / minutes,
            flicks: n_flicks,
            flicks_per_min: n_flicks as f64 / minutes,
            overshoot_median: stats::median(&os).unwrap_or(0.0),
            settle_median_ms: stats::median(&settle).unwrap_or(0.0),
            tremor_rms_counts_s: rms,
            tremor_rms_cm_s: p.counts_to_cm(rms),
            path_efficiency: eff,
            moving_fraction: moving_ms as f64 / (covered_s * 1000.0),
        });
    }
    out
}

const HEADER: &str = "minute,t_utc_us,covered_s,events,distance_cm,distance_deg,\
mean_speed_counts_s,mean_speed_cm_s,max_speed_counts_s,max_speed_cm_s,clicks,clicks_per_min,\
flicks,flicks_per_min,overshoot_median,settle_median_ms,tremor_rms_counts_s,tremor_rms_cm_s,\
path_efficiency,moving_fraction\n";

pub fn write_csv<W: Write>(w: &mut W, rows: &[MinuteRow]) -> io::Result<()> {
    w.write_all(HEADER.as_bytes())?;
    for r in rows {
        writeln!(
            w,
            "{},{},{:.1},{},{:.6},{:.6},{:.3},{:.6},{:.3},{:.6},{},{:.3},{},{:.3},{:.5},{:.2},\
{:.3},{:.6},{:.5},{:.5}",
            r.minute,
            r.t_utc_us,
            r.covered_s,
            r.events,
            r.distance_cm,
            r.distance_deg,
            r.mean_speed_counts_s,
            r.mean_speed_cm_s,
            r.max_speed_counts_s,
            r.max_speed_cm_s,
            r.clicks,
            r.clicks_per_min,
            r.flicks,
            r.flicks_per_min,
            r.overshoot_median,
            r.settle_median_ms,
            r.tremor_rms_counts_s,
            r.tremor_rms_cm_s,
            r.path_efficiency,
            r.moving_fraction,
        )?;
    }
    Ok(())
}

pub fn to_csv(rows: &[MinuteRow]) -> String {
    let mut buf = Vec::new();
    write_csv(&mut buf, rows).expect("writing to a Vec cannot fail");
    String::from_utf8(buf).expect("ASCII/UTF-8 output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{StreamBuilder, prep};
    use crate::{flicks, micro, per_second};
    use telemouse_core::event::buttons;

    /// Three minutes of flick-and-click, so the bucketing has something to
    /// split and the boundaries can be checked exactly.
    fn three_minutes() -> crate::series::Prepared {
        let mut b = StreamBuilder::new();
        // One 500ms cycle; 120 cycles per minute, 360 for three minutes.
        for _ in 0..362 {
            b.move_ms(25, 60, 0)
                .idle_ms(10)
                .move_ms(10, -10, 0)
                .idle_ms(5)
                .button(buttons::LEFT_DOWN)
                .idle_ms(39)
                .button(buttons::LEFT_UP)
                .idle_ms(409);
        }
        prep(b.into_events())
    }

    #[test]
    fn minutes_bucket_the_seconds_exactly() {
        let p = three_minutes();
        let fl = flicks::detect(&p);
        let (_, tremor) = micro::compute_full(&p);
        let secs = per_second::compute(&p, &fl, &[]);
        let mins = compute(&p, &secs, &fl, &tremor);

        assert_eq!(mins.len(), secs.len().div_ceil(60));
        assert!(mins.len() >= 3, "{} minutes", mins.len());
        // The first two minutes are complete; the last one is partial.
        assert_eq!(mins[0].covered_s, 60.0);
        assert_eq!(mins[1].covered_s, 60.0);
        assert!(mins.last().unwrap().covered_s <= 60.0);

        // Nothing is lost or double-counted in the roll-up.
        assert_eq!(
            mins.iter().map(|m| m.events).sum::<u64>(),
            secs.iter().map(|s| s.events).sum::<u64>()
        );
        assert_eq!(
            mins.iter().map(|m| m.clicks).sum::<u64>(),
            secs.iter().map(|s| s.clicks).sum::<u64>()
        );
        assert_eq!(mins.iter().map(|m| m.flicks).sum::<u64>(), fl.len() as u64);

        // Timeline anchoring, one minute apart.
        assert_eq!(mins[0].t_utc_us, p.t0_utc_us);
        assert_eq!(mins[2].t_utc_us, p.t0_utc_us + 120_000_000);

        // And the fatigue-curve columns are populated, not zero.
        assert!(mins[0].overshoot_median > 0.0);
        assert!(mins[0].tremor_rms_counts_s > 0.0);
        assert!(mins[0].path_efficiency > 0.5);
        assert!(mins[0].flicks_per_min > 60.0, "{}", mins[0].flicks_per_min);

        // Public callers may provide deserialized or hand-built flick lists;
        // aggregation remains identical even when those are not time ordered.
        let mut reversed = fl;
        reversed.reverse();
        assert_eq!(compute(&p, &secs, &reversed, &tremor), mins);
    }

    #[test]
    fn csv_has_one_line_per_minute_plus_a_header() {
        let p = three_minutes();
        let fl = flicks::detect(&p);
        let (_, tremor) = micro::compute_full(&p);
        let secs = per_second::compute(&p, &fl, &[]);
        let mins = compute(&p, &secs, &fl, &tremor);
        let csv = to_csv(&mins);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), mins.len() + 1);
        assert!(lines[0].starts_with("minute,t_utc_us,"));
        assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());
    }

    #[test]
    fn an_empty_session_has_no_minutes() {
        let p = prep(Vec::new());
        assert!(compute(&p, &[], &[], &TremorSeries::default()).is_empty());
    }
}
