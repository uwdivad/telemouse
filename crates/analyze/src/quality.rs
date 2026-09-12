//! Data quality — "trust the numbers".
//!
//! Everything above this module assumes the recording is a faithful sample of
//! the hand. This one checks that assumption: a 1 kHz mouse should produce
//! inter-event intervals piled up at 1 ms, no ring-buffer drops, strictly
//! increasing timestamps, an unbroken batch sequence, and a pointer that was
//! actually locked to the game. Anything else is reported loudly rather than
//! quietly biasing the metrics.

use serde::{Deserialize, Serialize};
use telemouse_core::recordings::SessionMeta;

use crate::series::Prepared;
use crate::stats::{self, Summary};

/// Upper edges of the inter-event interval histogram, ms.
const INTERVAL_EDGES: [f64; 9] = [0.5, 1.0, 2.0, 4.0, 8.0, 10.0, 20.0, 50.0, 100.0];
/// Intervals longer than this count as a gap (the plan's "gaps = stalls").
pub const GAP_MS: f64 = 10.0;
/// Below this share of events on one process, the session is really two.
pub const MIN_DOMINANT_GAME_SHARE: f64 = 0.8;
/// Below this share of pointer-locked events, degree metrics are suspect.
pub const MIN_LOCKED_FRACTION: f64 = 0.9;
/// Under this much analysed data, every per-minute number is an extrapolation.
pub const MIN_ANALYSIS_S: f64 = 1.0;

/// Intervals above this are the hand resting, not the mouse reporting, so the
/// polling-rate estimate ignores them.
const POLL_MAX_MS: f64 = 20.0;
/// Interval histogram resolution for the polling estimate. 0.125 ms separates
/// every rate a mouse actually runs at (8000/4000/2000/1000/500/250/125 Hz)
/// while staying coarse enough that USB jitter does not smear the mode.
const POLL_BUCKET_MS: f64 = 0.125;
/// `POLL_MAX_MS / POLL_BUCKET_MS`, plus the bucket that holds exactly 20 ms.
const POLL_BUCKETS: usize = 161;
/// An interval counts as "at the polling rate" within this much of the mode.
const POLL_TOLERANCE: f64 = 0.10;
/// Minutes with fewer qualifying intervals than this do not get their own
/// rate: a minute the hand spent still says nothing about the mouse.
const POLL_MIN_MINUTE_SAMPLES: usize = 100;

/// What the capture agent's `<session_id>.meta.json` says about the run that
/// produced this recording — the half of the story the JSONL cannot tell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarSummary {
    /// How the run ended, as [`telemouse_core::recordings::ExitReason`].
    pub exit: String,
    /// The run never wrote a final reason: it was killed, or is still going.
    pub unfinished: bool,
    /// Every worker thread joined without panicking.
    pub clean: bool,
    /// `debug` or `release` — a debug capture drops events a release one does
    /// not, which is the first thing a surprising drop count needs ruled out.
    pub capture_profile: String,
    /// Report rate the agent measured, Hz.
    pub poll_hz: Option<f64>,
    /// Events the agent counted. Compared against what the file holds.
    pub events: u64,
}

impl SidecarSummary {
    fn of(meta: &SessionMeta) -> Self {
        Self {
            exit: meta.exit.clone(),
            unfinished: meta.is_unfinished(),
            clean: meta.clean,
            capture_profile: meta.capture_profile.clone(),
            poll_hz: meta.poll_hz,
            events: meta.events,
        }
    }
}

/// The mouse's report rate, estimated from the inter-event intervals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Polling {
    /// Modal interval, as a rate. `None` when nothing moved.
    pub hz: Option<f64>,
    /// Share of qualifying intervals within ±10% of the mode: 1.0 is a mouse
    /// reporting like a metronome, 0.5 is one that is not.
    pub stability: f64,
    /// Lowest and highest per-minute modal rate, over the minutes that carried
    /// enough movement to have one. A 1000 Hz mouse that spends a minute at
    /// 500 Hz is throttling, and the session median hides it.
    pub hz_min: Option<f64>,
    pub hz_max: Option<f64>,
    /// Intervals the estimate is based on (those at or under 20 ms).
    pub samples: usize,
}

