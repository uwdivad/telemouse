use serde::{Deserialize, Serialize};

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
        let dus = dticks * 1_000_000 / self.qpc_freq as i128;
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
            qpc: 5_000_000_000,          // 500s of uptime
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
        assert_eq!(
            a.qpc_to_utc_us(a.qpc + one_hour),
            a.utc_us + 3_600_000_000
        );
    }

    #[test]
    fn ticks_and_ms_helpers() {
        let a = anchor();
        assert_eq!(a.ms_to_ticks(25), FREQ / 40);
        assert_eq!(a.ticks_to_us(a.qpc, a.qpc + FREQ / 1000), 1_000);
        assert_eq!(a.ticks_to_us(a.qpc + FREQ, a.qpc), -1_000_000);
    }
}
