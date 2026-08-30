//! Turning a loaded recording into the prepared series every metric reads.
//!
//! Raw input is an irregular event stream (~1 kHz while moving, *silent* while
//! still), which makes differentiation ill-defined. So everything numeric in
//! this crate runs off a uniform grid: counts are binned into fixed 1 ms cells,
//! divided by the cell width to get counts/s, and gaps zero-fill — which is
//! physically right, since no event means no movement.
//!
//! Velocity is then Savitzky–Golay smoothed (see [`crate::savgol`]) before any
//! thresholding or further differentiation, per the plan's "smoothed; e.g.
//! Savitzky–Golay to avoid amplifying sensor noise".
//!
//! # Why the grid is stored as runs
//!
//! A three-hour session is ~10.8 M cells, and a dense grid carries six `f64`
//! lanes plus a click counter — 52 bytes a cell, 560 MB, the overwhelming
//! majority of it zeros produced by a hand that was not moving. So the grid is
//! stored as [`Run`]s: contiguous stretches that actually contain events, each
//! padded by [`Grid::pad`] cells of silence on both sides.
//!
//! The padding is what makes the sparse grid *numerically identical* to the
//! dense one rather than merely similar:
//!
//! * the pad is at least `sg_half`, so every Savitzky–Golay window over a cell
//!   that carries data is fully inside its run, and
//! * the pad is at least the tremor boxcar half-width, so the high-pass
//!   baseline over any data-bearing cell is fully inside its run too.
//!
//! Runs whose pads touch are merged, which keeps the invariant that every cell
//! within `pad` of a run is genuinely zero. Outside the runs the dense grid is
//! exactly zero in every lane, so the metric passes iterate runs and treat
//! everything else as silence. `sparse_grid_matches_a_dense_grid` in the tests
//! below pins that equality down.

use serde::{Deserialize, Serialize};
use telemouse_core::{GameSens, RawEvent, event::buttons, units};

use crate::load::LoadedSession;
use crate::savgol::SavGol;

/// Every tunable in one place; the CLI overrides a subset.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Params {
    /// Uniform resampling grid width, seconds.
    pub grid_dt_s: f64,
    /// Savitzky–Golay half-window (3 ⇒ the classic 7-point kernel).
    pub sg_half: usize,
    /// Savitzky–Golay polynomial order.
    pub sg_order: usize,
    /// "Near zero" speed, counts/s — ends a flick and bounds a movement segment.
    pub still_speed: f64,
    /// How long speed must stay below `still_speed` to count as settled, ms.
    pub still_hold_ms: u64,
    /// Flick trigger, counts/s.
    pub flick_speed: f64,
    /// Flick start → button-down search window, ms.
    pub click_window_ms: f64,
    /// Pre-click stability window, ms before button-down (start, end). The
    /// plan's "50–100 ms window before button-down" is `(100, 50)`.
    pub pre_click_lo_ms: f64,
    pub pre_click_hi_ms: f64,
    /// Longest gap between two downs of the same button that still reads as a
    /// double click, ms.
    pub double_click_max_ms: f64,
    /// Shortest run above `still_speed` that counts as a movement, ms.
    pub min_segment_ms: usize,
    /// How long a reversed velocity must persist to count as a direction
    /// change. Smoothing a movement's trailing edge leaves a one-cell negative
    /// lobe (the kernel's outer taps are negative); requiring a few
    /// milliseconds of sustained reversal rejects that artifact while any real
    /// corrective sub-movement survives.
    pub min_reversal_ms: usize,
    /// Boxcar width used as the tremor high-pass baseline, ms. 200 ms puts the
    /// filter's spectral nulls at 5/10/15 Hz, so the 8–12 Hz tremor band passes
    /// into the residual essentially unattenuated.
    pub tremor_baseline_ms: usize,
    /// Safety valve against corrupt timestamps producing an enormous grid.
    pub max_grid_cells: usize,

    /// Restrict aim-space (degree-valued) metrics to spans the capture agent
    /// flagged `pointer_locked`. Desktop-mode movement is pointer motion, not
    /// aim, so converting it to degrees is meaningless.
    pub locked_only: bool,
    /// Render the per-marker-interval breakdown in the terminal summary. The
    /// intervals are always computed and always in the JSON — this only decides
    /// whether they are printed.
    pub split_by_marker: bool,

    // --- repositioning-lift heuristic (see `crate::lifts`) ---
    /// Shortest drift that can open a lift, ms. Below this it is a nudge.
    pub lift_drift_min_ms: usize,
    /// A drift is "slow": its peak smoothed speed stays under this, counts/s.
    pub lift_drift_max_speed: f64,
    /// A drift must actually cross the pad: net displacement, counts.
    pub lift_drift_min_counts: f64,
    /// The return sweep is "fast": peak smoothed speed above this, counts/s.
    pub lift_return_min_speed: f64,
    /// Longest stillness between the drift and the return, ms — the hand is
    /// off the pad here, so the gap is short but not zero.
    pub lift_max_gap_ms: usize,
    /// How opposed the return must be: cosine of the angle between the drift
    /// and return headings, at or below this (−1 = exactly reversed).
    pub lift_opposite_cos: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            grid_dt_s: 0.001,
            sg_half: 3,
            sg_order: 2,
            still_speed: 50.0,
            still_hold_ms: 20,
            flick_speed: 800.0,
            click_window_ms: 300.0,
            pre_click_lo_ms: 100.0,
            pre_click_hi_ms: 50.0,
            double_click_max_ms: 500.0,
            min_segment_ms: 3,
            min_reversal_ms: 4,
            tremor_baseline_ms: 200,
            max_grid_cells: 32_000_000, // ~8.9 h at 1 ms
            locked_only: false,
            split_by_marker: false,
            lift_drift_min_ms: 120,
            lift_drift_max_speed: 4_000.0,
            lift_drift_min_counts: 400.0,
            lift_return_min_speed: 8_000.0,
            lift_max_gap_ms: 400,
            lift_opposite_cos: -0.6,
        }
    }
}