/// Fixed-width interval histogram over `[0, POLL_MAX_MS]`, keeping each
/// bucket's sum so the mode can be reported as the mean of its members rather
/// than as a bucket edge.
#[derive(Debug, Clone)]
struct PollHist {
    counts: Vec<u32>,
    sums: Vec<f64>,
    n: usize,
}

impl PollHist {
    fn new() -> Self {
        Self {
            counts: vec![0; POLL_BUCKETS],
            sums: vec![0.0; POLL_BUCKETS],
            n: 0,
        }
    }

    #[inline]
    fn push(&mut self, ms: f64) {
        let b = ((ms / POLL_BUCKET_MS) as usize).min(POLL_BUCKETS - 1);
        self.counts[b] += 1;
        self.sums[b] += ms;
        self.n += 1;
    }

    /// Mean interval of the fullest bucket, ms. Ties go to the shorter
    /// interval, so a mouse split evenly between two buckets reads as the
    /// faster of the two rather than by iteration order.
    fn mode_ms(&self) -> Option<f64> {
        let (b, &count) = self
            .counts
            .iter()
            .enumerate()
            .max_by_key(|&(i, &c)| (c, std::cmp::Reverse(i)))?;
        (count > 0).then(|| self.sums[b] / count as f64)
    }

    fn mode_hz(&self) -> Option<f64> {
        self.mode_ms().filter(|ms| *ms > 0.0).map(|ms| 1000.0 / ms)
    }
}

