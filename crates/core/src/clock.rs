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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QpcAnchor {
    /// QPC reading at the anchor instant.
    pub qpc: u64,
    /// UTC microseconds since the Unix epoch at the same instant.
    pub utc_us: i64,
    /// QPC ticks per second (`QueryPerformanceFrequency`).
    pub qpc_freq: u64,
}

impl QpcAnchor {
    /// Map a QPC reading to UTC microseconds since the Unix epoch.
    ///
    /// Works for readings before the anchor too (negative delta). Uses i128
    /// internally so tick→µs scaling cannot overflow for any realistic uptime.
    pub fn qpc_to_utc_us(&self, qpc: u64) -> i64 {
        let dticks = qpc as i128 - self.qpc as i128;
        // Fast path for the ubiquitous 10 MHz QPC frequency: the product is
        // exact in i128, so `dticks * 1_000_000 / 10_000_000` and `dticks / 10`
        // are the same truncating division — bit-identical for negative deltas
        // too, since i128 division truncates toward zero in both forms. The
        // parity test below checks this across the sign and range spread.
        let dus = if self.qpc_freq == 10_000_000 {
            dticks / 10
        } else {
            dticks * 1_000_000 / self.qpc_freq as i128
        };
        (self.utc_us as i128 + dus) as i64
    }

    /// Microseconds elapsed from `from_qpc` to `to_qpc` (may be negative).
    pub fn ticks_to_us(&self, from_qpc: u64, to_qpc: u64) -> i64 {
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
