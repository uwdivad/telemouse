//! Minimal UTC formatting for report headers and the session listing.
//!
//! A full date-time crate would be a heavy dependency for "print the session
//! start"; this is Howard Hinnant's `civil_from_days` in a dozen lines.

/// Civil (year, month, day) from days since the Unix epoch.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-08-23 14:05:09.123 UTC` from microseconds since the Unix epoch.
pub fn format_utc_us(us: i64) -> String {
    let (secs, sub_us) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    format!(
        "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{:03} UTC",
        sub_us / 1000
    )
}

/// `1h 04m 12.3s`, `4m 12.3s`, or `12.34s` depending on magnitude.
pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "n/a".to_string();
    }
    let total = secs;
    let h = (total / 3600.0).floor() as u64;
    let m = ((total - h as f64 * 3600.0) / 60.0).floor() as u64;
    let s = total - h as f64 * 3600.0 - m as f64 * 60.0;
    if h > 0 {
        format!("{h}h {m:02}m {s:04.1}s")
    } else if m > 0 {
        format!("{m}m {s:04.1}s")
    } else {
        format!("{s:.2}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(format_utc_us(0), "1970-01-01 00:00:00.000 UTC");
    }

    #[test]
    fn known_instant() {
        // 1756000000 s = 2025-08-24 01:46:40 UTC
        assert_eq!(
            format_utc_us(1_756_000_000_000_000),
            "2025-08-24 01:46:40.000 UTC"
        );
    }

    #[test]
    fn sub_second_and_leap_year_day() {
        assert_eq!(format_utc_us(1_500_500), "1970-01-01 00:00:01.500 UTC");
        // 2024-02-29 00:00:00 UTC
        assert_eq!(
            format_utc_us(1_709_164_800_000_000),
            "2024-02-29 00:00:00.000 UTC"
        );
    }

    #[test]
    fn durations_scale_by_magnitude() {
        assert_eq!(format_duration(12.345), "12.35s");
        assert_eq!(format_duration(252.3), "4m 12.3s");
        assert_eq!(format_duration(3852.3), "1h 04m 12.3s");
        assert_eq!(format_duration(-1.0), "n/a");
    }
}