/// Estimate the report rate from `event_us` (µs since session start, ascending
/// apart from counted violations).
fn polling_stats(event_us: &[i64]) -> Polling {
    let mut session = PollHist::new();
    let mut minutes: Vec<PollHist> = Vec::new();
    for w in event_us.windows(2) {
        let d = w[1] - w[0];
        if d <= 0 {
            continue;
        }
        let ms = d as f64 / 1000.0;
        if ms > POLL_MAX_MS {
            continue;
        }
        session.push(ms);
        let m = (w[0].max(0) / 60_000_000) as usize;
        if minutes.len() <= m {
            minutes.resize_with(m + 1, PollHist::new);
        }
        minutes[m].push(ms);
    }

    let Some(modal_ms) = session.mode_ms().filter(|ms| *ms > 0.0) else {
        return Polling::default();
    };
    // Stability is defined on the intervals themselves, not on the buckets,
    // so it takes a second look rather than a sum of neighbouring counts.
    let (lo, hi) = (
        modal_ms * (1.0 - POLL_TOLERANCE),
        modal_ms * (1.0 + POLL_TOLERANCE),
    );
    let mut within = 0usize;
    for w in event_us.windows(2) {
        let d = w[1] - w[0];
        if d <= 0 {
            continue;
        }
        let ms = d as f64 / 1000.0;
        if ms <= POLL_MAX_MS && ms >= lo && ms <= hi {
            within += 1;
        }
    }

    let rates: Vec<f64> = minutes
        .iter()
        .filter(|h| h.n >= POLL_MIN_MINUTE_SAMPLES)
        .filter_map(PollHist::mode_hz)
        .collect();
    Polling {
        hz: Some(1000.0 / modal_ms),
        stability: within as f64 / session.n as f64,
        hz_min: rates.iter().copied().reduce(f64::min),
        hz_max: rates.iter().copied().reduce(f64::max),
        samples: session.n,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntervalBucket {
    pub label: String,
    pub lo_ms: f64,
    pub hi_ms: Option<f64>,
    pub count: usize,
    pub fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    pub event_count: usize,
    /// Events inside the analysed span — `event_count` unless the grid was
    /// truncated. This, not `event_count`, is what `events_per_s` divides.
    pub events_analyzed: usize,
    pub batch_count: usize,
    pub duration_s: f64,
    /// Span the grid actually covers; differs from `duration_s` only when the
    /// grid was truncated, and is what every rate here is divided by.
    pub analysis_duration_s: f64,
    pub events_per_s: f64,
    pub mean_events_per_batch: f64,

    /// Sum of `drops_since_last` across batches — should be zero.
    pub ring_drops: u64,
    /// Batches that reported a nonzero drop count.
    pub batches_with_drops: usize,
    /// Batches missing from the `seq_no` sequence — loss downstream of the
    /// capture agent's own ring buffer.
    pub lost_batches: u64,
    /// Places where the sequence jumped rather than incremented.
    pub seq_gaps: usize,
    /// Events whose timestamp went backwards relative to the previous event.
    pub monotonicity_violations: usize,
    /// JSONL lines that failed to parse.
    pub bad_lines: usize,
    /// 1-based file line numbers of the first and last unparseable line, and
    /// whether they are confined to the tail (a killed agent) rather than
    /// reaching into the body of the recording (corruption).
    pub bad_line_first: Option<u64>,
    pub bad_line_last: Option<u64>,
    pub bad_lines_tail_only: bool,
    /// What the parser said about the first one.
    pub bad_line_error: Option<String>,
    /// Absolute-motion `WM_INPUT` frames the agent saw and discarded.
    pub abs_frames: u64,

    /// Batch assembly latency: the envelope's anchor timestamp minus the UTC
    /// its own first event maps to. Milliseconds.
    pub batch_latency_ms: Summary,

    pub interval_histogram: Vec<IntervalBucket>,
    pub median_interval_ms: f64,
    pub p99_interval_ms: f64,
    pub max_interval_ms: f64,
    /// Share of intervals at or under 1 ms. This is *not* a polling-rate
    /// measurement: a still hand reports nothing, so a long idle stretch drags
    /// it down, and an 8000 Hz mouse pushes it to 100% while moving. Read it
    /// as "how much of this recording came in at 1 kHz or better"; read
    /// [`QualityReport::polling`] for the rate itself.
    pub pct_within_1ms: f64,
    /// The mouse's report rate, estimated from the intervals it did produce.
    pub polling: Polling,
    pub gaps_over_10ms: usize,

    /// Process that owned the most events, and its share of all of them.
    pub dominant_game: Option<String>,
    pub dominant_game_share: f64,
    /// Share of events captured while the pointer was locked to the game.
    pub locked_fraction: f64,
    /// Whether degree metrics were restricted to the locked spans.
    pub locked_only: bool,

    /// HID devices the session config listed, and the `device_ix` values the
    /// events actually carried.
    pub devices: Vec<String>,
    pub device_indices: Vec<u8>,
    /// Half-width of the QPC↔UTC read sandwich at anchor time, µs, when the
    /// capture agent measured it.
    pub anchor_uncertainty_us: Option<i64>,

    /// The session had no per-game sensitivity, so degree metrics are
    /// uncalibrated.
    pub aim_profile_missing: bool,
    /// The 1 ms grid hit its size cap and the tail was dropped.
    pub grid_truncated: bool,
    /// The capture agent's metadata sidecar, when it wrote one.
    pub sidecar: Option<SidecarSummary>,
    /// Whether anything above warrants a second look.
    pub clean: bool,
}

impl QualityReport {
    /// How to read the numbers, when there is too little data for them to
    /// mean much: no events at all, or under [`MIN_ANALYSIS_S`]. These are
    /// not defects in the recording (a ten-second smoke test is a valid
    /// recording), so they do not make the session unclean; they are printed
    /// above the notes so a zero is not mistaken for a result.
    pub fn caveats(&self) -> Vec<String> {
        let mut c = Vec::new();
        if self.event_count == 0 {
            c.push(
                "this recording contains no events — there is nothing to measure, and every \
                 number below is a zero, not a result"
                    .to_string(),
            );
        } else if self.analysis_duration_s < MIN_ANALYSIS_S {
            c.push(format!(
                "only {:.2}s of data was analyzed — every per-minute number here is \
                 extrapolated from less than a second of it",
                self.analysis_duration_s
            ));
        }
        c
    }

    /// Human-readable problems, ready to log at `warn` and print in the report.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if self.ring_drops > 0 {
            w.push(format!(
                "{} ring-buffer drops across {} batches — events are missing from this recording",
                self.ring_drops, self.batches_with_drops
            ));
        }
        if self.lost_batches > 0 {
            w.push(format!(
                "{} lost batches across {} seq_no gaps — whole batches never reached the \
                 recording, so the events in them are gone",
                self.lost_batches, self.seq_gaps
            ));
        }
        if self.monotonicity_violations > 0 {
            w.push(format!(
                "{} timestamp monotonicity violations — events arrived out of order",
                self.monotonicity_violations
            ));
        }
        if self.bad_lines > 0 {
            let where_ = match (self.bad_line_first, self.bad_line_last) {
                (Some(a), Some(b)) if a == b => format!(" at line {a}"),
                (Some(a), Some(b)) => format!(" between lines {a} and {b}"),
                _ => String::new(),
            };
            w.push(format!(
                "{} unparseable JSONL lines{where_} ({}){}",
                self.bad_lines,
                if self.bad_lines_tail_only {
                    "a truncated tail — the agent was killed mid-write"
                } else {
                    "corruption reaching into the body of the recording, not just its tail"
                },
                match &self.bad_line_error {
                    Some(e) => format!(": {e}"),
                    None => String::new(),
                }
            ));
        }
        if self.dominant_game.is_some() && self.dominant_game_share < MIN_DOMINANT_GAME_SHARE {
            w.push(format!(
                "the dominant process ({}) covers only {:.0}% of events — this recording spans \
                 more than one application, and the aim profile only fits one of them",
                self.dominant_game.as_deref().unwrap_or("?"),
                self.dominant_game_share * 100.0
            ));
        }
        if self.locked_fraction < MIN_LOCKED_FRACTION {
            w.push(format!(
                "the pointer was locked for only {:.0}% of events — desktop-mode movement is \
                 cursor motion, not aim, so degree-valued metrics are diluted (use --locked-only \
                 to exclude it)",
                self.locked_fraction * 100.0
            ));
        }
        if self.device_indices.len() > 1 {
            w.push(format!(
                "events came from {} different HID devices ({:?}) — the CPI and sensitivity \
                 profile only describe one of them",
                self.device_indices.len(),
                self.devices
            ));
        }
        // With no events there is nothing to convert, so a missing profile
        // is not a problem yet.
        if self.aim_profile_missing && self.event_count > 0 {
            w.push(
                "no per-game sensitivity profile matched; degree-valued metrics use the \
                 fallback sens 1.0 / 0.022 and are not comparable across sessions"
                    .to_string(),
            );
        }
        if self.grid_truncated {
            w.push(format!(
                "session exceeded the analysis grid cap: only the first {:.0}s of {:.0}s was \
                 analyzed. Every rate here counts the {} events inside that span and divides \
                 them by it, so the numbers describe the analyzed prefix rather than the \
                 whole recording",
                self.analysis_duration_s, self.duration_s, self.events_analyzed
            ));
        }
        if let Some(s) = &self.sidecar {
            if s.unfinished {
                w.push(format!(
                    "unfinished: the run did not stop cleanly (exit {:?}) — the recording ends \
                     wherever the agent stopped and the sidecar's counters are not final",
                    s.exit
                ));
            }
            if s.events > 0 && s.events != self.event_count as u64 {
                w.push(format!(
                    "the capture agent counted {} events but the recording holds {} ({:+}) — \
                     envelopes were lost between the agent and this file",
                    s.events,
                    self.event_count,
                    self.event_count as i64 - s.events as i64
                ));
            }
        }
        // Gaps are *not* a warning: raw input is silent while the hand is
        // still, so any real session is full of them. The count and the
        // interval histogram are reported for judgement instead.
        w
    }
}

