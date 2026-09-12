//! Trigger discipline: what the hand was doing around each button-down.
//!
//! Raw input reports button transitions as bitfield edges, and one event can
//! carry several at once (a fast double-click can pack a down and an up into a
//! single `WM_INPUT`), so every metric here iterates the five buttons rather
//! than assuming one transition per event.
//!
//! * **Pre-click stability** — mean speed over the 50–100 ms before the down.
//!   Shooting while still versus spraying while dragging.
//! * **Click-to-still latency** — last moment the mouse was above the still
//!   threshold, back to the click. Zero means the shot went off mid-movement.
//! * **Hold duration** — down→up per button, matched as a stack of one.
//! * **Double-click interval** — successive downs of the same button inside
//!   the double-click window.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use telemouse_core::event::buttons;

use crate::kinematics::mean_speed_in_window;
use crate::series::Prepared;
use crate::stats::Summary;

/// Which physical button a transition belongs to. An enum rather than a
/// `String`: a session runs to tens of thousands of clicks, and every one of
/// them was allocating a five-byte heap string to say "left".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl Button {
    pub fn as_str(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
            Button::X1 => "x1",
            Button::X2 => "x2",
        }
    }
}

impl fmt::Display for Button {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// `(down bit, up bit, button)` for every button on the wire.
pub const BUTTONS: [(u16, u16, Button); 5] = [
    (buttons::LEFT_DOWN, buttons::LEFT_UP, Button::Left),
    (buttons::RIGHT_DOWN, buttons::RIGHT_UP, Button::Right),
    (buttons::MIDDLE_DOWN, buttons::MIDDLE_UP, Button::Middle),
    (buttons::X1_DOWN, buttons::X1_UP, Button::X1),
    (buttons::X2_DOWN, buttons::X2_UP, Button::X2),
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickEvent {
    pub index: usize,
    pub button: Button,
    /// Seconds since session start.
    pub t_s: f64,
    /// Mean speed in the pre-click window, counts/s.
    pub pre_click_speed_counts_s: f64,
    pub pre_click_speed_cm_s: f64,
    /// Last movement above the still threshold → this click, ms.
    /// `None` when nothing moved beforehand.
    pub click_to_still_ms: Option<f64>,
    /// Down → matching up, ms. `None` if the up never arrived.
    pub hold_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ButtonStats {
    pub button: Button,
    pub downs: usize,
    pub ups: usize,
    pub hold_ms: Summary,
    pub double_click_interval_ms: Summary,
    pub double_clicks: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClickReport {
    pub total_clicks: usize,
    pub clicks_per_min: f64,
    pub pre_click_speed_counts_s: Summary,
    pub pre_click_speed_cm_s: Summary,
    /// Share of clicks taken with the hand essentially still.
    pub still_click_fraction: f64,
    pub click_to_still_ms: Summary,
    pub hold_ms: Summary,
    pub double_click_interval_ms: Summary,
    pub double_clicks: usize,
    /// Downs with no matching up before the recording ended.
    pub unmatched_downs: usize,
    pub per_button: Vec<ButtonStats>,
    pub clicks: Vec<ClickEvent>,
}

/// Running per-button counters while walking the event stream.
#[derive(Debug, Default)]
struct Tally {
    downs: usize,
    ups: usize,
    holds_ms: Vec<f64>,
    double_gaps_ms: Vec<f64>,
}

/// Last moment at or before `us` where the smoothed speed exceeded the still
/// threshold, in microseconds since session start.
///
/// The answer comes out of the reverse pass [`crate::series::prepare`] built,
/// so a click after a two-minute idle stretch costs a binary search rather than
/// 120 000 cells of walking backwards.
fn last_movement_before(p: &Prepared, us: i64) -> Option<i64> {
    if p.grid.is_empty() {
        return None;
    }
    p.grid
        .last_moving_cell(p.cell_at_us(us))
        .map(|i| p.cell_start_us(i))
}

pub fn compute(p: &Prepared) -> ClickReport {
    let prm = &p.params;
    let mut clicks: Vec<ClickEvent> = Vec::new();
    // button -> (index into `clicks` awaiting an up, its timestamp)
    let mut pending: BTreeMap<Button, (usize, i64)> = BTreeMap::new();
    let mut last_down: BTreeMap<Button, i64> = BTreeMap::new();
    let mut per_button: BTreeMap<Button, Tally> = BTreeMap::new();

    // Durations come off the integer µs timeline so a 40 ms hold reports as
    // 40.0, not 40.000000000000036. Only the events inside the analysed span
    // are counted, because `clicks_per_min` divides by that span.
    for (i, e) in p.analysed_events().iter().enumerate() {
        if e.buttons & buttons::MASK == 0 {
            continue;
        }
        let us = p.event_us[i];
        for (down, up, name) in BUTTONS {
            let slot = per_button.entry(name).or_default();
            if e.buttons & down != 0 {
                slot.downs += 1;
                // Button transitions are sparse, so converting the handful of
                // timestamps used here is much cheaper than retaining a second
                // 8-byte timestamp vector for every motion event.
                let t = us as f64 / 1e6;
                let pre = mean_speed_in_window(
                    p,
                    t - prm.pre_click_lo_ms / 1000.0,
                    t - prm.pre_click_hi_ms / 1000.0,
                )
                .unwrap_or(0.0);
                let idx = clicks.len();
                clicks.push(ClickEvent {
                    index: idx,
                    button: name,
                    t_s: t,
                    pre_click_speed_counts_s: pre,
                    pre_click_speed_cm_s: p.counts_to_cm(pre),
                    click_to_still_ms: last_movement_before(p, us)
                        .map(|m| ((us - m).max(0)) as f64 / 1000.0),
                    hold_ms: None,
                });
                if let Some(prev) = last_down.insert(name, us) {
                    let gap = (us - prev) as f64 / 1000.0;
                    if gap <= prm.double_click_max_ms {
                        per_button.get_mut(&name).unwrap().double_gaps_ms.push(gap);
                    }
                }
                // A down while already held (no up seen) replaces the pending
                // one; the earlier press simply never gets a hold duration.
                pending.insert(name, (idx, us));
            }
            if e.buttons & up != 0 {
                let slot = per_button.get_mut(&name).unwrap();
                slot.ups += 1;
                if let Some((idx, us_down)) = pending.remove(&name) {
                    let hold = (us - us_down) as f64 / 1000.0;
                    clicks[idx].hold_ms = Some(hold);
                    per_button.get_mut(&name).unwrap().holds_ms.push(hold);
                }
            }
        }
    }

    let pre: Vec<f64> = clicks.iter().map(|c| c.pre_click_speed_counts_s).collect();
    let pre_cm: Vec<f64> = clicks.iter().map(|c| c.pre_click_speed_cm_s).collect();
    let cts: Vec<f64> = clicks.iter().filter_map(|c| c.click_to_still_ms).collect();
    let holds: Vec<f64> = clicks.iter().filter_map(|c| c.hold_ms).collect();
    let dbl: Vec<f64> = per_button
        .values()
        .flat_map(|v| v.double_gaps_ms.iter().copied())
        .collect();
    let still = pre.iter().filter(|&&s| s <= prm.still_speed).count();

    let per_button: Vec<ButtonStats> = per_button
        .into_iter()
        .filter(|(_, v)| v.downs > 0 || v.ups > 0)
        .map(|(name, v)| ButtonStats {
            button: name,
            downs: v.downs,
            ups: v.ups,
            double_clicks: v.double_gaps_ms.len(),
            hold_ms: Summary::of_vec(v.holds_ms),
            double_click_interval_ms: Summary::of_vec(v.double_gaps_ms),
        })
        .collect();

    let double_clicks = dbl.len();
    ClickReport {
        total_clicks: clicks.len(),
        clicks_per_min: clicks.len() as f64 / p.minutes(),
        pre_click_speed_counts_s: Summary::of_vec(pre),
        pre_click_speed_cm_s: Summary::of_vec(pre_cm),
        still_click_fraction: if clicks.is_empty() {
            0.0
        } else {
            still as f64 / clicks.len() as f64
        },
        click_to_still_ms: Summary::of_vec(cts),
        hold_ms: Summary::of_vec(holds),
        double_click_interval_ms: Summary::of_vec(dbl),
        double_clicks,
        unmatched_downs: pending.len(),
        per_button,
        clicks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{StreamBuilder, prep};

    #[test]
    fn hold_durations_and_double_click_interval_are_exact() {
        let mut b = StreamBuilder::new();
        b.idle_ms(100)
            .button(buttons::LEFT_DOWN) // t = 100ms
            .idle_ms(44)
            .button(buttons::LEFT_UP) // t = 145ms -> hold 45ms
            .idle_ms(154)
            .button(buttons::LEFT_DOWN) // t = 300ms -> 200ms after the first
            .idle_ms(29)
            .button(buttons::LEFT_UP) // t = 330ms -> hold 30ms
            .idle_ms(200);
        let r = compute(&prep(b.into_events()));

        assert_eq!(r.total_clicks, 2);
        assert_eq!(r.unmatched_downs, 0);
        let holds: Vec<f64> = r.clicks.iter().map(|c| c.hold_ms.unwrap()).collect();
        assert!((holds[0] - 45.0).abs() < 1e-6, "{holds:?}");
        assert!((holds[1] - 30.0).abs() < 1e-6, "{holds:?}");
        assert_eq!(r.double_clicks, 1);
        assert!(
            (r.double_click_interval_ms.median - 200.0).abs() < 1e-6,
            "{:?}",
            r.double_click_interval_ms
        );
        assert_eq!(r.per_button.len(), 1);
        assert_eq!(r.per_button[0].button, Button::Left);
        assert_eq!(r.per_button[0].downs, 2);
        assert_eq!(r.per_button[0].ups, 2);
    }

    #[test]
    fn a_slow_pair_of_clicks_is_not_a_double_click() {
        let mut b = StreamBuilder::new();
        b.button(buttons::LEFT_DOWN)
            .idle_ms(19)
            .button(buttons::LEFT_UP)
            .idle_ms(900)
            .button(buttons::LEFT_DOWN)
            .idle_ms(19)
            .button(buttons::LEFT_UP)
            .idle_ms(100);
        let r = compute(&prep(b.into_events()));
        assert_eq!(r.total_clicks, 2);
        assert_eq!(r.double_clicks, 0);
        assert!(r.double_click_interval_ms.is_empty());
    }

    /// The distinguishing test from the brief: a shot taken standing still
    /// versus one taken mid-drag.
    #[test]
    fn pre_click_stability_separates_a_still_click_from_a_moving_one() {
        // Still: pull, stop, wait well past the 100ms window, then click.
        let mut still = StreamBuilder::new();
        still
            .move_ms(100, 20, 0)
            .idle_ms(200)
            .button(buttons::LEFT_DOWN)
            .idle_ms(50)
            .button(buttons::LEFT_UP)
            .idle_ms(100);
        let s = compute(&prep(still.into_events()));
        assert_eq!(s.total_clicks, 1);
        assert!(
            s.clicks[0].pre_click_speed_counts_s < 1.0,
            "{}",
            s.clicks[0].pre_click_speed_counts_s
        );
        assert_eq!(s.still_click_fraction, 1.0);
        // Movement stopped ~200ms before the shot.
        let cts = s.clicks[0].click_to_still_ms.unwrap();
        assert!((cts - 200.0).abs() < 10.0, "click-to-still {cts}");

        // Moving: click in the middle of a 20_000 counts/s drag.
        let mut moving = StreamBuilder::new();
        moving.move_ms(200, 20, 0);
        moving.push(20, 0, buttons::LEFT_DOWN, 0);
        moving.move_ms(100, 20, 0).idle_ms(100);
        let m = compute(&prep(moving.into_events()));
        assert_eq!(m.total_clicks, 1);
        assert!(
            (m.clicks[0].pre_click_speed_counts_s - 20_000.0).abs() < 200.0,
            "{}",
            m.clicks[0].pre_click_speed_counts_s
        );
        assert_eq!(m.still_click_fraction, 0.0);
        // The mouse was moving at the instant of the click.
        assert_eq!(m.clicks[0].click_to_still_ms, Some(0.0));
        // ...and in cm/s: 20_000 counts/s at 1600 CPI = 31.75 cm/s.
        assert!((m.clicks[0].pre_click_speed_cm_s - 31.75).abs() < 0.5);
    }

    #[test]
    fn several_buttons_are_tracked_independently() {
        let mut b = StreamBuilder::new();
        b.button(buttons::LEFT_DOWN)
            .idle_ms(9)
            .button(buttons::RIGHT_DOWN)
            .idle_ms(9)
            .button(buttons::LEFT_UP) // left held 20ms
            .idle_ms(49)
            .button(buttons::RIGHT_UP) // right held 70ms
            .idle_ms(100);
        let r = compute(&prep(b.into_events()));
        assert_eq!(r.total_clicks, 2);
        assert_eq!(r.per_button.len(), 2);
        let by = |name: Button| {
            r.per_button
                .iter()
                .find(|s| s.button == name)
                .unwrap()
                .hold_ms
                .median
        };
        assert!((by(Button::Left) - 20.0).abs() < 1e-6);
        assert!((by(Button::Right) - 60.0).abs() < 1e-6);
    }

    #[test]
    fn a_down_with_no_up_is_reported_as_unmatched() {
        let mut b = StreamBuilder::new();
        b.button(buttons::LEFT_DOWN).idle_ms(100);
        let r = compute(&prep(b.into_events()));
        assert_eq!(r.total_clicks, 1);
        assert_eq!(r.unmatched_downs, 1);
        assert_eq!(r.clicks[0].hold_ms, None);
        assert!(r.hold_ms.is_empty());
    }

    #[test]
    fn a_single_event_can_carry_a_down_and_an_up() {
        let mut b = StreamBuilder::new();
        b.button(buttons::LEFT_DOWN)
            .idle_ms(4)
            .push(0, 0, buttons::LEFT_UP | buttons::RIGHT_DOWN, 0)
            .idle_ms(9)
            .button(buttons::RIGHT_UP)
            .idle_ms(50);
        let r = compute(&prep(b.into_events()));
        assert_eq!(r.total_clicks, 2);
        assert_eq!(r.unmatched_downs, 0);
        assert!((r.clicks[0].hold_ms.unwrap() - 5.0).abs() < 1e-6);
        assert!((r.clicks[1].hold_ms.unwrap() - 10.0).abs() < 1e-6);
    }

    #[test]
    fn clicks_per_minute_scales_with_duration() {
        let mut b = StreamBuilder::new();
        for _ in 0..10 {
            b.button(buttons::LEFT_DOWN)
                .idle_ms(19)
                .button(buttons::LEFT_UP)
                .idle_ms(979);
        }
        let p = prep(b.into_events());
        let r = compute(&p);
        assert_eq!(r.total_clicks, 10);
        // The last up lands at 9.02s, so the rate is 10 / 9.02 min-normalized.
        let expected = 10.0 / (p.duration_s / 60.0);
        assert!((r.clicks_per_min - expected).abs() < 1e-9);
        assert!(
            r.clicks_per_min > 60.0 && r.clicks_per_min < 70.0,
            "{}",
            r.clicks_per_min
        );
    }

    #[test]
    fn no_clicks_is_a_zeroed_report() {
        let mut b = StreamBuilder::new();
        b.move_ms(50, 5, 0);
        let r = compute(&prep(b.into_events()));
        assert_eq!(r.total_clicks, 0);
        assert_eq!(r.clicks_per_min, 0.0);
        assert_eq!(r.still_click_fraction, 0.0);
        assert!(r.per_button.is_empty());
    }

    /// The reverse-pass lookup must agree with the cell-by-cell walk it
    /// replaced, including across a long idle span and before any movement.
    #[test]
    fn click_to_still_matches_a_backwards_walk() {
        let mut b = StreamBuilder::new();
        b.button(buttons::LEFT_DOWN) // no movement yet at all
            .idle_ms(50)
            .move_ms(30, 25, 0)
            .idle_ms(900)
            .button(buttons::LEFT_DOWN) // ~900ms after movement stopped
            .idle_ms(20)
            .move_ms(10, 40, 0)
            .push(40, 0, buttons::LEFT_DOWN, 0) // mid-movement
            .idle_ms(200);
        let p = prep(b.into_events());
        let speed = p.grid.dense(|r, j| r.speed[j]);
        let naive = |us: i64| -> Option<f64> {
            let i = p.cell_at_us(us);
            (0..=i)
                .rev()
                .find(|&k| speed[k] > p.params.still_speed)
                .map(|k| ((us - p.cell_start_us(k)).max(0)) as f64 / 1000.0)
        };
        let r = compute(&p);
        assert_eq!(r.total_clicks, 3);
        for c in &r.clicks {
            let us = (c.t_s * 1e6).round() as i64;
            assert_eq!(c.click_to_still_ms, naive(us), "click at {}s", c.t_s);
        }
        assert_eq!(r.clicks[0].click_to_still_ms, None);
        assert!(r.clicks[1].click_to_still_ms.unwrap() > 800.0);
        assert_eq!(r.clicks[2].click_to_still_ms, Some(0.0));
    }

    #[test]
    fn button_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Button::Middle).unwrap(),
            "\"middle\""
        );
        assert_eq!(
            serde_json::from_str::<Button>("\"x2\"").unwrap(),
            Button::X2
        );
        assert_eq!(format!("{:<8}|", Button::Left), "left    |");
    }
}
