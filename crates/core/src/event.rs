use serde::{Deserialize, Serialize};

/// Button-transition bitfield values for [`RawEvent::buttons`].
///
/// These are numerically identical to the Win32 `RI_MOUSE_*` transition flags
/// so the capture hot path can mask `RAWMOUSE.usButtonFlags` straight through
/// without translation.
pub mod buttons {
    pub const LEFT_DOWN: u16 = 0x0001;
    pub const LEFT_UP: u16 = 0x0002;
    pub const RIGHT_DOWN: u16 = 0x0004;
    pub const RIGHT_UP: u16 = 0x0008;
    pub const MIDDLE_DOWN: u16 = 0x0010;
    pub const MIDDLE_UP: u16 = 0x0020;
    pub const X1_DOWN: u16 = 0x0040;
    pub const X1_UP: u16 = 0x0080;
    pub const X2_DOWN: u16 = 0x0100;
    pub const X2_UP: u16 = 0x0200;
    /// All transition bits carried on the wire.
    pub const MASK: u16 = 0x03FF;
    /// Any button-down transition.
    pub const ANY_DOWN: u16 = LEFT_DOWN | RIGHT_DOWN | MIDDLE_DOWN | X1_DOWN | X2_DOWN;
    /// Any button-up transition.
    pub const ANY_UP: u16 = LEFT_UP | RIGHT_UP | MIDDLE_UP | X1_UP | X2_UP;
}

/// One raw mouse input event, exactly as captured.
///
/// Fixed-size and `Copy` so the capture thread can push it through the SPSC
/// ring buffer with zero allocation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawEvent {
    /// `QueryPerformanceCounter` value at receipt of `WM_INPUT`.
    pub ts_qpc: u64,
    /// Raw HID x delta (pre-acceleration counts).
    pub dx: i32,
    /// Raw HID y delta (positive = down, per HID convention).
    pub dy: i32,
    /// Button transition bitfield, see [`buttons`].
    ///
    /// Zero-valued rare fields (buttons, wheels, device index) are omitted
    /// from JSON entirely — consumers must treat a missing field as 0. This
    /// keeps the typical motion-only event small on the wire.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub buttons: u16,
    /// Wheel delta if this event carried one (signed, ±120 per detent typical).
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub wheel: i16,
    /// Horizontal (tilt) wheel delta, `RI_MOUSE_HWHEEL`.
    #[serde(default, skip_serializing_if = "is_zero_i16")]
    pub wheel_h: i16,
    /// Index into [`crate::session::SessionConfig::devices`] identifying which
    /// HID device produced this event. 0 when unknown (and for recordings made
    /// before device tracking existed).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub device_ix: u8,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}
fn is_zero_i16(v: &i16) -> bool {
    *v == 0
}
fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

impl RawEvent {
    pub fn is_click_down(&self) -> bool {
        self.buttons & buttons::ANY_DOWN != 0
    }

    pub fn is_click_up(&self) -> bool {
        self.buttons & buttons::ANY_UP != 0
    }

    pub fn has_motion(&self) -> bool {
        self.dx != 0 || self.dy != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_bits_are_disjoint_and_cover_mask() {
        let all = buttons::ANY_DOWN | buttons::ANY_UP;
        assert_eq!(all, buttons::MASK);
        // Each constant is a single distinct bit.
        let bits = [
            buttons::LEFT_DOWN,
            buttons::LEFT_UP,
            buttons::RIGHT_DOWN,
            buttons::RIGHT_UP,
            buttons::MIDDLE_DOWN,
            buttons::MIDDLE_UP,
            buttons::X1_DOWN,
            buttons::X1_UP,
            buttons::X2_DOWN,
            buttons::X2_UP,
        ];
        let mut seen = 0u16;
        for b in bits {
            assert_eq!(b.count_ones(), 1);
            assert_eq!(seen & b, 0, "overlapping bit {b:#x}");
            seen |= b;
        }
    }

    #[test]
    fn click_predicates() {
        let ev = RawEvent {
            buttons: buttons::LEFT_DOWN,
            ..Default::default()
        };
        assert!(ev.is_click_down());
        assert!(!ev.is_click_up());
        assert!(!ev.has_motion());

        let mv = RawEvent {
            dx: -3,
            dy: 1,
            ..Default::default()
        };
        assert!(mv.has_motion());
        assert!(!mv.is_click_down());
    }

    #[test]
    fn json_roundtrip() {
        let ev = RawEvent {
            ts_qpc: 123_456_789_012,
            dx: -5,
            dy: 7,
            buttons: buttons::RIGHT_DOWN,
            wheel: -120,
            wheel_h: 120,
            device_ix: 1,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: RawEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn pre_device_tracking_json_still_parses() {
        // A line written before wheel_h/device_ix existed.
        let ev: RawEvent =
            serde_json::from_str(r#"{"ts_qpc":1,"dx":2,"dy":3,"buttons":0,"wheel":0}"#).unwrap();
        assert_eq!(ev.wheel_h, 0);
        assert_eq!(ev.device_ix, 0);
    }
}
