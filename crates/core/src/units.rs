//! Raw-count → physical-unit conversions. Applied only in consumers; the
//! wire always carries raw counts.

use crate::session::GameSens;

/// Hand travel in centimeters for a given count total at a mouse CPI.
pub fn counts_to_cm(counts: f64, cpi: f64) -> f64 {
    counts / cpi * 2.54
}

/// Aim-space yaw degrees for a horizontal count total.
pub fn counts_to_yaw_deg(counts: f64, g: &GameSens) -> f64 {
    counts * g.sens * g.yaw_coeff
}

/// Aim-space pitch degrees for a vertical count total.
pub fn counts_to_pitch_deg(counts: f64, g: &GameSens) -> f64 {
    counts * g.sens * g.pitch_coeff
}

/// Wrap an accumulated yaw angle into (-180, 180].
pub fn wrap_yaw_deg(deg: f64) -> f64 {
    let mut d = deg % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d <= -180.0 {
        d += 360.0;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cm_conversion() {
        // 1600 counts at 1600 CPI = 1 inch = 2.54cm.
        assert!((counts_to_cm(1600.0, 1600.0) - 2.54).abs() < 1e-12);
    }

    #[test]
    fn cs2_yaw_example_from_plan() {
        // deg = counts × sens × 0.022
        let g = GameSens {
            sens: 2.0,
            yaw_coeff: 0.022,
            pitch_coeff: 0.022,
        };
        assert!((counts_to_yaw_deg(100.0, &g) - 4.4).abs() < 1e-12);
    }

    #[test]
    fn yaw_wraps_at_180() {
        assert_eq!(wrap_yaw_deg(180.0), 180.0);
        assert_eq!(wrap_yaw_deg(181.0), -179.0);
        assert_eq!(wrap_yaw_deg(-181.0), 179.0);
        assert_eq!(wrap_yaw_deg(720.0), 0.0);
        assert_eq!(wrap_yaw_deg(-540.0), 180.0);
    }
}