/// One contiguous stretch of the uniform grid that carries data, plus its
/// padding. Every lane is stored densely *within* the run; outside every run
/// the grid is exactly zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// Global index of this run's first cell.
    pub start: usize,
    /// Raw binned velocity (counts in the cell ÷ `dt`).
    pub vx: Vec<f64>,
    pub vy: Vec<f64>,
    /// Savitzky–Golay smoothed velocity.
    pub vxs: Vec<f64>,
    pub vys: Vec<f64>,
    /// `hypot(vx, vy)` — unsmoothed, for tremor high-passing.
    pub speed_raw: Vec<f64>,
    /// `hypot(vxs, vys)` — the signal every threshold is applied to.
    pub speed: Vec<f64>,
    /// Button-down transitions falling in the cell.
    pub clicks: Vec<u32>,
    /// `1 +` the local index of the last cell at or before this one whose
    /// smoothed speed exceeds the still threshold; `0` when there is none in
    /// this run. Precomputed so the click metrics never walk backwards across
    /// an idle span.
    last_move: Vec<u32>,
    /// Last moving cell (global) strictly before this run, if any.
    prev_last_move: Option<usize>,
}

impl Run {
    /// Measured off `vx`, which is the first lane allocated — the derived
    /// lanes are filled in later, and the binning pass needs `contains` to
    /// work before they exist.
    pub fn len(&self) -> usize {
        self.vx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vx.is_empty()
    }

    /// One past this run's last cell, global.
    pub fn end(&self) -> usize {
        self.start + self.len()
    }

    pub fn contains(&self, i: usize) -> bool {
        i >= self.start && i < self.end()
    }

    /// Local index of global cell `i`, if it belongs to this run.
    pub fn local(&self, i: usize) -> Option<usize> {
        self.contains(i).then(|| i - self.start)
    }

    /// The sub-slice bounds of `[a, b)` inside this run.
    pub fn clip(&self, a: usize, b: usize) -> (usize, usize) {
        let lo = a.max(self.start) - self.start;
        let hi = (b.min(self.end()).max(self.start)) - self.start;
        (lo.min(self.len()), hi.min(self.len()))
    }
}

/// The uniform 1 ms series, stored as [`Run`]s. All velocities are counts/s.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub dt: f64,
    /// Total cells the session spans, including everything the runs omit.
    n: usize,
    /// Cells of silence each run is padded by; see the module docs.
    pad: usize,
    pub runs: Vec<Run>,
}

impl Grid {
    /// An all-silent grid of `n` cells.
    pub fn empty(dt: f64, n: usize, pad: usize) -> Self {
        Self {
            dt,
            n,
            pad,
            runs: Vec::new(),
        }
    }

