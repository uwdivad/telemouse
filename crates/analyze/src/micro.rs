//! Sub-movements and micro-control: correction counts, tremor, and the
//! micro-adjustment size distribution.
//!
//! **Corrections** are velocity direction reversals inside a movement segment,
//! projected on that segment's net heading with a still-speed deadband. One
//! big pull plus one micro-correct is clean; four or five is noisy tracking.
//!
//! **Tremor** is the velocity signal minus a wide boxcar baseline. The 7-point
//! Savitzky–Golay smoother used elsewhere is far too narrow to serve as the
//! high-pass here — its passband reaches into the hundreds of Hz, so a 10 Hz
//! tremor would survive smoothing and vanish from the residual. A 200 ms
//! boxcar instead has spectral nulls at exactly 5/10/15 Hz, so the 8–12 Hz
//! band of interest lands in the residual essentially intact.
//!
//! Band power comes from Goertzel evaluations at 1 Hz spacing over overlapping
//! Hann-windowed blocks (a hand-rolled Welch estimate), which avoids pulling in
//! an FFT dependency for what is a handful of bins.
//!
//! # Decimation
//!
//! The band of interest tops out at 12 Hz and the reported spectrum at 25 Hz,
//! so the spectral estimate runs at 50 Hz rather than the grid's 1 kHz: the
//! residual is block-averaged 20:1 first, which is both the decimation and its
//! anti-alias filter (a 20-tap boxcar has nulls at every multiple of 50 Hz, and
//! is down only 0.5 dB at 10 Hz). That is ~40× less Goertzel work for a
//! spectrum that Nyquist says is unchanged in the band we report. The RMS is
//! *not* decimated — it is a broadband measure and stays at full rate.
//!
//! Scope, so the numbers are read correctly: both the RMS and the band power
//! cover **every moving sample**, ballistic flick phases included — samples
//! where the hand is completely idle are excluded (they would just dilute the
//! estimate toward zero), but fast movements are not. A session with more
//! flicking therefore has more broadband residual, so these are comparable
//! across sessions of similar composition, not across a flick drill and a
//! tracking drill.

use serde::{Deserialize, Serialize};

use crate::flicks::reversals;
use crate::series::Prepared;
use crate::stats::{self, Summary};

/// Sample rate the spectral estimate runs at, Hz. Nyquist 25 Hz covers the
/// whole reported band.
const DEC_TARGET_HZ: f64 = 50.0;
/// Welch block length and its floor, in seconds.
const BLOCK_S: f64 = 2.0;
const MIN_BLOCK_S: f64 = 0.5;
/// Bins evaluated, Hz.
const BAND_LO_HZ: usize = 1;
const BAND_HI_HZ: usize = 25;
/// The tremor band from the plan.
const TREMOR_LO_HZ: f64 = 8.0;
const TREMOR_HI_HZ: f64 = 12.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandBin {
    pub hz: f64,
    /// Mean power density in (counts/s)² for this bin.
    pub power: f64,
}

/// One log-ish bucket of the micro-adjustment size histogram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmplitudeBucket {
    pub label: String,
    pub lo_counts: f64,
    pub hi_counts: Option<f64>,
    pub lo_deg: f64,
    pub hi_deg: Option<f64>,
    pub count: usize,
    pub fraction: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicroReport {
    /// Corrections per movement segment.
    pub corrections_per_segment: Summary,
    pub total_corrections: usize,
    /// Segments with at most one correction — "one pull plus a micro-correct".
    pub clean_segment_fraction: f64,
    pub segment_count: usize,

    /// RMS of the high-passed velocity over every moving sample, counts/s.
    pub tremor_rms_counts_s: f64,
    pub tremor_rms_cm_s: f64,
    /// Fraction of the session's moving samples that fed the tremor estimate.
    pub tremor_sample_fraction: f64,
    /// Per-Hz band power of the high-passed velocity.
    pub band_power: Vec<BandBin>,
    pub band_power_8_12: f64,
    pub band_power_total: f64,
    /// 8–12 Hz share of the 1–25 Hz total. High = tremor-dominated.
    pub band_ratio_8_12: f64,
    /// Bin carrying the most power, Hz (0 when there is no signal).
    pub dominant_hz: f64,
    /// Blocks that contained movement and so contributed to the estimate.
    pub analyzed_blocks: usize,
    /// Rate the spectral estimate ran at after decimation, Hz.
    pub spectrum_fs_hz: f64,

    pub micro_adjustments: Vec<AmplitudeBucket>,
    /// Net amplitude of each movement segment, degrees.
    pub segment_amplitude_deg: Summary,
}

