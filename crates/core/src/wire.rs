//! Wire format shared by every transport: Kafka payloads, localhost UDP
//! datagrams for the live viz, and JSONL recording files on disk.
//!
//! One JSON-encoded [`Envelope`] per Kafka message / UDP datagram / JSONL
//! line. JSON now for debuggability; the tagged envelope gives a stable seam
//! for a binary schema later.

use serde::{Deserialize, Serialize};

use crate::{batch::Batch, batch::BatchView, session::Marker, session::SessionConfig};

pub const TOPIC_EVENTS: &str = "mouse.events";
pub const TOPIC_SESSIONS: &str = "mouse.sessions";
pub const TOPIC_MARKERS: &str = "mouse.markers";

/// Keep serialized batches under a single UDP datagram on loopback.
pub const MAX_UDP_PAYLOAD: usize = 60_000;
/// Event cap per batch chosen so even a worst-case batch (every field at its
/// widest value) serializes under [`MAX_UDP_PAYLOAD`] (~130 bytes/event); the
/// test below enforces it. At 25ms windows this cap only binds above ~17KHz.
pub const MAX_EVENTS_PER_BATCH: usize = 448;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Envelope {
    Session(SessionConfig),
    Batch(Batch),
    Marker(Marker),
}

impl Envelope {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Parse an envelope.
    ///
    /// Serde's internally-tagged enum has to buffer the whole document as a
    /// generic `Content` tree before it can see the tag, which on a batch
    /// costs ~3× the direct struct parse (measured: 116µs vs 37µs for a full
    /// 448-event batch, 6.8µs vs 2.5µs for a 25-event one). Every envelope
    /// telemouse itself writes starts with `{"type":"<tag>",` verbatim, so
    /// that prefix is matched first and the variant's struct is parsed
    /// directly (the `type` key is then just an ignored unknown field).
    /// Anything else — reordered keys, whitespace, hand-written JSON — takes
    /// the general tagged path, so the accepted language is unchanged.
    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        if let Some(tag) = fast_tag(s) {
            return match tag {
                FastTag::Batch => serde_json::from_str(s).map(Envelope::Batch),
                FastTag::Session => serde_json::from_str(s).map(Envelope::Session),
                FastTag::Marker => serde_json::from_str(s).map(Envelope::Marker),
            };
        }
        serde_json::from_str(s)
    }

    /// Kafka topic this envelope belongs on.
    pub fn topic(&self) -> &'static str {
        match self {
            Envelope::Session(_) => TOPIC_SESSIONS,
            Envelope::Batch(_) => TOPIC_EVENTS,
            Envelope::Marker(_) => TOPIC_MARKERS,
        }
    }

    /// Kafka message key: always the session id, so a session stays ordered
    /// within one partition.
    pub fn key(&self) -> &str {
        match self {
            Envelope::Session(s) => &s.session_id,
            Envelope::Batch(b) => &b.session_id,
            Envelope::Marker(m) => &m.session_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FastTag {
    Batch,
    Session,
    Marker,
}

/// The exact prefixes serde emits for each variant of the tagged enum, so a
/// document that matches one is known to carry that tag as its first key.
fn fast_tag(s: &str) -> Option<FastTag> {
    const BATCH: &str = "{\"type\":\"batch\",";
    const SESSION: &str = "{\"type\":\"session\",";
    const MARKER: &str = "{\"type\":\"marker\",";
    if s.starts_with(BATCH) {
        Some(FastTag::Batch)
    } else if s.starts_with(SESSION) {
        Some(FastTag::Session)
    } else if s.starts_with(MARKER) {
        Some(FastTag::Marker)
    } else {
        None
    }
}

/// Serialize-side borrowing mirror of [`Envelope`], carrying a [`BatchView`]
/// where the owned enum carries a [`Batch`]. Same `type` tag, same variant
/// name, so serializing it produces JSON byte-identical to the owned path
/// (the parity test enforces this). Only the batch variant exists: batches
/// are the 40/s hot path; session and marker envelopes are rare enough that
/// the owned [`Envelope`] stays fine for them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvelopeView<'a> {
    Batch(BatchView<'a>),
}

impl EnvelopeView<'_> {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Kafka topic this envelope belongs on.
    pub fn topic(&self) -> &'static str {
        match self {
            EnvelopeView::Batch(_) => TOPIC_EVENTS,
        }
    }

    /// Kafka message key: the session id, matching [`Envelope::key`].
    pub fn key(&self) -> &str {
        match self {
            EnvelopeView::Batch(b) => b.session_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RawEvent;
    use crate::event::buttons;

    fn batch() -> Batch {
        Batch {
            session_id: "s-1".into(),
            seq_no: 0,
            ts_anchor_us: 1,
            game: None,
            pointer_locked: false,
            screen_w: 1920,
            screen_h: 1080,
            cursor_x: Some(10),
            cursor_y: Some(20),
            drops_since_last: 0,
            abs_frames_since_last: 0,
            events: vec![RawEvent::default()],
        }
    }

    #[test]
    fn tagged_roundtrip() {
        let e = Envelope::Batch(batch());
        let s = e.to_json().unwrap();
        assert!(s.contains(r#""type":"batch""#));
        assert_eq!(Envelope::from_json(&s).unwrap(), e);
    }

    #[test]
    fn topic_and_key_routing() {
        let e = Envelope::Batch(batch());
        assert_eq!(e.topic(), TOPIC_EVENTS);
        assert_eq!(e.key(), "s-1");
    }

    #[test]
    fn full_batch_fits_in_udp_datagram() {
        // Worst case at every knob, not just the events: the longest session
        // id the recording rules allow and the longest game name a process
        // can have, both of which ride along in every batch.
        let mut b = batch();
        b.session_id = "s".repeat(crate::recordings::MAX_ID_LEN);
        b.game = Some(format!("{}.exe", "g".repeat(251)));
        assert_eq!(b.game.as_deref().map(str::len), Some(255));
        b.events = (0..MAX_EVENTS_PER_BATCH)
            .map(|i| RawEvent {
                ts_qpc: u64::MAX - i as u64,
                dx: i32::MIN,
                dy: i32::MIN,
                buttons: 0x03FF,
                wheel: i16::MIN,
                wheel_h: i16::MIN,
                device_ix: u8::MAX,
            })
            .collect();
        let s = Envelope::Batch(b).to_json().unwrap();
        assert!(
            s.len() < MAX_UDP_PAYLOAD,
            "serialized full batch is {} bytes",
            s.len()
        );
    }

    /// The owned and view serialize paths must be byte-identical — the wire
    /// format is defined by [`Envelope`], and [`EnvelopeView`] only exists to
    /// avoid allocation, never to diverge.
    fn assert_view_parity(b: Batch) {
        let owned = Envelope::Batch(b.clone()).to_json().unwrap();
        let view = EnvelopeView::Batch(b.as_view()).to_json().unwrap();
        assert_eq!(owned, view);
    }

    #[test]
    fn view_json_matches_owned_byte_for_byte() {
        // Representative batch: mixed zero/nonzero rare event fields, Some
        // cursor and game, several events.
        let mut b = batch();
        b.seq_no = 42;
        b.game = Some("cs2.exe".into());
        b.pointer_locked = true;
        b.drops_since_last = 3;
        b.abs_frames_since_last = 1;
        b.events = vec![
            RawEvent {
                ts_qpc: 100,
                dx: 3,
                dy: -1,
                ..Default::default()
            },
            RawEvent {
                ts_qpc: 110,
                dx: 0,
                dy: 0,
                buttons: buttons::LEFT_DOWN,
                wheel: -120,
                wheel_h: 0,
                device_ix: 0,
            },
            RawEvent {
                ts_qpc: 120,
                dx: -7,
                dy: 2,
                buttons: 0,
                wheel: 0,
                wheel_h: 120,
                device_ix: 1,
            },
        ];
        assert_view_parity(b);
    }

    #[test]
    fn view_parity_with_empty_events_and_absent_options() {
        let mut b = batch();
        b.game = None;
        b.cursor_x = None;
        b.cursor_y = None;
        b.events = Vec::new();
        assert_view_parity(b);
    }

    #[test]
    fn view_parity_with_every_field_populated() {
        let mut b = batch();
        b.game = Some("some-long-process-name.exe".into());
        b.cursor_x = Some(-1);
        b.cursor_y = Some(i32::MAX);
        b.drops_since_last = u32::MAX;
        b.abs_frames_since_last = u32::MAX;
        b.events = vec![RawEvent {
            ts_qpc: u64::MAX,
            dx: i32::MIN,
            dy: i32::MIN,
            buttons: buttons::MASK,
            wheel: i16::MIN,
            wheel_h: i16::MIN,
            device_ix: u8::MAX,
        }];
        assert_view_parity(b);
    }

    #[test]
    fn view_topic_and_key_match_owned() {
        let b = batch();
        let v = EnvelopeView::Batch(b.as_view());
        assert_eq!(v.topic(), TOPIC_EVENTS);
        assert_eq!(v.key(), "s-1");
    }

    /// The prefix fast path must accept exactly what the general tagged path
    /// accepts, and produce the same value — for every variant, and for the
    /// documents that miss the prefix (reordered keys, leading whitespace),
    /// which must still parse via the general path.
    #[test]
    fn fast_path_matches_general_tagged_parse() {
        let general = |s: &str| -> Envelope { serde_json::from_str(s).unwrap() };
        let mut b = batch();
        b.game = Some("cs2.exe".into());
        b.events.push(RawEvent {
            ts_qpc: 5,
            dx: -2,
            dy: 9,
            buttons: 1,
            ..Default::default()
        });
        let cfg = SessionConfig {
            session_id: "s-1".into(),
            started_utc_us: 1,
            qpc_freq: 10_000_000,
            anchor: crate::QpcAnchor {
                qpc: 1,
                utc_us: 1,
                qpc_freq: 10_000_000,
            },
            anchor_uncertainty_us: Some(3),
            mouse_cpi: 1600.0,
            devices: vec!["mouse".into()],
            games: Default::default(),
            monitors: vec![],
            capture_version: "t".into(),
            coalesce_ms: 2,
            ..Default::default()
        };
        let m = Marker {
            session_id: "s-1".into(),
            seq_no: 7,
            ts_qpc: 9,
            ts_utc_us: 10,
            label: "round".into(),
        };
        for e in [
            Envelope::Batch(b),
            Envelope::Session(cfg),
            Envelope::Marker(m),
        ] {
            let s = e.to_json().unwrap();
            assert!(
                fast_tag(&s).is_some(),
                "own output must hit the fast path: {s}"
            );
            assert_eq!(Envelope::from_json(&s).unwrap(), e);
            assert_eq!(Envelope::from_json(&s).unwrap(), general(&s));

            // Same document, tag not first: general path, same value.
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            let mut keys: Vec<_> = v.as_object().unwrap().iter().collect();
            keys.reverse();
            let reordered = serde_json::Value::Object(
                keys.into_iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            )
            .to_string();
            assert!(fast_tag(&reordered).is_none() || reordered.starts_with("{\"type\""));
            assert_eq!(Envelope::from_json(&reordered).unwrap(), e);
            let padded = format!("  {s}");
            assert!(fast_tag(&padded).is_none());
            assert_eq!(Envelope::from_json(&padded).unwrap(), e);
        }
    }

    #[test]
    fn fast_path_rejects_what_the_general_path_rejects() {
        // Prefix matches but the body is not a batch.
        assert!(Envelope::from_json(r#"{"type":"batch","seq_no":"x"}"#).is_err());
        assert!(Envelope::from_json(r#"{"type":"batch","#).is_err());
        // Trailing garbage after a valid document is still an error.
        let s = Envelope::Batch(batch()).to_json().unwrap() + "x";
        assert!(Envelope::from_json(&s).is_err());
    }

    #[test]
    fn unknown_type_is_an_error() {
        assert!(Envelope::from_json(r#"{"type":"bogus"}"#).is_err());
    }
}