    /// Wrap already-computed dense lanes as a single run covering `[0, n)`.
    /// Used by the tests to compare the sparse grid against a dense one.
    #[allow(clippy::too_many_arguments)]
    pub fn from_dense(
        dt: f64,
        vx: Vec<f64>,
        vy: Vec<f64>,
        vxs: Vec<f64>,
        vys: Vec<f64>,
        speed_raw: Vec<f64>,
        speed: Vec<f64>,
        clicks: Vec<u32>,
        still_speed: f64,
    ) -> Self {
        let n = speed.len();
        if n == 0 {
            return Self::empty(dt, 0, 0);
        }
        let mut run = Run {
            start: 0,
            vx,
            vy,
            vxs,
            vys,
            speed_raw,
            speed,
            clicks,
            last_move: Vec::new(),
            prev_last_move: None,
        };
        run.last_move = last_move_table(&run.speed, still_speed);
        Self {
            dt,
            n,
            pad: 0,
            runs: vec![run],
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Cells of padding around each run.
    pub fn pad(&self) -> usize {
        self.pad
    }

    /// Cells actually materialized — the sparse grid's real footprint.
    pub fn stored_cells(&self) -> usize {
        self.runs.iter().map(Run::len).sum()
    }

    /// Seconds since session start at the start of cell `i`.
    pub fn t(&self, i: usize) -> f64 {
        i as f64 * self.dt
    }

    /// Index of the run containing `i`, if any.
    fn run_ix(&self, i: usize) -> Option<usize> {
        let k = self.runs.partition_point(|r| r.start <= i);
        (k > 0 && self.runs[k - 1].contains(i)).then(|| k - 1)
    }

    /// The run containing cell `i`, if the cell carries anything at all.
    pub fn run_at(&self, i: usize) -> Option<&Run> {
        self.run_ix(i).map(|k| &self.runs[k])
    }

    /// The first run whose cells reach at or past `i`, with its index.
    /// `None` once `i` is past the last run — everything after is silence.
    pub fn run_from(&self, i: usize) -> Option<(usize, &Run)> {
        let k = self.runs.partition_point(|r| r.end() <= i);
        self.runs.get(k).map(|r| (k, r))
    }

    /// Start of the first stretch of `need` consecutive cells at or after
    /// `from` whose smoothed speed is at or below `still` — the settle point a
    /// flick's hold window is looking for. Returns [`Grid::len`] when the
    /// session ends first. Silence between runs counts, and is skipped in O(1)
    /// rather than a cell at a time.
    pub fn quiet_start_after(&self, from: usize, need: usize, still: f64) -> usize {
        let n = self.n;
        if need == 0 {
            return from.min(n);
        }
        let mut quiet = 0usize;
        let mut k = from;
        let mut ri = self.runs.partition_point(|r| r.end() <= from);
        while k < n {
            let next_start = self.runs.get(ri).map_or(n, |r| r.start);
            if k < next_start {
                let avail = next_start - k;
                if quiet + avail >= need {
                    return k - quiet;
                }
                quiet += avail;
                k = next_start;
                continue;
            }
            let Some(r) = self.runs.get(ri) else { break };
            for j in (k - r.start)..r.len() {
                if r.speed[j] <= still {
                    quiet += 1;
                    if quiet >= need {
                        return r.start + j + 1 - need;
                    }
                } else {
                    quiet = 0;
                }
            }
            k = r.end();
            ri += 1;
        }
        n
    }

    /// Runs intersecting `[a, b)`, in order.
    pub fn runs_in(&self, a: usize, b: usize) -> &[Run] {
        if b <= a {
            return &[];
        }
        let lo = self.runs.partition_point(|r| r.end() <= a);
        let hi = self.runs.partition_point(|r| r.start < b);
        &self.runs[lo..hi.max(lo)]
    }

    fn lane(&self, i: usize, f: impl Fn(&Run, usize) -> f64) -> f64 {
        match self.run_ix(i) {
            Some(k) => {
                let r = &self.runs[k];
                f(r, i - r.start)
            }
            None => 0.0,
        }
    }

    pub fn vx(&self, i: usize) -> f64 {
        self.lane(i, |r, j| r.vx[j])
    }
    pub fn vy(&self, i: usize) -> f64 {
        self.lane(i, |r, j| r.vy[j])
    }
    pub fn vxs(&self, i: usize) -> f64 {
        self.lane(i, |r, j| r.vxs[j])
    }
    pub fn vys(&self, i: usize) -> f64 {
        self.lane(i, |r, j| r.vys[j])
    }
    pub fn speed(&self, i: usize) -> f64 {
        self.lane(i, |r, j| r.speed[j])
    }
    pub fn speed_raw(&self, i: usize) -> f64 {
        self.lane(i, |r, j| r.speed_raw[j])
    }
    pub fn clicks(&self, i: usize) -> u32 {
        match self.run_ix(i) {
            Some(k) => {
                let r = &self.runs[k];
                r.clicks[i - r.start]
            }
            None => 0,
        }
    }

    /// Materialize one lane densely — for tests and small ad-hoc analyses.
    pub fn dense(&self, f: impl Fn(&Run, usize) -> f64) -> Vec<f64> {
        let mut out = vec![0.0; self.n];
        for r in &self.runs {
            for j in 0..r.len() {
                out[r.start + j] = f(r, j);
            }
        }
        out
    }

    fn clamp(&self, a: usize, b: usize) -> (usize, usize) {
        let n = self.len();
        (a.min(n), b.min(n).max(a.min(n)))
    }

    /// Net displacement in counts over `[a, b)`.
    pub fn displacement(&self, a: usize, b: usize) -> (f64, f64) {
        let (a, b) = self.clamp(a, b);
        let mut x = 0.0;
        let mut y = 0.0;
        for r in self.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            for j in lo..hi {
                x += r.vx[j];
                y += r.vy[j];
            }
        }
        (x * self.dt, y * self.dt)
    }

    /// Path length in counts over `[a, b)` (sum of per-cell step magnitudes).
    pub fn path_length(&self, a: usize, b: usize) -> f64 {
        let (a, b) = self.clamp(a, b);
        let mut acc = 0.0;
        for r in self.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            acc += r.speed_raw[lo..hi].iter().sum::<f64>();
        }
        acc * self.dt
    }

    /// Highest smoothed speed in `[a, b)`.
    pub fn peak_speed(&self, a: usize, b: usize) -> f64 {
        let (a, b) = self.clamp(a, b);
        let mut m = 0.0f64;
        for r in self.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            for &s in &r.speed[lo..hi] {
                m = m.max(s);
            }
        }
        m
    }

    /// Clicks in `[a, b)`.
    pub fn clicks_in(&self, a: usize, b: usize) -> u32 {
        let (a, b) = self.clamp(a, b);
        let mut acc = 0u32;
        for r in self.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            acc += r.clicks[lo..hi].iter().sum::<u32>();
        }
        acc
    }

    /// Cells in `[a, b)` whose smoothed speed exceeds `threshold`.
    pub fn moving_cells_in(&self, a: usize, b: usize, threshold: f64) -> usize {
        let (a, b) = self.clamp(a, b);
        let mut acc = 0usize;
        for r in self.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            acc += r.speed[lo..hi].iter().filter(|&&s| s > threshold).count();
        }
        acc
    }

    /// Sum of the smoothed speed over `[a, b)` (silent cells contribute zero).
    pub fn speed_sum(&self, a: usize, b: usize) -> f64 {
        let (a, b) = self.clamp(a, b);
        let mut acc = 0.0;
        for r in self.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            acc += r.speed[lo..hi].iter().sum::<f64>();
        }
        acc
    }

    /// Last cell at or before `i` whose smoothed speed exceeded the still
    /// threshold `prepare` was run with. `O(log runs)` — the reverse-pass
    /// tables were built during [`prepare`].
    pub fn last_moving_cell(&self, i: usize) -> Option<usize> {
        if self.runs.is_empty() {
            return None;
        }
        let k = self.runs.partition_point(|r| r.start <= i);
        if k == 0 {
            return None;
        }
        let r = &self.runs[k - 1];
        let local = (i - r.start).min(r.len() - 1);
        match r.last_move[local] {
            0 => r.prev_last_move,
            v => Some(r.start + v as usize - 1),
        }
    }
}

