//! Per-second aggregates — the plan's "derived table".
//!
//! One row per wall-clock second of the session, which is the grain the
//! longitudinal metrics (fatigue curves, warmup curves) are later built on,
//! and the natural thing to hand a dashboard.
//!
//! When the grid was truncated the table stops at the last *complete* second
//! the grid covers, and events past that are dropped rather than piled into the
//! final row — otherwise a truncated three-hour session ends on a spike of
//! several million events in "the last second", which is an artifact, not data.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::flicks::Flick;
use crate::markers::MarkerInterval;
use crate::series::Prepared;
use crate::stats;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecondRow {
    /// Whole seconds since session start.
    pub second: u64,
    /// UTC µs at the start of this second.
    pub t_utc_us: i64,
    pub events: u64,
    pub distance_counts: f64,
    pub distance_cm: f64,
    pub distance_deg: f64,
    /// Mean smoothed speed over the second, counts/s.
    pub mean_speed_counts_s: f64,
    pub mean_speed_cm_s: f64,
    pub max_speed_counts_s: f64,
    pub max_speed_cm_s: f64,
    pub max_speed_deg_s: f64,
    pub clicks: u64,
    pub flicks: u64,
    /// Milliseconds in this second spent above the still threshold.
    pub moving_ms: u64,
    /// Label of the marker interval this second falls in; empty before the
    /// first marker, and for a session with no markers at all.
    #[serde(default)]
    pub marker_label: String,
}

/// Build the per-second table. `flicks` are attributed to the second their
/// ballistic phase starts in, and each row carries the label of the marker
/// interval it belongs to.
pub fn compute(p: &Prepared, flicks: &[Flick], intervals: &[MarkerInterval]) -> Vec<SecondRow> {
    let g = &p.grid;
    if g.is_empty() {
        return Vec::new();
    }
    let per_sec = (1.0 / g.dt).round() as usize;
    // A truncated grid ends mid-second; that partial second is not a second of
    // session, so it is not reported as one.
    let n_sec = if p.grid_truncated {
        g.len() / per_sec
    } else {
        g.len().div_ceil(per_sec)
    };
    if n_sec == 0 {
        return Vec::new();
    }

    let label_at = |second: usize| -> String {
        let cell = second * per_sec;
        intervals
            .iter()
            .rev()
            .find(|iv| cell >= iv.start_cell)
            .map(|iv| iv.label.clone())
            .unwrap_or_default()
    };

    let mut rows: Vec<SecondRow> = (0..n_sec)
        .map(|s| SecondRow {
            second: s as u64,
            t_utc_us: p.t0_utc_us + (s as i64) * 1_000_000,
            events: 0,
            distance_counts: 0.0,
            distance_cm: 0.0,
            distance_deg: 0.0,
            mean_speed_counts_s: 0.0,
            mean_speed_cm_s: 0.0,
            max_speed_counts_s: 0.0,
            max_speed_cm_s: 0.0,
            max_speed_deg_s: 0.0,
            clicks: 0,
            flicks: 0,
            moving_ms: 0,
            marker_label: label_at(s),
        })
        .collect();

    let (kx, ky) = p.aim_scale();
    for (i, (e, &us)) in p.events().iter().zip(&p.event_us).enumerate() {
        let s = (us.max(0) / 1_000_000) as usize;
        if s >= n_sec {
            continue;
        }
        let row = &mut rows[s];
        row.events += 1;
        let (dx, dy) = (e.dx as f64, e.dy as f64);
        row.distance_counts += stats::mag(dx, dy);
        if p.aim_event_ok(i) {
            row.distance_deg += stats::mag(dx * kx, dy * ky);
        }
    }

    for (s, row) in rows.iter_mut().enumerate() {
        let a = s * per_sec;
        let b = ((s + 1) * per_sec).min(g.len());
        row.distance_cm = p.counts_to_cm(row.distance_counts);
        row.mean_speed_counts_s = if b > a {
            g.speed_sum(a, b) / (b - a) as f64
        } else {
            0.0
        };
        row.mean_speed_cm_s = p.counts_to_cm(row.mean_speed_counts_s);
        row.max_speed_counts_s = g.peak_speed(a, b);
        row.max_speed_cm_s = p.counts_to_cm(row.max_speed_counts_s);
        row.max_speed_deg_s = p.peak_aim_speed(a, b);
        row.clicks = g.clicks_in(a, b) as u64;
        row.moving_ms = g.moving_cells_in(a, b, p.params.still_speed) as u64;
    }

    for f in flicks {
        let s = ((f.t_start_s.max(0.0) * 1e6).round() as i64 / 1_000_000) as usize;
        if s < n_sec {
            rows[s].flicks += 1;
        }
    }

    rows
}

