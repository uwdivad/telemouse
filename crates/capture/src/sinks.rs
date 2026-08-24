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
pub mod kafka;
pub mod udp;

use anyhow::{Context, Result};
use telemouse_core::Envelope;

pub use jsonl::JsonlSink;
pub use kafka::KafkaSink;
pub use udp::UdpSink;

pub trait Sink: Send {
    /// Stable short name, also the key used by [`crate::stats::Stats`].
    fn name(&self) -> &'static str;
    /// Deliver one envelope. `payload` is `env` already serialized as JSON —
    /// sinks that ship JSON must use it rather than re-serializing.
    fn send(&mut self, env: &Envelope, payload: &str) -> Result<()>;
    /// Periodic housekeeping (flushes). Called roughly once per second.
    fn tick(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Serializes envelopes into one reused buffer: no per-sink, per-batch
/// allocation on the shipping path.
#[derive(Debug, Default)]
pub struct EnvelopeEncoder {
    buf: Vec<u8>,
}

impl EnvelopeEncoder {
    pub fn new() -> Self {
        Self {
            // Comfortably above a full 448-event batch, so the buffer stops
            // growing after the first flush.
            buf: Vec::with_capacity(64 * 1024),
        }
    }

    /// Encode `env` into the reused buffer. Read it back with [`Self::payload`]
    /// — split in two so the payload can be borrowed while the sinks are
    /// borrowed mutably.
    pub fn encode(&mut self, env: &Envelope) -> Result<()> {
        self.buf.clear();
        serde_json::to_writer(&mut self.buf, env).context("serialize envelope")?;
        // serde_json only ever writes UTF-8; check once here so `payload` is
        // infallible on the shipping path.
        std::str::from_utf8(&self.buf).context("serialized envelope is not utf-8")?;
        Ok(())
    }

    /// The JSON written by the last successful [`Self::encode`].
    pub fn payload(&self) -> &str {
        std::str::from_utf8(&self.buf).unwrap_or("")
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
pub fn fan_out(sinks: &mut [Box<dyn Sink>], env: &Envelope, payload: &str) -> Vec<SinkFailure> {
    let mut failures = Vec::new();
    for sink in sinks.iter_mut() {
        if let Err(e) = sink.send(env, payload) {
            failures.push(SinkFailure {
                sink: sink.name(),
                error: format!("{e:#}"),
            });
        }
    }
    failures
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

    use super::*;

    /// Records everything it receives.
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
            self.received.lock().unwrap().len()
        }
    }

    impl Sink for RecordingSink {
        fn name(&self) -> &'static str {
            self.name
        }
        fn send(&mut self, env: &Envelope, payload: &str) -> Result<()> {
            self.received.lock().unwrap().push(env.clone());
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
        fn send(&mut self, _env: &Envelope, _payload: &str) -> Result<()> {
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

        let failures = fan_out(&mut sinks, &env(), "{}");
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
        for f in fan_out(&mut sinks, &env(), "{}") {
            stats.count_sink_error(f.sink);
        }
        let snap = stats.snapshot();
        assert_eq!(snap.kafka_errors, 1);
        assert_eq!(snap.udp_errors, 1);
        assert_eq!(snap.jsonl_errors, 0);
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
        fan_out(&mut sinks, &e, &payload);
        assert_eq!(*a.payloads.lock().unwrap(), *b.payloads.lock().unwrap());
        assert_eq!(a.payloads.lock().unwrap()[0], payload);
    }
}