/// `1 + index of the last cell at or before i above `threshold``, else 0.
fn last_move_table(speed: &[f64], threshold: f64) -> Vec<u32> {
    let mut out = vec![0u32; speed.len()];
    let mut last = 0u32;
    for (i, &s) in speed.iter().enumerate() {
        if s > threshold {
            last = i as u32 + 1;
        }
        out[i] = last;
    }
    out
}

/// A contiguous run of movement — grid cells whose smoothed speed exceeds the
/// still threshold. `end` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
}

impl Segment {
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Maximal runs above `threshold`, discarding runs shorter than `min_len`
/// cells (single-sample noise is not a movement).
///
/// Silent cells are below any sane threshold, so a movement can never span two
/// grid runs and this only ever walks the stored cells.
pub fn movement_segments(grid: &Grid, threshold: f64, min_len: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    for r in &grid.runs {
        let mut start: Option<usize> = None;
        for (j, &s) in r.speed.iter().enumerate() {
            let i = r.start + j;
            match (s > threshold, start) {
                (true, None) => start = Some(i),
                (false, Some(a)) => {
                    if i - a >= min_len {
                        out.push(Segment { start: a, end: i });
                    }
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(a) = start
            && r.end() - a >= min_len
        {
            out.push(Segment {
                start: a,
                end: r.end(),
            });
        }
    }
    out
}

/// The loaded recording plus everything derived that more than one metric
/// module needs.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub params: Params,
    /// The recording itself. `Prepared` owns it, so the 8 M-event vector is
    /// moved in rather than cloned.
    pub session: LoadedSession,
    pub session_id: String,
    /// Zero of the analysis timeline, UTC µs.
    pub t0_utc_us: i64,
    /// Wall-clock span of the *events*, first to last.
    pub duration_s: f64,
    /// Span the analysis grid actually covers. Equals `duration_s` unless the
    /// grid hit its cell cap, in which case every rate denominator uses this
    /// instead so a truncated tail cannot inflate per-minute numbers.
    pub analysis_duration_s: f64,
    pub cpi: f64,
    pub aim: GameSens,
    /// True when no per-game sensitivity was found and the fallback is in use;
    /// every degree-valued metric is then uncalibrated.
    pub aim_fallback: bool,
    pub game: Option<String>,
    /// Microseconds since `t0_utc_us`, one per event.
    ///
    /// Integer, deliberately: at 1 kHz the interval histogram and the click
    /// durations are decided by whether a gap is `<= 1.0 ms`, and
    /// `0.003_f64 / 0.001` is `2.999…`, which silently mis-bins events by a
    /// whole grid cell. Every timeline decision is made on these integers;
    /// `event_t` exists only for display and coarse arithmetic.
    pub event_us: Vec<i64>,
    /// Seconds since `t0_utc_us`, one per event.
    pub event_t: Vec<f64>,
    /// Grid cell width in microseconds.
    pub grid_dt_us: i64,
    pub grid: Grid,
    /// Set when the grid hit `Params::max_grid_cells` and was truncated.
    pub grid_truncated: bool,
    /// Movement segments at the still-speed threshold, computed once.
    pub segments: Vec<Segment>,
    /// Event index ranges `[a, b)` captured while the pointer was locked.
    pub locked_events: Vec<(usize, usize)>,
    /// The same spans as grid cell ranges.
    pub locked_cells: Vec<(usize, usize)>,
}

impl Prepared {
    /// The recording's events.
    pub fn events(&self) -> &[RawEvent] {
        &self.session.events
    }

    /// Degrees per count, horizontal and vertical.
    pub fn aim_scale(&self) -> (f64, f64) {
        (
            self.aim.sens * self.aim.yaw_coeff,
            self.aim.sens * self.aim.pitch_coeff,
        )
    }

    /// Convert a count vector to an aim-space degree vector.
    pub fn to_deg(&self, x: f64, y: f64) -> (f64, f64) {
        (
            units::counts_to_yaw_deg(x, &self.aim),
            units::counts_to_pitch_deg(y, &self.aim),
        )
    }

    /// Magnitude in degrees of a count vector.
    pub fn deg_mag(&self, x: f64, y: f64) -> f64 {
        let (a, b) = self.to_deg(x, y);
        a.hypot(b)
    }

    pub fn counts_to_cm(&self, counts: f64) -> f64 {
        units::counts_to_cm(counts, self.cpi)
    }

    /// Minutes the rate denominators use — grid coverage, not event span.
    pub fn minutes(&self) -> f64 {
        (self.analysis_duration_s / 60.0).max(f64::MIN_POSITIVE)
    }

    /// Smoothed aim-space speed at cell `i`, deg/s.
    pub fn aim_speed(&self, i: usize) -> f64 {
        let (kx, ky) = self.aim_scale();
        (self.grid.vxs(i) * kx).hypot(self.grid.vys(i) * ky)
    }

    /// Highest aim-space speed in `[a, b)`, deg/s.
    pub fn peak_aim_speed(&self, a: usize, b: usize) -> f64 {
        let (kx, ky) = self.aim_scale();
        let n = self.grid.len();
        let (a, b) = (a.min(n), b.min(n));
        let mut m = 0.0f64;
        for r in self.grid.runs_in(a, b) {
            let (lo, hi) = r.clip(a, b);
            for j in lo..hi {
                m = m.max((r.vxs[j] * kx).hypot(r.vys[j] * ky));
            }
        }
        m
    }

    /// Grid cell containing `us` microseconds since session start, clamped.
    pub fn cell_at_us(&self, us: i64) -> usize {
        if self.grid.is_empty() {
            return 0;
        }
        let i = us.max(0) / self.grid_dt_us;
        (i as usize).min(self.grid.len() - 1)
    }

    /// Grid cell width in milliseconds.
    pub fn dt_ms(&self) -> f64 {
        self.grid_dt_us as f64 / 1000.0
    }

    /// Start of cell `i`, in microseconds since session start.
    pub fn cell_start_us(&self, i: usize) -> i64 {
        i as i64 * self.grid_dt_us
    }

    /// Movement segments at the still-speed threshold.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Whether aim-space metrics should consider cell `i` at all.
    /// Always true unless `Params::locked_only` is set.
    pub fn aim_cell_ok(&self, i: usize) -> bool {
        if !self.params.locked_only {
            return true;
        }
        span_contains(&self.locked_cells, i)
    }

    /// Whether aim-space metrics should consider event `i` at all.
    pub fn aim_event_ok(&self, i: usize) -> bool {
        if !self.params.locked_only {
            return true;
        }
        span_contains(&self.locked_events, i)
    }

    /// Share of events captured while the pointer was locked.
    pub fn locked_fraction(&self) -> f64 {
        let total = self.events().len();
        if total == 0 {
            return 1.0;
        }
        let n: usize = self.locked_events.iter().map(|(a, b)| b - a).sum();
        n as f64 / total as f64
    }
}

/// Sorted, disjoint half-open index ranges — used for the pointer-locked spans
/// in both event-index and cell-index space.
pub type Spans = Vec<(usize, usize)>;

/// Binary search over sorted, disjoint `[a, b)` spans.
fn span_contains(spans: &[(usize, usize)], i: usize) -> bool {
    let k = spans.partition_point(|(a, _)| *a <= i);
    k > 0 && i < spans[k - 1].1
}

/// A dense bitmap of "this cell carries at least one event".
struct CellBits(Vec<u64>);

impl CellBits {
    fn new(n: usize) -> Self {
        Self(vec![0u64; n.div_ceil(64)])
    }
    #[inline]
    fn set(&mut self, i: usize) {
        self.0[i >> 6] |= 1u64 << (i & 63);
    }
    /// Padded, merged spans covering every set bit.
    fn spans(&self, n: usize, pad: usize) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (w, &word) in self.0.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let c = (w << 6) | bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let lo = c.saturating_sub(pad);
                let hi = (c + pad + 1).min(n);
                match out.last_mut() {
                    Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                    _ => out.push((lo, hi)),
                }
            }
        }
        out
    }
}