const SECOND_HEADER: &str = "second,t_utc_us,events,distance_counts,distance_cm,distance_deg,\
mean_speed_counts_s,mean_speed_cm_s,max_speed_counts_s,max_speed_cm_s,max_speed_deg_s,\
clicks,flicks,moving_ms,marker_label\n";

/// Stream the per-second table into `w`. A three-hour session is ~11 000 rows;
/// a full day of them is not, so nothing is buffered into a `String` first.
pub fn write_csv<W: Write>(w: &mut W, rows: &[SecondRow]) -> io::Result<()> {
    w.write_all(SECOND_HEADER.as_bytes())?;
    for r in rows {
        writeln!(
            w,
            "{},{},{},{:.3},{:.6},{:.6},{:.3},{:.6},{:.3},{:.6},{:.6},{},{},{},{}",
            r.second,
            r.t_utc_us,
            r.events,
            r.distance_counts,
            r.distance_cm,
            r.distance_deg,
            r.mean_speed_counts_s,
            r.mean_speed_cm_s,
            r.max_speed_counts_s,
            r.max_speed_cm_s,
            r.max_speed_deg_s,
            r.clicks,
            r.flicks,
            r.moving_ms,
            csv_field(&r.marker_label),
        )?;
    }
    Ok(())
}

/// CSV rendering of the per-second table (header included).
pub fn to_csv(rows: &[SecondRow]) -> String {
    let mut buf = Vec::new();
    write_csv(&mut buf, rows).expect("writing to a Vec cannot fail");
    String::from_utf8(buf).expect("ASCII/UTF-8 output")
}

const FLICK_HEADER: &str = "index,t_start_s,t_ballistic_end_s,t_end_s,duration_ms,amplitude_deg,amplitude_counts,\
peak_velocity_deg_s,peak_velocity_counts_s,overshoot_ratio,correction_deg,settle_ms,\
time_to_click_ms,direction_deg,corrections\n";

/// Stream the detected flicks into `w`.
pub fn write_flicks_csv<W: Write>(w: &mut W, rows: &[Flick]) -> io::Result<()> {
    w.write_all(FLICK_HEADER.as_bytes())?;
    for f in rows {
        writeln!(
            w,
            "{},{:.4},{:.4},{:.4},{:.2},{:.4},{:.2},{:.2},{:.2},{:.5},{:.4},{:.2},{},{:.2},{}",
            f.index,
            f.t_start_s,
            f.t_ballistic_end_s,
            f.t_end_s,
            f.duration_ms,
            f.amplitude_deg,
            f.amplitude_counts,
            f.peak_velocity_deg_s,
            f.peak_velocity_counts_s,
            f.overshoot_ratio,
            f.correction_deg,
            f.settle_ms,
            f.time_to_click_ms
                .map_or(String::new(), |v| format!("{v:.2}")),
            f.direction_deg,
            f.corrections,
        )?;
    }
    Ok(())
}

/// CSV rendering of the detected flicks (header included).
pub fn flicks_to_csv(rows: &[Flick]) -> String {
    let mut buf = Vec::new();
    write_flicks_csv(&mut buf, rows).expect("writing to a Vec cannot fail");
    String::from_utf8(buf).expect("ASCII/UTF-8 output")
}

