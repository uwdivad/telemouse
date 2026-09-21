use serde::{Deserialize, Serialize};

/// UTC microseconds since the Unix epoch, right now. Every binary stamps
/// wall-clock time through this one function so the capture anchor, the
/// bridge's latency estimate and the analyzer's report provenance agree on
/// the unit. A clock set before 1970 degrades to a negative value rather
/// than panicking.
pub fn now_utc_us() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i64,
        Err(e) => -(e.duration().as_micros() as i64),
    }
}

/// One QPC↔UTC anchor taken at session start.
///
/// `QueryPerformanceCounter` is monotonic but has an arbitrary zero; a single
/// anchor per session maps any captured QPC value onto the UTC timeline for
/// storage and replay. All math is pure integer arithmetic so it is exactly
/// reproducible in any consumer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QpcAnchor {
    /// QPC reading at the anchor instant.
    pub qpc: u64,
    /// UTC microseconds since the Unix epoch at the same instant.
    pub utc_us: i64,
    /// QPC ticks per second (`QueryPerformanceFrequency`).
    pub qpc_freq: u64,
}

/// Scale a tick *magnitude* to whole microseconds in 64-bit arithmetic:
/// `⌊m × 1_000_000 ÷ freq⌋`, or `None` when 64 bits cannot carry it.
///
/// `None` means "ask i128": a zero frequency (the caller's i128 form panics on
/// it, as it always has) or an intermediate product that would wrap. Working
/// on the magnitude keeps every value unsigned, and since i128 division
/// truncates toward zero, `|trunc(x)| == trunc(|x|)` — the caller puts the
/// sign back afterwards.
#[inline]
fn us_from_ticks_mag(m: u64, freq: u64) -> Option<u64> {
    // The ubiquitous Windows frequency, checked first: `m × 1e6 ÷ 1e7` is
    // exactly `m ÷ 10`, and a constant divisor is a multiply-and-shift, not a
    // division at all. This is the path every real session takes.
    if freq == 10_000_000 {
        return Some(m / 10);
    }
    if freq == 0 {
        return None;
    }
    // Whole-µs frequencies (1 MHz, 24 MHz, …): one exact division.
    if freq.is_multiple_of(1_000_000) {
        return Some(m / (freq / 1_000_000));
    }
    // Frequencies that divide 1 MHz: no division at all. (`freq` is non-zero
    // here, so this is the plain `1_000_000 % freq == 0`.)
    if 1_000_000u64.is_multiple_of(freq) {
        return m.checked_mul(1_000_000 / freq);
    }
    // Odd frequencies (3_579_545, 2_435_885, …): split the ticks into whole
    // seconds and a remainder, so neither product has to carry `m × 1e6`.
    // m = q·freq + r ⟹ m·1e6/freq = q·1e6 + r·1e6/freq, and q·1e6 is an
    // integer, so the floors compose exactly. Both multiplications are
    // checked: `q × 1e6` can only wrap below 1 MHz, `r × 1e6` only above
    // ~18 THz, and either way the i128 fallback takes over.
    let q = m / freq;
    let r = m % freq;
    let whole = q.checked_mul(1_000_000)?;
    let frac = r.checked_mul(1_000_000)? / freq;
    whole.checked_add(frac)
}

/// `us_from_ticks_mag` with the sign put back, `None` if the result does not
/// fit an `i64` (only reachable below ~2 MHz over an absurd tick span).
#[inline]
fn us_from_ticks(m: u64, neg: bool, freq: u64) -> Option<i64> {
    let mag = us_from_ticks_mag(m, freq)?;
    if mag > i64::MAX as u64 {
        return None;
    }
    Some(if neg { -(mag as i64) } else { mag as i64 })
}

/// The exact magnitude of `to - from` as a `u64`, with its sign — the full
/// range of a QPC difference, without ever widening to 128 bits.
#[inline]
fn tick_delta(from: u64, to: u64) -> (u64, bool) {
    if to >= from {
        (to - from, false)
    } else {
        (from - to, true)
    }
}