/// Apply a Savitzky–Golay operator to one run so the result is bit-identical
/// to applying it to the equivalent dense grid.
///
/// The run is temporarily framed by up to `half` cells of the silence that
/// really surrounds it, which is exactly the context an interior window needs;
/// where the frame would run past the ends of the session it is left off, so
/// the operator's genuine edge handling applies there — as it does densely.
pub(crate) fn sg_run(
    sg: &SavGol,
    x: &[f64],
    start: usize,
    n: usize,
    half: usize,
    dt: f64,
) -> Vec<f64> {
    let mut scratch = SgScratch::default();
    let (out, from) = sg_run_into(sg, x, start, n, half, dt, &mut scratch);
    out[from..from + x.len()].to_vec()
}

/// Reusable buffers for [`sg_run_into`]: the framed input and the framed
/// output. One pair serves every run and lane of a sweep, so the sweep
/// allocates at most twice (for the longest run) instead of three times per
/// run per lane.
#[derive(Debug, Default)]
pub(crate) struct SgScratch {
    framed: Vec<f64>,
    out: Vec<f64>,
}

/// [`sg_run`] without the per-call allocations: returns the framed output
/// buffer and the offset at which the run's `x.len()` results start, so the
/// caller reads `out[from..from + x.len()]`. Bit-identical to `sg_run`.
pub(crate) fn sg_run_into<'s>(
    sg: &SavGol,
    x: &[f64],
    start: usize,
    n: usize,
    half: usize,
    dt: f64,
    scratch: &'s mut SgScratch,
) -> (&'s [f64], usize) {
    let left = half.min(start);
    let right = half.min(n - (start + x.len()));
    if left == 0 && right == 0 {
        sg.apply_into(x, dt, &mut scratch.out);
        return (&scratch.out, 0);
    }
    scratch.framed.clear();
    scratch.framed.resize(left + x.len() + right, 0.0);
    scratch.framed[left..left + x.len()].copy_from_slice(x);
    sg.apply_into(&scratch.framed, dt, &mut scratch.out);
    (&scratch.out, left)
}