/// Quote a field that might contain a comma, quote or newline.
pub(crate) fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flicks;
    use crate::markers;
    use crate::testutil::{FIXTURE_DEG_PER_COUNT, StreamBuilder, prep};
    use telemouse_core::event::buttons;

    #[test]
    fn one_row_per_second_with_the_right_totals() {
        // Second 0: 1000 events of 10 counts each. Second 1: idle.
        // Second 2: 1000 events of 2 counts, plus a click.
        let mut b = StreamBuilder::new();
        b.move_ms(1000, 10, 0)
            .idle_ms(1000)
            .move_ms(999, 2, 0)
            .button(buttons::LEFT_DOWN)
            .idle_ms(100);
        let p = prep(b.into_events());
        let rows = compute(&p, &[], &[]);

        // Trailing idle produces no events, so the table ends with second 2.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].events, 1000);
        assert!((rows[0].distance_counts - 10_000.0).abs() < 1e-6);
        assert!((rows[0].distance_cm - 10_000.0 / 1600.0 * 2.54).abs() < 1e-9);
        assert!((rows[0].distance_deg - 10_000.0 * FIXTURE_DEG_PER_COUNT).abs() < 1e-6);
        assert!((rows[0].mean_speed_counts_s - 10_000.0).abs() < 20.0);
        // Savitzky–Golay overshoots a velocity step by up to 2/21 of its
        // height, so the peak sits just above the plateau where the pull ends.
        assert!(
            (rows[0].max_speed_counts_s - 10_000.0).abs() < 1_100.0,
            "{}",
            rows[0].max_speed_counts_s
        );
        assert_eq!(rows[0].moving_ms, 1000);
        assert_eq!(rows[0].clicks, 0);

        assert_eq!(rows[1].events, 0);
        assert_eq!(rows[1].distance_counts, 0.0);
        assert!(rows[1].mean_speed_counts_s < 20.0);

        assert_eq!(rows[2].events, 1000);
        assert_eq!(rows[2].clicks, 1);
        assert!((rows[2].distance_counts - 1998.0).abs() < 1e-6);

        // Timeline anchoring.
        assert_eq!(rows[0].t_utc_us, p.t0_utc_us);
        assert_eq!(rows[2].t_utc_us, p.t0_utc_us + 2_000_000);
    }

    #[test]
    fn flicks_are_attributed_to_the_second_they_start_in() {
        let mut b = StreamBuilder::new();
        b.idle_ms(1500).move_ms(20, 50, 0).idle_ms(1000);
        let p = prep(b.into_events());
        let fs = flicks::detect(&p);
        assert_eq!(fs.len(), 1);
        let rows = compute(&p, &fs, &[]);
        assert_eq!(rows[0].flicks, 0);
        assert_eq!(rows[1].flicks, 1, "flick starts at t=1.5s");
    }

    #[test]
    fn csv_round_trips_the_header_and_row_count() {
        let mut b = StreamBuilder::new();
        b.move_ms(2500, 4, 0);
        let p = prep(b.into_events());
        let rows = compute(&p, &[], &[]);
        let csv = to_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), rows.len() + 1);
        assert!(lines[0].starts_with("second,t_utc_us,events,"));
        assert!(lines[0].ends_with("marker_label"));
        assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());
    }

    #[test]
    fn rows_carry_the_marker_interval_label() {
        let mut b = StreamBuilder::new();
        // The trailing movement is what makes the session three seconds long:
        // idle time carries no events, so it never extends the grid.
        b.move_ms(500, 5, 0).idle_ms(2000).move_ms(600, 5, 0);
        let mut s = crate::testutil::loaded_from(b.into_events(), Some("cs2.exe"));
        s.markers = vec![crate::testutil::marker_at(2000, "round-2")];
        let p = crate::series::prepare(s, crate::series::Params::default());
        let iv = markers::intervals(&p, &markers::rows(&p));
        let rows = compute(&p, &[], &iv);
        assert_eq!(rows[0].marker_label, "");
        assert_eq!(rows[1].marker_label, "");
        assert_eq!(rows[2].marker_label, "round-2");
        assert!(to_csv(&rows).contains("round-2"));
    }

    #[test]
    fn flick_csv_leaves_an_absent_click_time_empty() {
        let mut b = StreamBuilder::new();
        b.move_ms(20, 50, 0).idle_ms(500);
        let p = prep(b.into_events());
        let fs = flicks::detect(&p);
        let csv = flicks_to_csv(&fs);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2);
        let cols: Vec<&str> = lines[1].split(',').collect();
        assert_eq!(cols.len(), 15);
        assert_eq!(cols[12], "", "no click, so time_to_click_ms is blank");
    }

    #[test]
    fn a_label_with_a_comma_is_quoted() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn empty_session_yields_no_rows() {
        assert!(compute(&prep(Vec::new()), &[], &[]).is_empty());
        assert_eq!(to_csv(&[]).lines().count(), 1);
    }

    /// A truncated grid must not dump the unanalyzed tail into the last row.
    #[test]
    fn a_truncated_grid_drops_the_partial_final_second() {
        let mut b = StreamBuilder::new();
        b.move_ms(4500, 5, 0);
        let params = crate::series::Params {
            max_grid_cells: 2_500, // 2.5 s of a 4.5 s session
            ..Default::default()
        };
        let p = crate::testutil::prepared_with(b.into_events(), Some("cs2.exe"), params);
        assert!(p.grid_truncated);
        let rows = compute(&p, &[], &[]);
        // Two complete seconds, not three — and no event spike at the end.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].events, 1000);
        assert_eq!(rows[1].events, 1000);
    }
}