impl QpcAnchor {
    /// Map a QPC reading to UTC microseconds since the Unix epoch.
    ///
    /// Works for readings before the anchor too (negative delta). The scaling
    /// is done in 64-bit integers, with the i128 form kept as a fallback for
    /// the frequencies and tick spans where 64 bits could overflow, so the
    /// result is identical for every input but costs no `__divti3` call on the
    /// path the analyzer walks once per event.
    #[inline]
    pub fn qpc_to_utc_us(&self, qpc: u64) -> i64 {
        let (m, neg) = tick_delta(self.qpc, qpc);
        if let Some(dus) = us_from_ticks(m, neg, self.qpc_freq) {
            // `wrapping_add` is what `(utc_us as i128 + dus) as i64` did: the
            // cast keeps the low 64 bits, which is addition modulo 2^64.
            return self.utc_us.wrapping_add(dus);
        }
        let dticks = qpc as i128 - self.qpc as i128;
        (self.utc_us as i128 + dticks * 1_000_000 / self.qpc_freq as i128) as i64
    }

    /// Microseconds elapsed from `from_qpc` to `to_qpc` (may be negative).
    #[inline]
    pub fn ticks_to_us(&self, from_qpc: u64, to_qpc: u64) -> i64 {
        let (m, neg) = tick_delta(from_qpc, to_qpc);
        if let Some(us) = us_from_ticks(m, neg, self.qpc_freq) {
            return us;
        }
        let dticks = to_qpc as i128 - from_qpc as i128;
        (dticks * 1_000_000 / self.qpc_freq as i128) as i64
    }