/// Build the prepared series from a loaded recording.
///
/// Takes the session by value: at 8 M events the event vector is ~200 MB, and
/// cloning it to hand the metrics a copy is the single largest avoidable
/// allocation in the pipeline.
pub fn prepare(session: LoadedSession, params: Params) -> Prepared {
    let (game, aim, aim_fallback) = session.resolve_aim();
    let anchor = session.config.anchor;

    let n_ev = session.events.len();
    let mut event_us: Vec<i64> = Vec::with_capacity(n_ev);
    let mut t0_utc_us = session.config.started_utc_us;
    for e in &session.events {
        let u = anchor.qpc_to_utc_us(e.ts_qpc);
        event_us.push(u);
        t0_utc_us = t0_utc_us.min(u);
    }
    for u in &mut event_us {
        *u -= t0_utc_us;
    }
    let event_t: Vec<f64> = event_us.iter().map(|u| *u as f64 / 1e6).collect();
    let duration_us = event_us.iter().copied().max().unwrap_or(0).max(0);
    let duration_s = duration_us as f64 / 1e6;

    let dt = params.grid_dt_s;
    let grid_dt_us = ((dt * 1e6).round() as i64).max(1);
    let wanted = if n_ev == 0 {
        0
    } else {
        (duration_us / grid_dt_us) as usize + 1
    };
    let grid_truncated = wanted > params.max_grid_cells;
    let n = wanted.min(params.max_grid_cells);
    let analysis_duration_s = if grid_truncated {
        (n as f64) * dt
    } else {
        duration_s
    };

    // Padding that makes the sparse grid numerically identical to a dense one:
    // wide enough for both the SG window and the tremor boxcar baseline.
    let pad = params.sg_half.max(params.tremor_baseline_ms / 2).max(1);

    // Pass 1: which cells carry anything.
    let mut bits = CellBits::new(n);
    for &us in &event_us {
        if us < 0 {
            continue;
        }
        let idx = (us / grid_dt_us) as usize;
        if idx < n {
            bits.set(idx);
        }
    }
    let spans = bits.spans(n, pad);
    drop(bits);

    // Pass 2: bin the counts into the runs.
    let mut runs: Vec<Run> = spans
        .iter()
        .map(|&(lo, hi)| Run {
            start: lo,
            vx: vec![0.0; hi - lo],
            vy: vec![0.0; hi - lo],
            vxs: Vec::new(),
            vys: Vec::new(),
            speed_raw: Vec::new(),
            speed: Vec::new(),
            clicks: vec![0u32; hi - lo],
            last_move: Vec::new(),
            prev_last_move: None,
        })
        .collect();

    let mut cur = 0usize;
    for (e, &us) in session.events.iter().zip(&event_us) {
        if us < 0 {
            continue;
        }
        let idx = (us / grid_dt_us) as usize;
        if idx >= n {
            continue;
        }
        if !runs[cur].contains(idx) {
            // Events are near-monotonic, so the cursor almost always holds;
            // fall back to a search when a batch arrives out of order.
            let k = runs.partition_point(|r| r.start <= idx);
            debug_assert!(k > 0 && runs[k - 1].contains(idx));
            cur = k - 1;
        }
        let r = &mut runs[cur];
        let j = idx - r.start;
        r.vx[j] += e.dx as f64;
        r.vy[j] += e.dy as f64;
        r.clicks[j] += (e.buttons & buttons::ANY_DOWN).count_ones();
    }

    // Pass 3: scale, smooth, derive.
    let inv_dt = 1.0 / dt;
    let sg = SavGol::smoother(params.sg_half, params.sg_order);
    let mut prev_last_move: Option<usize> = None;
    for r in &mut runs {
        for v in r.vx.iter_mut().chain(r.vy.iter_mut()) {
            *v *= inv_dt;
        }
        r.vxs = sg_run(&sg, &r.vx, r.start, n, params.sg_half, dt);
        r.vys = sg_run(&sg, &r.vy, r.start, n, params.sg_half, dt);
        r.speed_raw = r
            .vx
            .iter()
            .zip(&r.vy)
            .map(|(x, y)| x.hypot(*y))
            .collect();
        r.speed = r
            .vxs
            .iter()
            .zip(&r.vys)
            .map(|(x, y)| x.hypot(*y))
            .collect();
        r.last_move = last_move_table(&r.speed, params.still_speed);
        r.prev_last_move = prev_last_move;
        if let Some(&v) = r.last_move.last()
            && v > 0
        {
            prev_last_move = Some(r.start + v as usize - 1);
        }
    }

    let grid = Grid { dt, n, pad, runs };
    let segments = movement_segments(&grid, params.still_speed, params.min_segment_ms);
    let (locked_events, locked_cells) = locked_spans(&session, &event_us, grid_dt_us, n);

    Prepared {
        params,
        session_id: session.config.session_id.clone(),
        t0_utc_us,
        duration_s,
        analysis_duration_s,
        cpi: session.config.mouse_cpi,
        aim,
        aim_fallback,
        game,
        event_us,
        event_t,
        grid_dt_us,
        grid,
        grid_truncated,
        segments,
        locked_events,
        locked_cells,
        session,
    }
}

