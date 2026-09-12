//! Output sinks and the fan-out that feeds them.
//!
//! Every sink is independently fallible: one failing sink logs a warning and
//! bumps its error counter, and the others still receive the envelope. The
//! fan-out itself is a plain function over `[Box<dyn Sink>]` so the isolation
//! property is unit-testable with mock sinks.
//!
//! An envelope is serialized **once** per flush, by [`EnvelopeEncoder`], into a
//! buffer that is reused for the whole session; sinks receive the finished JSON
//! alongside the typed envelope and never re-encode it.

pub mod jsonl;
#[cfg(feature = "kafka")]
pub mod kafka;
pub mod udp;

use anyhow::{Context, Result};

pub use jsonl::JsonlSink;
#[cfg(feature = "kafka")]
pub use kafka::KafkaSink;
pub use udp::UdpSink;

pub trait Sink: Send {
    /// Stable short name, also the key used by [`crate::stats::Stats`].
    fn name(&self) -> &'static str;
    /// Deliver one envelope, already serialized as `payload` — sinks that ship
    /// JSON must use it rather than re-serializing. `topic` and `key` carry
    /// the routing (`Envelope::topic()`/`key()` or the `EnvelopeView`
    /// equivalents), so the per-batch path never needs an owned `Envelope`.
    fn send(&mut self, topic: &'static str, key: &str, payload: &str) -> Result<()>;
    /// Periodic housekeeping (flushes). Called roughly once per second.
    fn tick(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Serializes envelopes into one reused buffer: no per-sink, per-batch
/// allocation on the shipping path.
#[derive(Debug, Default)]
pub struct EnvelopeEncoder {
    buf: String,
}

impl EnvelopeEncoder {
    pub fn new() -> Self {
        Self {
            // Comfortably above a full 448-event batch, so the buffer stops
            // growing after the first flush.
            buf: String::with_capacity(64 * 1024),
        }
    }

    /// Encode `env` into the reused buffer. Read it back with [`Self::payload`]
    /// — split in two so the payload can be borrowed while the sinks are
    /// borrowed mutably. Takes anything serializable so the owned [`Envelope`]
    /// and the borrowing `EnvelopeView` share one path.
    pub fn encode<T: serde::Serialize>(&mut self, env: &T) -> Result<()> {
        // Serialize into the buffer's bytes, then validate UTF-8 exactly once
        // on the way back into the `String` — `payload` is then a free borrow,
        // not a second O(n) scan per flush.
        let mut bytes = std::mem::take(&mut self.buf).into_bytes();
        bytes.clear();
        let serialized = serde_json::to_writer(&mut bytes, env).context("serialize envelope");
        if serialized.is_err() {
            // Never leave a half-written payload readable.
            bytes.clear();
        }
        match String::from_utf8(bytes) {
            Ok(s) => {
                self.buf = s;
                serialized
            }
            // serde_json only ever writes UTF-8, so this is a bug; keep the
            // (emptied) buffer usable either way.
            Err(e) => {
                let mut bytes = e.into_bytes();
                bytes.clear();
                self.buf = String::from_utf8(bytes).expect("an empty buffer is valid utf-8");
                serialized.and(Err(anyhow::anyhow!("serialized envelope is not utf-8")))
            }
        }
    }

    /// The JSON written by the last successful [`Self::encode`].
    pub fn payload(&self) -> &str {
        &self.buf
    }

    /// Capacity of the reusable buffer, for tests.
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkFailure {
    pub sink: &'static str,
    pub error: String,
}

/// Deliver one envelope to every sink, collecting (not propagating) failures.
pub fn fan_out(
    sinks: &mut [Box<dyn Sink>],
    topic: &'static str,
    key: &str,
    payload: &str,
) -> Vec<SinkFailure> {
    let mut failures = Vec::new();
    for sink in sinks.iter_mut() {
        if let Err(e) = sink.send(topic, key, payload) {
            failures.push(SinkFailure {
                sink: sink.name(),
                error: format!("{e:#}"),
            });
        }
    }
    failures
}

/// Deliver one envelope to exactly one sink, by name.
///
/// The session envelope is re-sent to the live viz every few seconds so a viz
/// started after the agent still learns the CPI, the device names and the
/// anchor. It must not reach the others: a second `session` line in a
/// recording would change what the file means, and a second one on
/// `mouse.sessions` is a record nobody asked for. `None` when the sink is not
/// present (disabled, or it failed to start) or when it accepted the
/// envelope.
pub fn send_to(
    sinks: &mut [Box<dyn Sink>],
    name: &str,
    topic: &'static str,
    key: &str,
    payload: &str,
) -> Option<SinkFailure> {
    let sink = sinks.iter_mut().find(|s| s.name() == name)?;
    match sink.send(topic, key, payload) {
        Ok(()) => None,
        Err(e) => Some(SinkFailure {
            sink: sink.name(),
            error: format!("{e:#}"),
        }),
    }
}

/// Housekeeping pass over every sink, same isolation rules.
pub fn tick_all(sinks: &mut [Box<dyn Sink>]) -> Vec<SinkFailure> {
    let mut failures = Vec::new();
    for sink in sinks.iter_mut() {
        if let Err(e) = sink.tick() {
            failures.push(SinkFailure {
                sink: sink.name(),
                error: format!("{e:#}"),
            });
        }
    }
    failures
}

#[cfg(test)]
pub(crate) mod mock {
    use std::sync::{Arc, Mutex};

    use telemouse_core::Envelope;

    use super::*;

    /// Records everything it receives. `received` holds the typed envelopes
    /// parsed back from the JSON payloads (the trait no longer hands sinks a
    /// typed envelope); payloads that do not parse are still counted.
    #[derive(Clone, Default)]
    pub struct RecordingSink {
        pub name: &'static str,
        pub received: Arc<Mutex<Vec<Envelope>>>,
        pub payloads: Arc<Mutex<Vec<String>>>,
        pub ticks: Arc<Mutex<usize>>,
    }

    impl RecordingSink {
        pub fn new(name: &'static str) -> Self {
            Self {
                name,
                received: Arc::new(Mutex::new(Vec::new())),
                payloads: Arc::new(Mutex::new(Vec::new())),
                ticks: Arc::new(Mutex::new(0)),
            }
        }

        pub fn count(&self) -> usize {
            self.payloads.lock().unwrap().len()
        }
    }

    impl Sink for RecordingSink {
        fn name(&self) -> &'static str {
            self.name
        }
        fn send(&mut self, _topic: &'static str, _key: &str, payload: &str) -> Result<()> {
            if let Ok(env) = Envelope::from_json(payload) {
                self.received.lock().unwrap().push(env);
            }
            self.payloads.lock().unwrap().push(payload.to_string());
            Ok(())
        }
        fn tick(&mut self) -> Result<()> {
            *self.ticks.lock().unwrap() += 1;
            Ok(())
        }
    }

    /// Always fails.
    pub struct FailingSink {
        pub name: &'static str,
        pub calls: Arc<Mutex<usize>>,
    }

    impl FailingSink {
        pub fn new(name: &'static str) -> Self {
            Self {
                name,
                calls: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl Sink for FailingSink {
        fn name(&self) -> &'static str {
            self.name
        }
        fn send(&mut self, _topic: &'static str, _key: &str, _payload: &str) -> Result<()> {
            *self.calls.lock().unwrap() += 1;
            Err(anyhow::anyhow!("boom"))
        }
        fn tick(&mut self) -> Result<()> {
            Err(anyhow::anyhow!("tick boom"))
        }
    }
}

#[cfg(test)]
mod tests {
    use telemouse_core::{Batch, Envelope};

    use super::mock::{FailingSink, RecordingSink};
    use super::*;
    use crate::stats::Stats;

    fn env() -> Envelope {
        Envelope::Batch(Batch {
            session_id: "s-1".into(),
            seq_no: 0,
            ts_anchor_us: 0,
            game: None,
            pointer_locked: false,
            screen_w: 0,
            screen_h: 0,
            cursor_x: None,
            cursor_y: None,
            drops_since_last: 0,
            abs_frames_since_last: 0,
            events: vec![],
        })
    }

    #[test]
    fn a_failing_sink_does_not_starve_the_others() {
        let first = RecordingSink::new("udp");
        let last = RecordingSink::new("jsonl");
        let failing = FailingSink::new("kafka");
        let failing_calls = failing.calls.clone();
        let mut sinks: Vec<Box<dyn Sink>> = vec![
            Box::new(first.clone()),
            Box::new(failing),
            Box::new(last.clone()),
        ];

        let e = env();
        let failures = fan_out(&mut sinks, e.topic(), e.key(), "{}");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].sink, "kafka");
        assert!(failures[0].error.contains("boom"));
        assert_eq!(*failing_calls.lock().unwrap(), 1);
        // Both healthy sinks got it, including the one *after* the failure.
        assert_eq!(first.count(), 1);
        assert_eq!(last.count(), 1);
    }

    #[test]
    fn failures_map_onto_per_sink_counters() {
        let mut sinks: Vec<Box<dyn Sink>> = vec![
            Box::new(FailingSink::new("kafka")),
            Box::new(FailingSink::new("udp")),
        ];
        let stats = Stats::default();
        let e = env();
        for f in fan_out(&mut sinks, e.topic(), e.key(), "{}") {
            stats.count_sink_error(f.sink);
        }
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_errors, 1);
        assert_eq!(snap.udp_errors, 1);
        assert_eq!(snap.jsonl_errors, 0);
    }

    #[test]
    fn a_targeted_send_reaches_one_sink_and_no_other() {
        let udp = RecordingSink::new("udp");
        let jsonl = RecordingSink::new("jsonl");
        let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(udp.clone()), Box::new(jsonl.clone())];
        let e = env();
        assert!(send_to(&mut sinks, "udp", e.topic(), e.key(), "{}").is_none());
        assert_eq!(udp.count(), 1);
        assert_eq!(jsonl.count(), 0, "the recording must not see it");

        // A sink that is not running is not an error: there is nobody to tell.
        assert!(send_to(&mut sinks, "kafka", e.topic(), e.key(), "{}").is_none());

        // A failure is reported, and still reaches nobody else.
        let mut sinks: Vec<Box<dyn Sink>> =
            vec![Box::new(FailingSink::new("udp")), Box::new(jsonl.clone())];
        let failure = send_to(&mut sinks, "udp", e.topic(), e.key(), "{}").unwrap();
        assert_eq!(failure.sink, "udp");
        assert_eq!(jsonl.count(), 0);
    }

    #[test]
    fn tick_reaches_every_sink() {
        let ok = RecordingSink::new("jsonl");
        let mut sinks: Vec<Box<dyn Sink>> =
            vec![Box::new(FailingSink::new("kafka")), Box::new(ok.clone())];
        let failures = tick_all(&mut sinks);
        assert_eq!(failures.len(), 1);
        assert_eq!(*ok.ticks.lock().unwrap(), 1);
    }

    #[test]
    fn the_encoder_matches_core_and_reuses_its_buffer() {
        let mut enc = EnvelopeEncoder::new();
        let e = env();
        let expected = e.to_json().unwrap();
        enc.encode(&e).unwrap();
        assert_eq!(enc.payload(), expected);
        let capacity = enc.capacity();
        // A second, smaller envelope must not reallocate or leave a tail behind.
        let marker = Envelope::Marker(telemouse_core::Marker {
            session_id: "s-1".into(),
            seq_no: 0,
            ts_qpc: 1,
            ts_utc_us: 2,
            label: "hotkey".into(),
        });
        enc.encode(&marker).unwrap();
        assert_eq!(enc.payload(), marker.to_json().unwrap());
        assert_eq!(enc.capacity(), capacity);
        // And round-trips: what sinks write is what consumers read.
        enc.encode(&e).unwrap();
        assert_eq!(Envelope::from_json(enc.payload()).unwrap(), e);
    }

    #[test]
    fn every_sink_receives_the_same_bytes() {
        let a = RecordingSink::new("udp");
        let b = RecordingSink::new("jsonl");
        let mut sinks: Vec<Box<dyn Sink>> = vec![Box::new(a.clone()), Box::new(b.clone())];
        let mut enc = EnvelopeEncoder::new();
        let e = env();
        enc.encode(&e).unwrap();
        let payload = enc.payload().to_string();
        fan_out(&mut sinks, e.topic(), e.key(), &payload);
        assert_eq!(*a.payloads.lock().unwrap(), *b.payloads.lock().unwrap());
        assert_eq!(a.payloads.lock().unwrap()[0], payload);
    }
}