/// The tremor residual's energy, bucketed by second, so any later slicing of
/// the session (per minute, between markers) can report an RMS without a
/// second pass over the grid.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TremorSeries {
    pub sumsq: Vec<f64>,
    pub active: Vec<u32>,
}

impl TremorSeries {
    /// RMS of the high-passed velocity over seconds `[a, b)`, counts/s.
    pub fn rms_seconds(&self, a: usize, b: usize) -> f64 {
        let b = b.min(self.sumsq.len());
        if b <= a {
            return 0.0;
        }
        let sum: f64 = self.sumsq[a..b].iter().sum();
        let n: u32 = self.active[a..b].iter().sum();
        if n == 0 { 0.0 } else { (sum / n as f64).sqrt() }
    }
}

/// Centered boxcar of width `2 * half + 1`, computed with prefix sums so a long
/// session stays linear-time. Edges shrink the window rather than pad it.
pub fn boxcar(x: &[f64], half: usize) -> Vec<f64> {
    let n = x.len();
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0);
    let mut acc = 0.0;
    for &v in x {
        acc += v;
        prefix.push(acc);
    }
    (0..n)
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(n);
            (prefix[b] - prefix[a]) / (b - a) as f64
        })
        .collect()
}

/// `x` minus its centered boxcar baseline, for one grid run.
///
/// The window is taken in *global* coordinates, so the divisor shrinks only at
/// the real ends of the session — never at a run boundary. That is what makes
/// the residual identical to the one a dense grid would produce: the run pad is
/// at least `half` cells wide, so every cell the window reaches outside the run
/// is genuinely zero and contributes nothing to the sum.
fn residual_into(
    x: &[f64],
    start: usize,
    n_global: usize,
    half: usize,
    out: &mut Vec<f64>,
    prefix: &mut Vec<f64>,
) {
    prefix.clear();
    prefix.push(0.0);
    let mut acc = 0.0;
    for &v in x {
        acc += v;
        prefix.push(acc);
    }
    out.clear();
    out.reserve(x.len());
    for (i, &v) in x.iter().enumerate() {
        let g = start + i;
        let a_g = g.saturating_sub(half);
        let b_g = (g + half + 1).min(n_global);
        let a_l = a_g.saturating_sub(start).min(x.len());
        let b_l = (b_g - start).min(x.len());
        let sum = prefix[b_l] - prefix[a_l];
        out.push(v - sum / (b_g - a_g) as f64);
    }
}

/// `|X(f)|²` by Goertzel — one bin of the DFT without building the rest.
pub fn goertzel_mag2(x: &[f64], hz: f64, fs: f64) -> f64 {
    let w = std::f64::consts::TAU * hz / fs;
    let (sw, cw) = w.sin_cos();
    let c = 2.0 * cw;
    let mut s1 = 0.0;
    let mut s2 = 0.0;
    for &v in x {
        let s0 = v + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let re = s1 - s2 * cw;
    let im = s2 * sw;
    re * re + im * im
}

fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos())
        .collect()
}

const BUCKET_EDGES: [f64; 10] = [
    2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0,
];