fn histogram(intervals: &[f64]) -> Vec<IntervalBucket> {
    let total = intervals.len().max(1) as f64;
    let mut out: Vec<IntervalBucket> = Vec::with_capacity(INTERVAL_EDGES.len() + 1);
    let mut lo = 0.0;
    for &hi in &INTERVAL_EDGES {
        out.push(IntervalBucket {
            label: format!("≤{hi}ms"),
            lo_ms: lo,
            hi_ms: Some(hi),
            count: 0,
            fraction: 0.0,
        });
        lo = hi;
    }
    out.push(IntervalBucket {
        label: format!(">{lo}ms"),
        lo_ms: lo,
        hi_ms: None,
        count: 0,
        fraction: 0.0,
    });

    for &v in intervals {
        let idx = INTERVAL_EDGES
            .iter()
            .position(|&e| v <= e)
            .unwrap_or(INTERVAL_EDGES.len());
        out[idx].count += 1;
    }
    for b in &mut out {
        b.fraction = b.count as f64 / total;
    }
    out
}

pub fn compute(p: &Prepared) -> QualityReport {
    let session = &p.session;

    // Intervals come off the integer µs timeline: at 1 kHz the whole histogram
    // hinges on `<= 1.0 ms`, and f64 seconds put most of a clean stream at
    // 1.000000000000112 ms.
    let mut intervals = Vec::with_capacity(p.event_us.len().saturating_sub(1));
    let mut violations = 0usize;
    for w in p.event_us.windows(2) {
        let d = w[1] - w[0];
        if d < 0 {
            violations += 1;
        } else {
            intervals.push(d as f64 / 1000.0);
        }
    }

    let gaps = intervals.iter().filter(|&&d| d > GAP_MS).count();
    let within_1ms = intervals.iter().filter(|&&d| d <= 1.0).count();
    let histogram = histogram(&intervals);
    let polling = polling_stats(p.analysed_event_us());
    // One sort for all three order statistics, in place: the intervals are
    // owned here and already finite (they come off the integer µs timeline),
    // so `sorted_finite`'s filtered copy was a second 64 MB allocation on an
    // 8 M-event session for nothing.
    let n_intervals = intervals.len();
    intervals.sort_unstable_by(f64::total_cmp);
    let median_interval_ms = stats::percentile_sorted(&intervals, 0.5).unwrap_or(0.0);
    let p99_interval_ms = stats::percentile_sorted(&intervals, 0.99).unwrap_or(0.0);
    let max_interval_ms = intervals.last().copied().unwrap_or(0.0);
    drop(intervals);

    let batches_with_drops = session
        .batches
        .iter()
        .filter(|b| b.drops_since_last > 0)
        .count();

    // seq_no is monotonic per session; a jump means batches never landed.
    let mut lost_batches = 0u64;
    let mut seq_gaps = 0usize;
    for w in session.batches.windows(2) {
        let missing = w[1].seq_no.saturating_sub(w[0].seq_no).saturating_sub(1);
        if missing > 0 {
            lost_batches += missing;
            seq_gaps += 1;
        }
    }

    // Batch latency: how long after its first event the envelope was stamped.
    let anchor = &session.config.anchor;
    let latencies: Vec<f64> = session
        .batches
        .iter()
        .filter_map(|b| {
            b.first_event_qpc
                .map(|q| (b.ts_anchor_us - anchor.qpc_to_utc_us(q)) as f64 / 1000.0)
        })
        .collect();

    let span = p.analysis_duration_s;
    let bad = &session.bad_lines;
    let report = QualityReport {
        event_count: p.events().len(),
        events_analyzed: p.events_in_grid,
        batch_count: session.batches.len(),
        duration_s: p.duration_s,
        analysis_duration_s: span,
        // Numerator and denominator must cover the same span: on a truncated
        // grid the whole recording's events over the analysed seconds was a
        // rate no part of this session ever ran at.
        events_per_s: if span > 0.0 {
            p.events_in_grid as f64 / span
        } else {
            0.0
        },
        mean_events_per_batch: if session.batches.is_empty() {
            0.0
        } else {
            p.events().len() as f64 / session.batches.len() as f64
        },
        ring_drops: session.total_drops,
        batches_with_drops,
        lost_batches,
        seq_gaps,
        monotonicity_violations: violations,
        bad_lines: bad.count,
        bad_line_first: bad.first_line,
        bad_line_last: bad.last_line,
        bad_lines_tail_only: bad.tail_only,
        bad_line_error: bad.first_error.clone(),
        abs_frames: session.total_abs_frames,
        batch_latency_ms: Summary::of_vec(latencies),
        median_interval_ms,
        p99_interval_ms,
        max_interval_ms,
        pct_within_1ms: if n_intervals == 0 {
            0.0
        } else {
            100.0 * within_1ms as f64 / n_intervals as f64
        },
        polling,
        gaps_over_10ms: gaps,
        interval_histogram: histogram,
        dominant_game: p.game.clone(),
        dominant_game_share: p.dominant_game_share,
        locked_fraction: p.locked_fraction(),
        locked_only: p.params.locked_only,
        devices: session.config.devices.clone(),
        device_indices: session.device_indices(),
        anchor_uncertainty_us: session.config.anchor_uncertainty_us,
        aim_profile_missing: p.aim_fallback,
        grid_truncated: p.grid_truncated,
        sidecar: crate::load::read_sidecar(&session.path)
            .as_ref()
            .map(SidecarSummary::of),
        clean: false,
    };
    QualityReport {
        clean: report.warnings().is_empty(),
        ..report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::LoadedSession;
    use crate::series::{Params, prepare};
    use crate::testutil::{StreamBuilder, batch_meta, loaded_from, loaded_with_batches};

    fn analyze(session: LoadedSession) -> QualityReport {
        compute(&prepare(session, Params::default()))
    }

    #[test]
    fn a_clean_1khz_stream_looks_clean() {
        let mut b = StreamBuilder::new();
        b.move_ms(500, 3, 0);
        let q = analyze(loaded_from(b.into_events(), Some("cs2.exe")));

        assert_eq!(q.event_count, 500);
        assert_eq!(q.monotonicity_violations, 0);
        assert_eq!(q.gaps_over_10ms, 0);
        assert_eq!(q.ring_drops, 0);
        assert!((q.pct_within_1ms - 100.0).abs() < 1e-9);
        assert!((q.median_interval_ms - 1.0).abs() < 1e-6);
        assert!(q.clean, "{:?}", q.warnings());
        assert_eq!(q.locked_fraction, 1.0);
        assert_eq!(q.anchor_uncertainty_us, Some(8));
        // Bucket totals add up.
        let total: usize = q.interval_histogram.iter().map(|b| b.count).sum();
        assert_eq!(total, 499);
        assert_eq!(
            q.interval_histogram
                .iter()
                .find(|b| b.label == "≤1ms")
                .unwrap()
                .count,
            499
        );
    }

    /// The brief's fixture: one deliberate 50 ms gap and one out-of-order event.
    #[test]
    fn a_gap_and_an_out_of_order_timestamp_are_both_counted() {
        let mut b = StreamBuilder::new();
        b.move_ms(100, 3, 0) // t = 0..99ms
            .idle_ms(50) // 50ms of silence
            .move_ms(100, 3, 0); // resumes at t = 150ms
        // Now append an event stamped *before* the one preceding it.
        b.push_at_us(252_000, 1, 0, 0);
        b.push_at_us(251_000, 1, 0, 0); // 1ms backwards

        let q = analyze(loaded_from(b.into_events(), Some("cs2.exe")));
        assert_eq!(q.event_count, 202);
        assert_eq!(q.monotonicity_violations, 1);
        assert_eq!(q.gaps_over_10ms, 1);
        assert!(
            (q.max_interval_ms - 51.0).abs() < 1e-6,
            "max interval {}",
            q.max_interval_ms
        );
        // The gap lands in the >50ms bucket.
        assert_eq!(
            q.interval_histogram
                .iter()
                .find(|b| b.label == "≤100ms")
                .unwrap()
                .count,
            1
        );
        assert!(!q.clean);
        let w = q.warnings();
        assert_eq!(w.len(), 1, "gaps alone are not a warning: {w:?}");
        assert!(w[0].contains("monotonicity"), "{w:?}");
    }

    /// Idle stretches make gaps unavoidable, so they are counted but never
    /// warned about.
    #[test]
    fn idle_gaps_alone_keep_the_session_clean() {
        let mut b = StreamBuilder::new();
        b.move_ms(20, 3, 0).idle_ms(500).move_ms(20, 3, 0);
        let q = analyze(loaded_from(b.into_events(), Some("cs2.exe")));
        assert_eq!(q.gaps_over_10ms, 1);
        assert!(q.clean, "{:?}", q.warnings());
    }

    #[test]
    fn ring_drops_and_a_missing_aim_profile_are_surfaced() {
        let mut b = StreamBuilder::new();
        b.move_ms(50, 2, 0);
        let mut session = loaded_from(b.into_events(), Some("valorant.exe"));
        session.total_drops = 17;
        session.batches[0].drops_since_last = 17;
        session.bad_lines.count = 2;

        let q = analyze(session);
        assert_eq!(q.ring_drops, 17);
        assert_eq!(q.batches_with_drops, 1);
        assert_eq!(q.bad_lines, 2);
        assert!(q.aim_profile_missing);
        assert!(!q.clean);
        let w = q.warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w.iter().any(|s| s.contains("ring-buffer drops")));
        assert!(w.iter().any(|s| s.contains("sensitivity profile")));
        assert!(w.iter().any(|s| s.contains("unparseable")));
    }

    #[test]
    fn an_empty_recording_does_not_divide_by_zero() {
        let q = analyze(loaded_from(Vec::new(), Some("cs2.exe")));
        assert_eq!(q.event_count, 0);
        assert_eq!(q.events_per_s, 0.0);
        assert_eq!(q.pct_within_1ms, 0.0);
        assert_eq!(q.max_interval_ms, 0.0);
        assert!(q.clean, "{:?}", q.warnings());
    }

    /// A jump in `seq_no` means whole batches never made it to disk.
    #[test]
    fn seq_no_gaps_are_counted_as_lost_batches() {
        let mut b = StreamBuilder::new();
        b.move_ms(60, 4, 0);
        let evs = b.into_events();
        let batches = vec![
            batch_meta(0, Some("cs2.exe"), true, 20),
            batch_meta(1, Some("cs2.exe"), true, 20),
            // seq 2, 3 and 4 never arrived.
            batch_meta(5, Some("cs2.exe"), true, 20),
        ];
        let q = analyze(loaded_with_batches(evs.clone(), batches));
        assert_eq!(q.lost_batches, 3);
        assert_eq!(q.seq_gaps, 1);
        assert!(!q.clean);
        assert!(
            q.warnings().iter().any(|w| w.contains("lost batches")),
            "{:?}",
            q.warnings()
        );

        // An unbroken sequence loses nothing.
        let ok = vec![
            batch_meta(7, Some("cs2.exe"), true, 30),
            batch_meta(8, Some("cs2.exe"), true, 30),
        ];
        let q = analyze(loaded_with_batches(evs, ok));
        assert_eq!(q.lost_batches, 0);
        assert_eq!(q.seq_gaps, 0);
        assert!(q.clean, "{:?}", q.warnings());
    }

    #[test]
    fn unlocked_spans_lower_the_locked_fraction_and_warn() {
        let mut b = StreamBuilder::new();
        b.move_ms(100, 4, 0);
        let evs = b.into_events();
        // 80 locked events out of 100.
        let batches = vec![
            batch_meta(0, Some("cs2.exe"), true, 40),
            batch_meta(1, Some("cs2.exe"), false, 20),
            batch_meta(2, Some("cs2.exe"), true, 40),
        ];
        let q = analyze(loaded_with_batches(evs, batches));
        assert!(
            (q.locked_fraction - 0.8).abs() < 1e-9,
            "{}",
            q.locked_fraction
        );
        assert!(
            q.warnings()
                .iter()
                .any(|w| w.contains("pointer was locked")),
            "{:?}",
            q.warnings()
        );
    }

    #[test]
    fn a_split_session_warns_about_its_dominant_process() {
        let mut b = StreamBuilder::new();
        b.move_ms(100, 4, 0);
        let evs = b.into_events();
        let batches = vec![
            batch_meta(0, Some("cs2.exe"), true, 60),
            batch_meta(1, Some("chrome.exe"), true, 40),
        ];
        let q = analyze(loaded_with_batches(evs, batches));
        assert_eq!(q.dominant_game.as_deref(), Some("cs2.exe"));
        assert!((q.dominant_game_share - 0.6).abs() < 1e-9);
        assert!(
            q.warnings().iter().any(|w| w.contains("dominant process")),
            "{:?}",
            q.warnings()
        );
    }

    #[test]
    fn batch_latency_is_measured_against_the_first_events_qpc() {
        let mut b = StreamBuilder::new();
        b.move_ms(40, 4, 0);
        let evs = b.into_events();
        let cfg = crate::testutil::session_cfg();
        let mut batches = vec![
            batch_meta(0, Some("cs2.exe"), true, 20),
            batch_meta(1, Some("cs2.exe"), true, 20),
        ];
        // Batch 0's first event is at t=0, stamped 25ms later.
        batches[0].first_event_qpc = Some(evs[0].ts_qpc);
        batches[0].ts_anchor_us = cfg.anchor.qpc_to_utc_us(evs[0].ts_qpc) + 25_000;
        // Batch 1's first event is at t=20ms, stamped 5ms later.
        batches[1].first_event_qpc = Some(evs[20].ts_qpc);
        batches[1].ts_anchor_us = cfg.anchor.qpc_to_utc_us(evs[20].ts_qpc) + 5_000;

        let q = analyze(loaded_with_batches(evs, batches));
        assert_eq!(q.batch_latency_ms.n, 2);
        assert!((q.batch_latency_ms.max - 25.0).abs() < 1e-6);
        assert!((q.batch_latency_ms.median - 15.0).abs() < 1e-6);
    }
}