/// Event-index and cell-index spans the capture agent flagged `pointer_locked`.
/// Batches are contiguous in event order, so the event ranges fall straight out
/// of the per-batch counts.
fn locked_spans(
    session: &LoadedSession,
    event_us: &[i64],
    grid_dt_us: i64,
    n: usize,
) -> (Spans, Spans) {
    let mut ev: Spans = Vec::new();
    let mut at = 0usize;
    for b in &session.batches {
        let end = (at + b.event_count).min(event_us.len());
        if b.pointer_locked && end > at {
            match ev.last_mut() {
                Some(last) if last.1 == at => last.1 = end,
                _ => ev.push((at, end)),
            }
        }
        at = end;
    }
    // Anything past the last batch (a hand-built session) counts as locked so
    // the flag never silently drops data the loader never attributed.
    if at < event_us.len() && session.batches.is_empty() {
        ev.push((at, event_us.len()));
    }

    let cell = |i: usize| -> usize {
        let us = event_us.get(i).copied().unwrap_or(0).max(0);
        ((us / grid_dt_us) as usize).min(n.saturating_sub(1))
    };
    let mut cells: Spans = Vec::with_capacity(ev.len());
    for &(a, b) in &ev {
        if b == 0 || n == 0 {
            continue;
        }
        let lo = cell(a);
        let hi = (cell(b - 1) + 1).min(n);
        match cells.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => cells.push((lo, hi)),
        }
    }
    (ev, cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::savgol::SavGol;
    use crate::testutil::{FIXTURE_DEG_PER_COUNT, StreamBuilder, prep, prepared_from};

    /// A grid with a hand-written smoothed-speed profile, for segment tests.
    fn grid_of(speeds: &[f64]) -> Grid {
        Grid::from_dense(
            0.001,
            speeds.to_vec(),
            vec![0.0; speeds.len()],
            speeds.to_vec(),
            vec![0.0; speeds.len()],
            speeds.to_vec(),
            speeds.to_vec(),
            vec![0; speeds.len()],
            50.0,
        )
    }

    /// The dense grid `prepare` would have built, computed the obvious way.
    fn dense_reference(p: &Prepared) -> Grid {
        let n = p.grid.len();
        let dt = p.grid.dt;
        let mut vx = vec![0.0; n];
        let mut vy = vec![0.0; n];
        let mut clicks = vec![0u32; n];
        for (e, &us) in p.events().iter().zip(&p.event_us) {
            if us < 0 {
                continue;
            }
            let i = (us / p.grid_dt_us) as usize;
            if i >= n {
                continue;
            }
            vx[i] += e.dx as f64;
            vy[i] += e.dy as f64;
            clicks[i] += (e.buttons & buttons::ANY_DOWN).count_ones();
        }
        let inv = 1.0 / dt;
        for i in 0..n {
            vx[i] *= inv;
            vy[i] *= inv;
        }
        let sg = SavGol::smoother(p.params.sg_half, p.params.sg_order);
        let vxs = sg.apply(&vx, dt);
        let vys = sg.apply(&vy, dt);
        let speed_raw: Vec<f64> = (0..n).map(|i| vx[i].hypot(vy[i])).collect();
        let speed: Vec<f64> = (0..n).map(|i| vxs[i].hypot(vys[i])).collect();
        Grid::from_dense(
            dt,
            vx,
            vy,
            vxs,
            vys,
            speed_raw,
            speed,
            clicks,
            p.params.still_speed,
        )
    }

    #[test]
    fn constant_motion_lands_exactly_on_the_grid() {
        // 5 counts every ms = 5000 counts/s.
        let mut b = StreamBuilder::new();
        b.move_ms(200, 5, 0);
        let p = prep(b.into_events());
        assert_eq!(p.grid.len(), 200);
        for i in 0..200 {
            assert!((p.grid.vx(i) - 5000.0).abs() < 1e-9, "cell {i}");
        }
        // Smoothing a constant leaves it alone.
        for i in 0..200 {
            assert!((p.grid.speed(i) - 5000.0).abs() < 1e-6, "cell {i}");
        }
    }

    #[test]
    fn gaps_zero_fill() {
        let mut b = StreamBuilder::new();
        b.move_ms(10, 4, 0).idle_ms(30).move_ms(10, 4, 0);
        let p = prep(b.into_events());
        assert_eq!(p.grid.len(), 50);
        // The silent stretch reads as zero velocity.
        for i in 10..40 {
            assert!(p.grid.vx(i).abs() < 1e-9, "cell {i} should be idle");
        }
    }

    #[test]
    fn displacement_and_path_length_recover_the_counts() {
        let mut b = StreamBuilder::new();
        b.move_ms(50, 3, 4); // magnitude 5 counts per ms
        let p = prep(b.into_events());
        let (dx, dy) = p.grid.displacement(0, p.grid.len());
        assert!((dx - 150.0).abs() < 1e-6);
        assert!((dy - 200.0).abs() < 1e-6);
        assert!((p.grid.path_length(0, p.grid.len()) - 250.0).abs() < 1e-6);
    }

    #[test]
    fn aim_conversion_uses_the_dominant_games_profile() {
        let mut b = StreamBuilder::new();
        b.move_ms(10, 100, 0);
        let p = prep(b.into_events());
        assert!(!p.aim_fallback);
        assert_eq!(p.game.as_deref(), Some("cs2.exe"));
        assert!((p.deg_mag(1000.0, 0.0) - 44.0).abs() < 1e-9);
        assert!((p.aim_scale().0 - FIXTURE_DEG_PER_COUNT).abs() < 1e-12);
    }

    #[test]
    fn unknown_game_falls_back_and_flags_it() {
        let mut b = StreamBuilder::new();
        b.move_ms(10, 1, 0);
        let p = prepared_from(b.into_events(), Some("quake.exe"));
        assert!(p.aim_fallback);
        assert_eq!(p.aim.sens, 1.0);
    }

    #[test]
    fn segments_split_on_stillness() {
        let mut b = StreamBuilder::new();
        b.move_ms(40, 5, 0).idle_ms(60).move_ms(40, 5, 0);
        let p = prep(b.into_events());
        let segs = p.segments();
        assert_eq!(segs.len(), 2, "got {segs:?}");
        assert!(segs[0].len() >= 40);
        assert!(segs[1].len() >= 40);
        assert!(segs[0].end < segs[1].start);
    }

    #[test]
    fn segments_ignore_runs_shorter_than_the_minimum() {
        let mut speeds = vec![0.0; 20];
        speeds[5] = 900.0; // 1-cell blip
        speeds[6] = 900.0; // 2 cells, still under min_len 3
        speeds[12] = 900.0;
        speeds[13] = 900.0;
        speeds[14] = 900.0; // 3 cells, kept
        let g = grid_of(&speeds);
        let segs = movement_segments(&g, 50.0, 3);
        assert_eq!(segs, vec![Segment { start: 12, end: 15 }]);
        // A lower minimum keeps both.
        assert_eq!(movement_segments(&g, 50.0, 1).len(), 2);
    }

    #[test]
    fn a_segment_running_to_the_end_is_closed() {
        let g = grid_of(&[0.0, 0.0, 900.0, 900.0, 900.0, 900.0]);
        assert_eq!(
            movement_segments(&g, 50.0, 3),
            vec![Segment { start: 2, end: 6 }]
        );
    }

    #[test]
    fn clicks_bin_into_their_cell() {
        let mut b = StreamBuilder::new();
        b.move_ms(5, 1, 0)
            .button(buttons::LEFT_DOWN)
            .move_ms(5, 1, 0)
            .button(buttons::LEFT_UP);
        let p = prep(b.into_events());
        assert_eq!(p.grid.clicks_in(0, p.grid.len()), 1);
        assert_eq!(p.grid.clicks(5), 1);
    }

    #[test]
    fn empty_session_prepares_without_panicking() {
        let p = prep(Vec::new());
        assert!(p.grid.is_empty());
        assert_eq!(p.duration_s, 0.0);
        assert!(p.segments().is_empty());
        assert_eq!(p.cell_at_us(1_000_000), 0);
    }

    /// Regression: `0.003 / 0.001` floors to 2, not 3. Binning on `f64`
    /// seconds silently drops every third event into the previous cell and
    /// turns a constant velocity into an alternating one.
    #[test]
    fn grid_binning_is_exact_at_every_millisecond() {
        let mut b = StreamBuilder::new();
        b.move_ms(1000, 7, 0);
        let p = prep(b.into_events());
        assert_eq!(p.grid.len(), 1000);
        for i in 0..1000 {
            assert!(
                (p.grid.vx(i) - 7000.0).abs() < 1e-9,
                "cell {i} holds {}",
                p.grid.vx(i)
            );
        }
        // And the integer timeline is exactly 1000µs per event.
        for w in p.event_us.windows(2) {
            assert_eq!(w[1] - w[0], 1000);
        }
    }

    /// The sparse grid is not an approximation: every lane matches the dense
    /// grid exactly, on a fixture that mixes long idle spans with movement at
    /// both ends of the session (so run padding is exercised clamped against
    /// the session boundary and free-standing in the middle).
    #[test]
    fn sparse_grid_matches_a_dense_grid() {
        let mut b = StreamBuilder::new();
        b.move_ms(40, 30, -12) // starts at cell 0 — left pad clamps
            .idle_ms(900)
            .move_ms(15, -80, 4)
            .idle_ms(50) // short gap: these two runs must merge
            .move_ms(25, 5, 5)
            .idle_ms(2000)
            .button(buttons::LEFT_DOWN)
            .idle_ms(300)
            .move_ms(60, 1, -1); // ends at the last cell — right pad clamps
        let p = prep(b.into_events());
        let dense = dense_reference(&p);
        let n = p.grid.len();
        assert!(n > 3000, "{n}");

        // The whole point: far fewer cells are actually stored.
        assert!(
            p.grid.stored_cells() * 2 < n,
            "stored {} of {n}",
            p.grid.stored_cells()
        );
        assert!(p.grid.runs.len() >= 3, "{} runs", p.grid.runs.len());

        for i in 0..n {
            for (name, got, want) in [
                ("vx", p.grid.vx(i), dense.vx(i)),
                ("vy", p.grid.vy(i), dense.vy(i)),
                ("vxs", p.grid.vxs(i), dense.vxs(i)),
                ("vys", p.grid.vys(i), dense.vys(i)),
                ("speed_raw", p.grid.speed_raw(i), dense.speed_raw(i)),
                ("speed", p.grid.speed(i), dense.speed(i)),
            ] {
                assert_eq!(got, want, "{name} differs at cell {i}");
            }
            assert_eq!(p.grid.clicks(i), dense.clicks(i), "clicks at {i}");
        }

        // Range queries agree too.
        assert_eq!(
            movement_segments(&p.grid, 50.0, 3),
            movement_segments(&dense, 50.0, 3)
        );
        // Sums over a range are compared with a tolerance, not bit-for-bit:
        // the sparse form adds each run's cells and then the runs, so the
        // *grouping* of an f64 addition chain differs from the dense one by a
        // couple of ULP. Every per-cell value above is exactly equal, which is
        // the property the padding actually guarantees.
        let (sx, sy) = p.grid.displacement(0, n);
        let (dx, dy) = dense.displacement(0, n);
        assert!((sx - dx).abs() < 1e-6 && (sy - dy).abs() < 1e-6);
        assert!((p.grid.path_length(0, n) - dense.path_length(0, n)).abs() < 1e-6);
        assert_eq!(p.grid.clicks_in(0, n), dense.clicks_in(0, n));
        assert_eq!(p.grid.peak_speed(100, 2000), dense.peak_speed(100, 2000));
    }

    /// The precomputed reverse pass must answer exactly what a backwards
    /// cell-by-cell walk would, including across idle spans and before any
    /// movement has happened.
    #[test]
    fn last_moving_cell_matches_a_backwards_scan() {
        let mut b = StreamBuilder::new();
        b.idle_ms(200)
            .move_ms(30, 40, 0)
            .idle_ms(700)
            .move_ms(20, -30, 10)
            .idle_ms(400);
        let p = prep(b.into_events());
        let speed = p.grid.dense(|r, j| r.speed[j]);
        let naive = |i: usize| -> Option<usize> {
            (0..=i).rev().find(|&k| speed[k] > p.params.still_speed)
        };
        for i in 0..p.grid.len() {
            assert_eq!(p.grid.last_moving_cell(i), naive(i), "cell {i}");
        }
    }
}