    /// Convert a duration in milliseconds to QPC ticks (rounds down).
    pub fn ms_to_ticks(&self, ms: u64) -> u64 {
        (ms as u128 * self.qpc_freq as u128 / 1_000) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Windows QPC frequency is 10 MHz on virtually all modern systems.
    const FREQ: u64 = 10_000_000;

    /// The i128 implementation the 64-bit path replaced, kept verbatim as the
    /// oracle: every test below asserts the new code is bit-identical to it.
    fn ref_qpc_to_utc_us(a: &QpcAnchor, qpc: u64) -> i64 {
        let dticks = qpc as i128 - a.qpc as i128;
        let dus = if a.qpc_freq == 10_000_000 {
            dticks / 10
        } else {
            dticks * 1_000_000 / a.qpc_freq as i128
        };
        (a.utc_us as i128 + dus) as i64
    }

    fn ref_ticks_to_us(a: &QpcAnchor, from_qpc: u64, to_qpc: u64) -> i64 {
        let dticks = to_qpc as i128 - from_qpc as i128;
        (dticks * 1_000_000 / a.qpc_freq as i128) as i64
    }

    /// Frequencies worth sweeping: the ubiquitous 10 MHz, odd real-world
    /// crystals, whole-MHz and sub-MHz divisors, and values that force the
    /// i128 fallback by overflowing one of the 64-bit products.
    const SWEEP_FREQS: [u64; 11] = [
        10_000_000,
        3_579_545,
        2_435_885,
        1_193_182,
        14_318_180,
        1_000_000,
        24_000_000,
        1_000_000_000_000_000_000,
        2,
        7,
        u64::MAX,
    ];

    /// Anchors worth sweeping: the zero and maximum QPC, a realistic uptime,
    /// and UTC stamps at both ends of `i64` so the final wrap is exercised.
    const SWEEP_ANCHORS: [(u64, i64); 6] = [
        (5_000_000_000, 1_756_000_000_000_000),
        (0, 0),
        (u64::MAX, 1_756_000_000_000_000),
        (u64::MAX / 2, -1_756_000_000_000_000),
        (1, i64::MAX - 1_000),
        (u64::MAX / 3, i64::MIN + 1_000),
    ];

    /// Deterministic xorshift64*, so the sweep is the same on every machine
    /// and needs no dependency.
    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn parity_with_the_i128_reference_over_a_pseudo_random_sweep() {
        // 11 frequencies × 6 anchors × 12_000 probes × 2 functions ≈ 1.6 M
        // compared results.
        const PROBES: usize = 12_000;
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut cases = 0u64;
        for freq in SWEEP_FREQS {
            for (qpc, utc_us) in SWEEP_ANCHORS {
                let a = QpcAnchor {
                    qpc,
                    utc_us,
                    qpc_freq: freq,
                };
                for i in 0..PROBES {
                    let x = rng.next();
                    // Half the probes land within an hour of the anchor (what
                    // a session looks like), half anywhere in the u64 range.
                    let probe = if i % 2 == 0 {
                        let span = freq.saturating_mul(3_600).max(1);
                        let off = (x >> 1) % span;
                        if x & 1 == 0 {
                            qpc.wrapping_add(off)
                        } else {
                            qpc.wrapping_sub(off)
                        }
                    } else {
                        x
                    };
                    assert_eq!(
                        a.qpc_to_utc_us(probe),
                        ref_qpc_to_utc_us(&a, probe),
                        "qpc_to_utc_us diverged at freq={freq} anchor={qpc}/{utc_us} qpc={probe}"
                    );
                    let other = rng.next();
                    assert_eq!(
                        a.ticks_to_us(probe, other),
                        ref_ticks_to_us(&a, probe, other),
                        "ticks_to_us diverged at freq={freq} from={probe} to={other}"
                    );
                    cases += 2;
                }
            }
        }
        assert!(cases >= 1_000_000, "sweep shrank to {cases} cases");
    }

    #[test]
    fn parity_with_the_i128_reference_at_edge_values() {
        // Values where a 64-bit split could go wrong: the limits of u64 and
        // i64, the anchor itself, and ticks just either side of a whole µs.
        let probes: [u64; 16] = [
            0,
            1,
            2,
            9,
            10,
            11,
            999_999,
            1_000_000,
            1_000_001,
            i64::MAX as u64 - 1,
            i64::MAX as u64,
            1u64 << 63,
            (1u64 << 63) + 1,
            u64::MAX - 10,
            u64::MAX - 1,
            u64::MAX,
        ];
        for freq in SWEEP_FREQS {
            for (qpc, utc_us) in SWEEP_ANCHORS {
                let a = QpcAnchor {
                    qpc,
                    utc_us,
                    qpc_freq: freq,
                };
                let near = [
                    qpc,
                    qpc.wrapping_add(1),
                    qpc.wrapping_sub(1),
                    qpc.wrapping_add(freq),
                    qpc.wrapping_sub(freq),
                ];
                for probe in probes.into_iter().chain(near) {
                    assert_eq!(
                        a.qpc_to_utc_us(probe),
                        ref_qpc_to_utc_us(&a, probe),
                        "qpc_to_utc_us diverged at freq={freq} anchor={qpc}/{utc_us} qpc={probe}"
                    );
                    for other in probes {
                        assert_eq!(
                            a.ticks_to_us(probe, other),
                            ref_ticks_to_us(&a, probe, other),
                            "ticks_to_us diverged at freq={freq} from={probe} to={other}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn scaling_truncates_toward_zero_on_both_sides() {
        // 3_579_545 Hz: 1 tick ≈ 0.2794 µs, so 4 ticks is 1.117 µs.
        let a = QpcAnchor {
            qpc: 1_000_000_000,
            utc_us: 0,
            qpc_freq: 3_579_545,
        };
        assert_eq!(a.qpc_to_utc_us(a.qpc + 4), 1);
        assert_eq!(a.qpc_to_utc_us(a.qpc - 4), -1);
        assert_eq!(a.qpc_to_utc_us(a.qpc + 3), 0);
        assert_eq!(a.qpc_to_utc_us(a.qpc - 3), 0);
        assert_eq!(a.ticks_to_us(a.qpc, a.qpc + 3_579_545), 1_000_000);
        assert_eq!(a.ticks_to_us(a.qpc + 3_579_545, a.qpc), -1_000_000);
    }

    #[test]
    #[should_panic(expected = "divide by zero")]
    fn zero_frequency_still_panics_like_the_i128_form() {
        // Not reachable in practice (`session_setup` maps 0 → 1), but the
        // 64-bit path must not quietly invent an answer where the old one
        // divided by zero.
        let a = QpcAnchor {
            qpc: 10,
            utc_us: 0,
            qpc_freq: 0,
        };
        let _ = a.qpc_to_utc_us(20);
    }

    fn anchor() -> QpcAnchor {
        QpcAnchor {
            qpc: 5_000_000_000,            // 500s of uptime
            utc_us: 1_756_000_000_000_000, // some 2025-era UTC µs
            qpc_freq: FREQ,
        }
    }

    #[test]
    fn identity_at_anchor() {
        let a = anchor();
        assert_eq!(a.qpc_to_utc_us(a.qpc), a.utc_us);
    }

    #[test]
    fn one_second_forward() {
        let a = anchor();
        assert_eq!(a.qpc_to_utc_us(a.qpc + FREQ), a.utc_us + 1_000_000);
    }

    #[test]
    fn before_anchor_is_negative_offset() {
        let a = anchor();
        assert_eq!(a.qpc_to_utc_us(a.qpc - FREQ / 2), a.utc_us - 500_000);
    }

    #[test]
    fn sub_tick_precision_truncates() {
        let a = anchor();
        // 3 ticks at 10MHz = 0.3µs → truncates to 0.
        assert_eq!(a.qpc_to_utc_us(a.qpc + 3), a.utc_us);
        // 13 ticks = 1.3µs → 1µs.
        assert_eq!(a.qpc_to_utc_us(a.qpc + 13), a.utc_us + 1);
    }

    #[test]
    fn no_overflow_at_large_uptime() {
        // ~213 days of uptime at 10MHz, near u64 QPC values seen in practice.
        let a = QpcAnchor {
            qpc: u64::MAX / 100,
            utc_us: 1_756_000_000_000_000,
            qpc_freq: FREQ,
        };
        let one_hour = FREQ * 3600;
        assert_eq!(a.qpc_to_utc_us(a.qpc + one_hour), a.utc_us + 3_600_000_000);
    }

    #[test]
    fn fast_path_matches_general_expression_at_10mhz() {
        let a = anchor();
        // Sign spread, values straddling multiples of 10, and range extremes
        // (qpc = 0 and qpc = u64::MAX both stay in-range around the anchor).
        let dticks: [i128; 21] = [
            0,
            1,
            -1,
            3,
            -3,
            9,
            -9,
            10,
            -10,
            11,
            -11,
            19,
            -19,
            20,
            -20,
            999_999_999_999,
            -4_999_999_999,
            -(anchor().qpc as i128),                 // qpc = 0
            u64::MAX as i128 - anchor().qpc as i128, // qpc = u64::MAX
            u64::MAX as i128 / 2,
            -(anchor().qpc as i128) + 7,
        ];
        for dt in dticks {
            let qpc = (a.qpc as i128 + dt) as u64;
            // The general expression, computed explicitly.
            let expected = a.utc_us as i128 + dt * 1_000_000 / a.qpc_freq as i128;
            assert_eq!(
                a.qpc_to_utc_us(qpc) as i128,
                expected,
                "fast path diverged at dticks={dt}"
            );
        }
    }

    #[test]
    fn general_path_still_serves_other_frequencies() {
        // A 3 MHz clock (not the 10 MHz fast path): 1 tick = 1/3 µs.
        let a = QpcAnchor {
            qpc: 9_000_000,
            utc_us: 1_756_000_000_000_000,
            qpc_freq: 3_000_000,
        };
        assert_eq!(a.qpc_to_utc_us(a.qpc), a.utc_us);
        assert_eq!(a.qpc_to_utc_us(a.qpc + 3_000_000), a.utc_us + 1_000_000);
        assert_eq!(a.qpc_to_utc_us(a.qpc - 1_500_000), a.utc_us - 500_000);
        // 4 ticks at 3MHz = 1.33µs → truncates to 1; -4 ticks → -1 (toward zero).
        assert_eq!(a.qpc_to_utc_us(a.qpc + 4), a.utc_us + 1);
        assert_eq!(a.qpc_to_utc_us(a.qpc - 4), a.utc_us - 1);
    }

    #[test]
    fn now_utc_us_is_a_plausible_wall_clock() {
        // Between 2020 and 2100, in µs: catches ms/ns unit mistakes.
        let t = now_utc_us();
        assert!(t > 1_577_836_800_000_000, "{t}");
        assert!(t < 4_102_444_800_000_000, "{t}");
    }

    #[test]
    fn ticks_and_ms_helpers() {
        let a = anchor();
        assert_eq!(a.ms_to_ticks(25), FREQ / 40);
        assert_eq!(a.ticks_to_us(a.qpc, a.qpc + FREQ / 1000), 1_000);
        assert_eq!(a.ticks_to_us(a.qpc + FREQ, a.qpc), -1_000_000);
    }
}
