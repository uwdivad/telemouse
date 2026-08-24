//! Wire format shared by every transport: Kafka payloads, localhost UDP
//! datagrams for the live viz, and JSONL recording files on disk.
//!
//! One JSON-encoded [`Envelope`] per Kafka message / UDP datagram / JSONL
//! line. JSON now for debuggability; the tagged envelope gives a stable seam
//! for a binary schema later.

use serde::{Deserialize, Serialize};

use crate::{batch::Batch, session::Marker, session::SessionConfig};

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

    pub fn from_json(s: &str) -> serde_json::Result<Self> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RawEvent;

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
        // Worst-case-ish events: large negative values everywhere.
        let mut b = batch();
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
        b.game = Some("some-long-process-name.exe".into());
        let s = Envelope::Batch(b).to_json().unwrap();
        assert!(
            s.len() < MAX_UDP_PAYLOAD,
            "serialized full batch is {} bytes",
            s.len()
        );
    }

    #[test]
    fn unknown_type_is_an_error() {
        assert!(Envelope::from_json(r#"{"type":"bogus"}"#).is_err());
    }
}