fn histogram(p: &Prepared, amps_counts: &[f64]) -> Vec<AmplitudeBucket> {
    let (kx, _) = p.aim_scale();
    let total = amps_counts.len().max(1) as f64;
    let mut buckets: Vec<AmplitudeBucket> = Vec::with_capacity(BUCKET_EDGES.len() + 1);
    let mut lo = 0.0;
    for &hi in &BUCKET_EDGES {
        buckets.push(AmplitudeBucket {
            label: format!("{lo:.0}–{hi:.0}"),
            lo_counts: lo,
            hi_counts: Some(hi),
            lo_deg: lo * kx,
            hi_deg: Some(hi * kx),
            count: 0,
            fraction: 0.0,
        });
        lo = hi;
    }
    buckets.push(AmplitudeBucket {
        label: format!("{lo:.0}+"),
        lo_counts: lo,
        hi_counts: None,
        lo_deg: lo * kx,
        hi_deg: None,
        count: 0,
        fraction: 0.0,
    });

    for &a in amps_counts {
        let idx = BUCKET_EDGES
            .iter()
            .position(|&e| a < e)
            .unwrap_or(BUCKET_EDGES.len());
        buckets[idx].count += 1;
    }
    for b in &mut buckets {
        b.fraction = b.count as f64 / total;
    }
    buckets
}

pub fn compute(p: &Prepared) -> MicroReport {
    compute_full(p).0
}

