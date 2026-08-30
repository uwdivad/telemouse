use serde::{Deserialize, Serialize};

use crate::event::RawEvent;

/// The batch envelope shipped to Kafka (`mouse.events`) and fanned out over
/// localhost UDP for the live viz. Assembled by the shipping thread every
/// ~25ms; the capture hot path never builds these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    pub session_id: String,
    /// Monotonic per-session batch counter; gaps at a consumer mean loss
    /// downstream of the ring buffer (which reports its own drops).
    pub seq_no: u64,
    /// UTC µs of the first event in `events`, mapped via the session anchor.
    pub ts_anchor_us: i64,
    /// Foreground process name at assembly time (lowercase, no path), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    /// Pointer-lock heuristic result: cursor frozen while deltas flow.
    pub pointer_locked: bool,
    pub screen_w: u32,
    pub screen_h: u32,
    /// Sampled cursor position; only meaningful in desktop (non-locked) mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_y: Option<i32>,
    /// Ring-buffer drops since the previous batch. Should be zero; nonzero is
    /// a data-quality signal, never a capture stall.
    pub drops_since_last: u32,
    /// Absolute-motion `WM_INPUT` frames (RDP, tablets, virtual devices) seen
    /// and discarded since the previous batch — recorded so consumers can
    /// audit data quality retroactively. Defaults to 0 for old recordings.
    #[serde(default)]
    pub abs_frames_since_last: u32,
    pub events: Vec<RawEvent>,
}

impl Batch {
    pub fn total_counts(&self) -> (i64, i64) {
        self.events
            .iter()
            .fold((0i64, 0i64), |(x, y), e| (x + e.dx as i64, y + e.dy as i64))
    }

    /// Borrow this batch as a serialize-side [`BatchView`].
    pub fn as_view(&self) -> BatchView<'_> {
        BatchView {
            session_id: &self.session_id,
            seq_no: self.seq_no,
            ts_anchor_us: self.ts_anchor_us,
            game: self.game.as_deref(),
            pointer_locked: self.pointer_locked,
            screen_w: self.screen_w,
            screen_h: self.screen_h,
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            drops_since_last: self.drops_since_last,
            abs_frames_since_last: self.abs_frames_since_last,
            events: &self.events,
        }
    }
}

/// Serialize-side borrowing mirror of [`Batch`], for the shipping thread's
/// zero-realloc flush path: the batcher keeps its event `Vec`, the session id
/// and process name stay wherever they already live, and the view serializes
/// straight into a reused buffer. Field order and every `skip_serializing_if`
/// match [`Batch`] exactly, so the JSON is byte-identical (the wire test
/// enforces this). Serialize-only by design — the deserialize side keeps
/// using the owned [`Batch`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BatchView<'a> {
    pub session_id: &'a str,
    pub seq_no: u64,
    pub ts_anchor_us: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game: Option<&'a str>,
    pub pointer_locked: bool,
    pub screen_w: u32,
    pub screen_h: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_y: Option<i32>,
    pub drops_since_last: u32,
    pub abs_frames_since_last: u32,
    pub events: &'a [RawEvent],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::buttons;

    fn sample() -> Batch {
        Batch {
            session_id: "s-1".into(),
            seq_no: 42,
            ts_anchor_us: 1_756_000_000_000_000,
            game: Some("cs2.exe".into()),
            pointer_locked: true,
            screen_w: 2560,
            screen_h: 1440,
            cursor_x: None,
            cursor_y: None,
            drops_since_last: 0,
            abs_frames_since_last: 0,
            events: vec![
                RawEvent {
                    ts_qpc: 100,
                    dx: 3,
                    dy: -1,
                    ..Default::default()
                },
                RawEvent {
                    ts_qpc: 110,
                    dx: -1,
                    dy: 2,
                    buttons: buttons::LEFT_DOWN,
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn json_roundtrip() {
        let b = sample();
        let s = serde_json::to_string(&b).unwrap();
        let back: Batch = serde_json::from_str(&s).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn absent_options_are_not_serialized() {
        let s = serde_json::to_string(&sample()).unwrap();
        // None-valued cursor/game fields are omitted, not written as null.
        assert!(!s.contains("cursor_x"));
        assert!(!s.contains("null"));
    }

    #[test]
    fn pre_abs_frames_json_still_parses() {
        let mut b = sample();
        b.abs_frames_since_last = 0;
        let mut v: serde_json::Value = serde_json::to_value(&b).unwrap();
        v.as_object_mut().unwrap().remove("abs_frames_since_last");
        let back: Batch = serde_json::from_value(v).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn total_counts_sums_deltas() {
        assert_eq!(sample().total_counts(), (2, 1));
    }
}