/// The micro report plus the per-second tremor energy the longitudinal tables
/// slice up.
pub fn compute_full(p: &Prepared) -> (MicroReport, TremorSeries) {
    let g = &p.grid;
    let segs = p.segments();

    let mut corr_counts = Vec::with_capacity(segs.len());
    let mut amps_counts = Vec::with_capacity(segs.len());
    let mut amps_deg = Vec::with_capacity(segs.len());
    for s in segs {
        let (dx, dy) = g.displacement(s.start, s.end);
        let mag = dx.hypot(dy);
        amps_counts.push(mag);
        amps_deg.push(p.deg_mag(dx, dy));
        let (ux, uy) = if mag > 0.0 {
            (dx / mag, dy / mag)
        } else {
            (1.0, 0.0)
        };
        corr_counts.push(reversals(p, s.start, s.end, ux, uy) as f64);
    }
    let total_corrections: usize = corr_counts.iter().sum::<f64>() as usize;
    let clean = corr_counts.iter().filter(|&&c| c <= 1.0).count();

    // --- tremor -----------------------------------------------------------
    let half = p.params.tremor_baseline_ms / 2;
    let fs = 1.0 / g.dt;
    let dec = ((fs / DEC_TARGET_HZ).round() as usize).max(1);
    let fs_dec = fs / dec as f64;
    let block = ((BLOCK_S * fs_dec).round() as usize).max(8);
    let hop = (block / 2).max(1);
    let min_block = ((MIN_BLOCK_S * fs_dec).round() as usize).max(4);
    let n_dec = g.len().div_ceil(dec);
    let wlen = block.min(n_dec.max(1));
    let usable = n_dec >= wlen && wlen >= min_block;

    let win = hann(wlen);
    let wsum: f64 = win.iter().sum();
    let norm = 2.0 / (wsum * wsum);

    let per_sec = ((1.0 / g.dt).round() as usize).max(1);
    let n_sec = g.len().div_ceil(per_sec);
    let mut tremor = TremorSeries {
        sumsq: vec![0.0; n_sec],
        active: vec![0u32; n_sec],
    };

    // Scratch, allocated once and reused for every run and every Welch block.
    let mut prefix: Vec<f64> = Vec::new();
    let mut rx: Vec<f64> = Vec::new();
    let mut ry: Vec<f64> = Vec::new();
    let mut dx: Vec<f64> = Vec::new();
    let mut dy: Vec<f64> = Vec::new();
    let mut dact: Vec<bool> = Vec::new();
    let mut bx = vec![0.0f64; wlen];
    let mut by = vec![0.0f64; wlen];

    let mut acc_bins = vec![0.0; BAND_HI_HZ - BAND_LO_HZ + 1];
    let mut blocks = 0usize;
    let mut energy = 0.0;
    let mut n_active = 0usize;

    for r in &g.runs {
        residual_into(&r.vx, r.start, g.len(), half, &mut rx, &mut prefix);
        residual_into(&r.vy, r.start, g.len(), half, &mut ry, &mut prefix);

        for j in 0..r.len() {
            if r.speed_raw[j] > 0.0 {
                let e = rx[j] * rx[j] + ry[j] * ry[j];
                energy += e;
                n_active += 1;
                let s = (r.start + j) / per_sec;
                tremor.sumsq[s] += e;
                tremor.active[s] += 1;
            }
        }

        if !usable {
            continue;
        }

        // Block-average down to `fs_dec`. Cells the last sample reaches past
        // the run are exactly zero, so dividing by `dec` throughout is right.
        let k0 = r.start / dec;
        let k1 = r.end().div_ceil(dec);
        dx.clear();
        dy.clear();
        dact.clear();
        for k in k0..k1 {
            let a = (k * dec).max(r.start) - r.start;
            let b = ((k + 1) * dec).min(r.end()) - r.start;
            let mut sx = 0.0;
            let mut sy = 0.0;
            let mut act = false;
            for j in a..b {
                sx += rx[j];
                sy += ry[j];
                act |= r.speed_raw[j] > 0.0;
            }
            dx.push(sx / dec as f64);
            dy.push(sy / dec as f64);
            dact.push(act);
        }

        // Blocks sit on global hop boundaries so the estimate does not depend
        // on where a run happens to start.
        let mut k = k0.div_ceil(hop) * hop;
        while k + wlen <= k1 {
            let lo = k - k0;
            if dact[lo..lo + wlen].iter().any(|&a| a) {
                bx.copy_from_slice(&dx[lo..lo + wlen]);
                by.copy_from_slice(&dy[lo..lo + wlen]);
                let mx = stats::mean(&bx).unwrap_or(0.0);
                let my = stats::mean(&by).unwrap_or(0.0);
                for i in 0..wlen {
                    bx[i] = (bx[i] - mx) * win[i];
                    by[i] = (by[i] - my) * win[i];
                }
                for (b, a) in acc_bins.iter_mut().enumerate() {
                    let hz = (BAND_LO_HZ + b) as f64;
                    *a += (goertzel_mag2(&bx, hz, fs_dec) + goertzel_mag2(&by, hz, fs_dec)) * norm;
                }
                blocks += 1;
            }
            k += hop;
        }
    }

    let scale = if blocks > 0 { 1.0 / blocks as f64 } else { 0.0 };
    let band_power: Vec<BandBin> = acc_bins
        .into_iter()
        .enumerate()
        .map(|(b, a)| BandBin {
            hz: (BAND_LO_HZ + b) as f64,
            power: a * scale,
        })
        .collect();

    let tremor_rms = if n_active > 0 {
        (energy / n_active as f64).sqrt()
    } else {
        0.0
    };

    let band_power_total: f64 = band_power.iter().map(|b| b.power).sum();
    let band_power_8_12: f64 = band_power
        .iter()
        .filter(|b| b.hz >= TREMOR_LO_HZ && b.hz <= TREMOR_HI_HZ)
        .map(|b| b.power)
        .sum();
    let dominant_hz = band_power
        .iter()
        .fold(None::<&BandBin>, |best, b| match best {
            Some(x) if x.power >= b.power => Some(x),
            _ => Some(b),
        })
        .filter(|b| b.power > 0.0)
        .map_or(0.0, |b| b.hz);

    let report = MicroReport {
        corrections_per_segment: Summary::of(&corr_counts),
        total_corrections,
        clean_segment_fraction: if segs.is_empty() {
            0.0
        } else {
            clean as f64 / segs.len() as f64
        },
        segment_count: segs.len(),
        tremor_rms_counts_s: tremor_rms,
        tremor_rms_cm_s: p.counts_to_cm(tremor_rms),
        tremor_sample_fraction: if g.is_empty() {
            0.0
        } else {
            n_active as f64 / g.len() as f64
        },
        band_power,
        band_power_8_12,
        band_power_total,
        band_ratio_8_12: if band_power_total > 0.0 {
            band_power_8_12 / band_power_total
        } else {
            0.0
        },
        dominant_hz,
        analyzed_blocks: blocks,
        spectrum_fs_hz: fs_dec,
        micro_adjustments: histogram(p, &amps_counts),
        segment_amplitude_deg: Summary::of(&amps_deg),
    };
    (report, tremor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{StreamBuilder, prep};

    #[test]
    fn goertzel_recovers_a_known_sinusoid_amplitude() {
        let fs = 1000.0;
        let n = 1000;
        let amp = 2.0;
        let x: Vec<f64> = (0..n)
            .map(|i| amp * (std::f64::consts::TAU * 10.0 * i as f64 / fs).sin())
            .collect();
        // Rectangular window: power = 2|X|^2 / N^2 = A^2/2.
        let power = 2.0 * goertzel_mag2(&x, 10.0, fs) / (n as f64 * n as f64);
        assert!((power - amp * amp / 2.0).abs() < 1e-6, "{power}");
        // A bin far from the tone carries almost nothing.
        let off = 2.0 * goertzel_mag2(&x, 25.0, fs) / (n as f64 * n as f64);
        assert!(off < 1e-3, "{off}");
    }

    #[test]
    fn boxcar_removes_a_constant_and_keeps_length() {
        let x = vec![5.0; 100];
        let b = boxcar(&x, 10);
        assert_eq!(b.len(), 100);
        for v in b {
            assert!((v - 5.0).abs() < 1e-12);
        }
    }

    /// The run-local residual has to reproduce what a dense boxcar over the
    /// whole session would give — that equality is the sparse grid's contract.
    #[test]
    fn run_residual_matches_a_dense_boxcar() {
        let mut b = StreamBuilder::new();
        b.move_ms(30, 7, 0)
            .idle_ms(900)
            .move_ms(40, -3, 2)
            .idle_ms(600);
        let p = prep(b.into_events());
        let half = p.params.tremor_baseline_ms / 2;
        let dense_vx = p.grid.dense(|r, j| r.vx[j]);
        let dense_base = boxcar(&dense_vx, half);

        let mut out = Vec::new();
        let mut prefix = Vec::new();
        for r in &p.grid.runs {
            residual_into(&r.vx, r.start, p.grid.len(), half, &mut out, &mut prefix);
            for j in 0..r.len() {
                let want = dense_vx[r.start + j] - dense_base[r.start + j];
                assert!(
                    (out[j] - want).abs() < 1e-9,
                    "cell {} got {} want {want}",
                    r.start + j,
                    out[j]
                );
            }
        }
        // And outside the runs the dense residual is exactly zero, which is
        // why the runs are allowed to omit those cells at all.
        let covered: Vec<bool> = {
            let mut c = vec![false; p.grid.len()];
            for r in &p.grid.runs {
                c[r.start..r.end()].fill(true);
            }
            c
        };
        for i in 0..p.grid.len() {
            if !covered[i] {
                assert_eq!(dense_vx[i] - dense_base[i], 0.0, "cell {i}");
            }
        }
    }

    /// The plan's fatigue signal: a 10 Hz tremor on slow drift must light up
    /// the 8–12 Hz band relative to an otherwise identical steady drag.
    #[test]
    fn a_10hz_tremor_dominates_the_8_to_12hz_band() {
        let mut t = StreamBuilder::new();
        t.tremor_ms(6000, 400.0, 3000.0, 10.0);
        let tremor = compute(&prep(t.into_events()));

        let mut c = StreamBuilder::new();
        c.move_at_ms(6000, 400.0, 0.0);
        let control = compute(&prep(c.into_events()));

        // The spectrum runs at 50 Hz now, so a 6 s session yields 2 s blocks
        // hopping 1 s: five of them, where the 1 kHz estimate got five 2.048 s
        // blocks. Same count, same coverage — the threshold is unchanged.
        assert_eq!(tremor.spectrum_fs_hz, 50.0);
        assert!(tremor.analyzed_blocks > 3, "{}", tremor.analyzed_blocks);
        assert!(
            tremor.band_ratio_8_12 > 0.8,
            "tremor ratio {} bins {:?}",
            tremor.band_ratio_8_12,
            tremor.band_power
        );
        assert_eq!(tremor.dominant_hz, 10.0);
        assert!(
            control.band_ratio_8_12 < 0.4,
            "control ratio {}",
            control.band_ratio_8_12
        );
        assert!(
            tremor.band_power_8_12 > control.band_power_8_12 * 50.0,
            "tremor {} control {}",
            tremor.band_power_8_12,
            control.band_power_8_12
        );
        // The high-passed RMS rises with the tremor too. (Full rate: the RMS
        // is broadband and is not decimated.)
        assert!(
            tremor.tremor_rms_counts_s > control.tremor_rms_counts_s * 2.0,
            "tremor rms {} control rms {}",
            tremor.tremor_rms_counts_s,
            control.tremor_rms_counts_s
        );
    }

    #[test]
    fn a_clean_pull_has_no_corrections_and_a_stuttery_one_does() {
        let mut clean = StreamBuilder::new();
        clean.move_ms(80, 20, 0).idle_ms(100);
        let c = compute(&prep(clean.into_events()));
        assert_eq!(c.segment_count, 1);
        assert_eq!(c.total_corrections, 0);
        assert_eq!(c.clean_segment_fraction, 1.0);

        // Pull, back off, pull, back off — four reversals inside one segment.
        let mut stutter = StreamBuilder::new();
        for _ in 0..4 {
            stutter.move_ms(15, 30, 0).move_ms(10, -30, 0);
        }
        stutter.idle_ms(100);
        let s = compute(&prep(stutter.into_events()));
        assert_eq!(s.segment_count, 1);
        assert!(
            s.total_corrections >= 5,
            "expected several reversals, got {}",
            s.total_corrections
        );
        assert_eq!(s.clean_segment_fraction, 0.0);
    }

    #[test]
    fn micro_adjustment_histogram_buckets_by_size() {
        let mut b = StreamBuilder::new();
        // Three tiny nudges (~6 counts), then one big sweep (2000 counts).
        for _ in 0..3 {
            b.move_ms(3, 2, 0).idle_ms(120);
        }
        b.move_ms(39, 50, 0).idle_ms(200); // 1950 counts
        let m = compute(&prep(b.into_events()));
        assert_eq!(m.segment_count, 4);

        let bucket = |label: &str| {
            m.micro_adjustments
                .iter()
                .find(|x| x.label == label)
                .unwrap()
                .count
        };
        assert_eq!(bucket("5–10"), 3, "{:?}", m.micro_adjustments);
        assert_eq!(bucket("1000–2000"), 1);
        let total: usize = m.micro_adjustments.iter().map(|x| x.count).sum();
        assert_eq!(total, 4);
        assert!((m.micro_adjustments.iter().map(|x| x.fraction).sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn per_second_tremor_energy_reconstructs_the_session_rms() {
        let mut b = StreamBuilder::new();
        b.tremor_ms(4000, 400.0, 2000.0, 10.0);
        let p = prep(b.into_events());
        let (m, t) = compute_full(&p);
        let whole = t.rms_seconds(0, t.sumsq.len());
        assert!(
            (whole - m.tremor_rms_counts_s).abs() < 1e-9,
            "{whole} vs {}",
            m.tremor_rms_counts_s
        );
        // And a slice of it is a real number, not the whole-session value.
        assert!(t.rms_seconds(0, 1) > 0.0);
        assert_eq!(t.rms_seconds(9, 20), 0.0);
    }

    #[test]
    fn empty_session_produces_a_zeroed_report() {
        let m = compute(&prep(Vec::new()));
        assert_eq!(m.segment_count, 0);
        assert_eq!(m.analyzed_blocks, 0);
        assert_eq!(m.band_ratio_8_12, 0.0);
        assert_eq!(m.dominant_hz, 0.0);
        assert_eq!(m.tremor_rms_counts_s, 0.0);
        assert_eq!(m.tremor_sample_fraction, 0.0);
        assert_eq!(m.micro_adjustments.len(), BUCKET_EDGES.len() + 1);
    }
}
