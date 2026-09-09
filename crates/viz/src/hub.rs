//! The live fan-out hub: UDP datagrams in, WebSocket frames out.
//!
//! The browser does every derivation (cm, degrees, velocity), so the hub
//! forwards the envelope JSON **unchanged**. It only looks at the envelope tag
//! to decide whether a datagram is worth forwarding, to notice `session`
//! envelopes so a late-joining browser can be told the CPI / sens table / QPC
//! anchor before it sees its first batch, and to read `ts_anchor_us` off
//! batches for the bridge-latency estimator.
//!
//! That look is a *tag probe*, not a full [`Envelope`] parse: at 1kHz the
//! per-event `serde` work was the hub's whole CPU cost, and it bought nothing —
//! the page tolerates any shape, and forwarding is verbatim either way.
//!
//! The probe and the on-connect frame list are pure functions
//! ([`classify_datagram`], [`frames_on_connect`]) so they can be tested without
//! a socket.

use std::borrow::Cow;
use std::sync::Mutex;

use axum::extract::ws::Utf8Bytes;
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::stats::{Stats, now_utc_us};

/// A frame on its way to every connected browser. [`Utf8Bytes`] (a refcounted
/// `Bytes` view, and exactly what `axum` wants in a WS text message) rather
/// than `String` so both the broadcast channel's per-subscriber clone *and*
/// each client's send are refcount bumps instead of copies of the whole batch.
/// The payload is copied exactly once, at acceptance time.
pub type Frame = Utf8Bytes;

/// Why a datagram was not forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Zero-length (or whitespace-only) datagram.
    Empty,
    /// Payload was not valid UTF-8.
    NotUtf8,
    /// Valid UTF-8 but not a tagged `telemouse-core` envelope.
    BadEnvelope(String),
}

impl RejectReason {
    /// Stable index for rate-limiting one warning per distinct reason.
    pub fn kind_index(&self) -> usize {
        match self {
            RejectReason::Empty => 0,
            RejectReason::NotUtf8 => 1,
            RejectReason::BadEnvelope(_) => 2,
        }
    }

    /// Short, cardinality-free label for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            RejectReason::Empty => "empty",
            RejectReason::NotUtf8 => "not_utf8",
            RejectReason::BadEnvelope(_) => "bad_envelope",
        }
    }

    /// How many distinct reason kinds exist.
    pub const KINDS: usize = 3;
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::Empty => write!(f, "empty datagram"),
            RejectReason::NotUtf8 => write!(f, "not utf-8"),
            RejectReason::BadEnvelope(e) => write!(f, "bad envelope: {e}"),
        }
    }
}

/// The only fields the bridge itself needs off a datagram. Everything else
/// rides through untouched.
#[derive(Debug, Deserialize)]
struct TagProbe<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default)]
    ts_anchor_us: Option<i64>,
}

/// A datagram that passed validation and should be broadcast verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The original JSON text (trimmed of surrounding whitespace only),
    /// already in the shared [`Frame`] representation: the one payload copy
    /// per datagram happens here, and every later hop clones the handle.
    pub text: Frame,
    /// True for `{"type":"session"}` envelopes, which the hub caches.
    pub is_session: bool,
    /// `ts_anchor_us` of a `batch`, for the latency estimator. `None` on
    /// non-batches and on batches from a capture agent that omitted it.
    pub ts_anchor_us: Option<i64>,
}

/// Decide whether a raw UDP datagram should be broadcast to WebSocket clients.
///
/// Pure: no I/O, no state. The returned text is the caller's payload byte for
/// byte (modulo surrounding whitespace) — we never re-serialize, so any field
/// the viz crate does not know about still reaches the browser.
///
/// Validation is deliberately shallow: a recognised `type` tag on a JSON
/// object is enough. A batch with a field the bridge has never heard of, or
/// with a field missing, is the page's problem (it defaults everything) and
/// not a reason to drop telemetry on the floor.
pub fn classify_datagram(bytes: &[u8]) -> Result<Accepted, RejectReason> {
    let text = std::str::from_utf8(bytes).map_err(|_| RejectReason::NotUtf8)?;
    let text = text.trim();
    if text.is_empty() {
        return Err(RejectReason::Empty);
    }
    let probe: TagProbe =
        serde_json::from_str(text).map_err(|e| RejectReason::BadEnvelope(e.to_string()))?;
    let (is_session, ts_anchor_us) = match probe.kind.as_ref() {
        "session" => (true, None),
        "batch" => (false, probe.ts_anchor_us),
        "marker" => (false, None),
        other => {
            return Err(RejectReason::BadEnvelope(format!(
                "unknown envelope type {other:?}"
            )));
        }
    };
    Ok(Accepted {
        text: Frame::from(text),
        is_session,
        ts_anchor_us,
    })
}

/// Frames to push to a WebSocket client the instant it connects.
///
/// Currently just the most recent `session` envelope (if any) so a browser that
/// joins mid-session can convert counts to cm/degrees immediately instead of
/// waiting for the next session broadcast (which may never come).
pub fn frames_on_connect(cached_session: Option<&Frame>) -> Vec<Frame> {
    match cached_session {
        Some(s) => vec![s.clone()],
        None => Vec::new(),
    }
}

/// Broadcast channel capacity, in envelopes. At ~40 batches/s this is ~6s of
/// slack: enough to ride out a browser hiccup, small enough that a client
/// which is genuinely wedged is dropped before the hub is holding a minute of
/// stale telemetry for it.
pub const BROADCAST_CAPACITY: usize = 256;

pub struct Hub {
    tx: broadcast::Sender<Frame>,
    session: Mutex<Option<Frame>>,
    pub stats: Stats,
}

impl Hub {
    pub fn new() -> Self {
        Self::with_capacity(BROADCAST_CAPACITY)
    }

    pub fn with_capacity(cap: usize) -> Self {
        let (tx, _rx) = broadcast::channel(cap);
        Self {
            tx,
            session: Mutex::new(None),
            stats: Stats::default(),
        }
    }

    /// Ingest one datagram. Returns `Ok` if it was accepted (and therefore
    /// offered to the broadcast channel), `Err` with the reason otherwise.
    /// Never panics and never propagates errors upward — a malformed datagram
    /// is a counter, not an outage.
    pub fn publish(&self, bytes: &[u8]) -> Result<(), RejectReason> {
        self.publish_at(bytes, now_utc_us())
    }

    /// [`Hub::publish`] with the wall clock injected, so the latency estimator
    /// is testable without sleeping.
    pub fn publish_at(&self, bytes: &[u8], now_utc_us: i64) -> Result<(), RejectReason> {
        use std::sync::atomic::Ordering::Relaxed;
        self.stats.datagrams.fetch_add(1, Relaxed);
        self.stats.note_datagram(now_utc_us);
        match classify_datagram(bytes) {
            Ok(accepted) => {
                if let Some(anchor) = accepted.ts_anchor_us {
                    // End-to-end: capture stamped the batch's first event at
                    // `anchor`, we have it now. Includes batch assembly
                    // (~25ms window) plus the loopback hop.
                    self.stats.record_latency(now_utc_us - anchor);
                }
                // `accepted.text` already is the shared frame — caching and
                // broadcasting it are refcount bumps, not copies.
                if accepted.is_session {
                    *self.session.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some(accepted.text.clone());
                }
                // Err just means "no subscribers right now" — still counted as
                // forwarded work done; the session cache above is what matters
                // for a browser that connects later.
                let _ = self.tx.send(accepted.text);
                self.stats.forwarded.fetch_add(1, Relaxed);
                Ok(())
            }
            Err(reason) => {
                self.stats.parse_errors.fetch_add(1, Relaxed);
                Err(reason)
            }
        }
    }

    /// Push a frame the hub generated itself (the periodic `viz_stats` frame)
    /// to every connected client. Not counted as forwarded telemetry.
    pub fn broadcast(&self, frame: Frame) {
        let _ = self.tx.send(frame);
    }

    /// The cached `session` envelope JSON, if one has been seen.
    pub fn cached_session(&self) -> Option<Frame> {
        self.session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Subscribe a new client: the catch-up frames it should get first, plus a
    /// receiver for everything after. Taking the receiver *before* reading the
    /// cache would risk a duplicate session frame; taking it after would risk
    /// missing one, so we read the cache while holding the lock and subscribe
    /// inside it.
    pub fn subscribe(&self) -> (Vec<Frame>, broadcast::Receiver<Frame>) {
        // A poisoned lock (a client task panicked mid-publish) must not turn
        // every later reconnect into a 500: the cached value is still whole.
        let guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let rx = self.tx.subscribe();
        let frames = frames_on_connect(guard.as_ref());
        drop(guard);
        (frames, rx)
    }

    #[cfg(test)]
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use telemouse_core::wire::Envelope;
    use telemouse_core::{Batch, QpcAnchor, RawEvent, SessionConfig};

    pub fn session_json() -> String {
        let cfg = SessionConfig {
            session_id: "s-1".into(),
            started_utc_us: 1_756_000_000_000_000,
            qpc_freq: 10_000_000,
            anchor: QpcAnchor {
                qpc: 1_000,
                utc_us: 1_756_000_000_000_000,
                qpc_freq: 10_000_000,
            },
            anchor_uncertainty_us: Some(11),
            mouse_cpi: 1600.0,
            devices: vec!["mouse-0".into()],
            games: BTreeMap::new(),
            monitors: vec![],
            capture_version: "0.1.0".into(),
            coalesce_ms: 0,
        };
        Envelope::Session(cfg).to_json().unwrap()
    }

    pub fn batch_at(ts_anchor_us: i64) -> String {
        let b = Batch {
            session_id: "s-1".into(),
            seq_no: 1,
            ts_anchor_us,
            game: Some("cs2.exe".into()),
            pointer_locked: true,
            screen_w: 2560,
            screen_h: 1440,
            cursor_x: None,
            cursor_y: None,
            drops_since_last: 0,
            abs_frames_since_last: 0,
            events: vec![RawEvent {
                ts_qpc: 1_000,
                dx: 4,
                dy: -2,
                ..Default::default()
            }],
        };
        Envelope::Batch(b).to_json().unwrap()
    }

    pub fn batch_json() -> String {
        batch_at(1_756_000_000_000_000)
    }

    #[test]
    fn good_batch_datagram_is_accepted_verbatim() {
        let json = batch_json();
        let a = classify_datagram(json.as_bytes()).unwrap();
        assert_eq!(a.text, json);
        assert!(!a.is_session);
        assert_eq!(a.ts_anchor_us, Some(1_756_000_000_000_000));
    }

    #[test]
    fn session_datagram_is_flagged() {
        let a = classify_datagram(session_json().as_bytes()).unwrap();
        assert!(a.is_session);
        assert_eq!(a.ts_anchor_us, None, "only batches carry a latency anchor");
    }

    #[test]
    fn marker_datagram_is_accepted_without_an_anchor() {
        let json = r#"{"type":"marker","session_id":"s-1","seq_no":0,"ts_qpc":1,"ts_utc_us":2,"label":"flick"}"#;
        let a = classify_datagram(json.as_bytes()).unwrap();
        assert!(!a.is_session);
        assert_eq!(a.ts_anchor_us, None);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_but_body_untouched() {
        let json = batch_json();
        let padded = format!("\n  {json}\r\n");
        let a = classify_datagram(padded.as_bytes()).unwrap();
        assert_eq!(a.text, json);
    }

    #[test]
    fn unknown_fields_survive_forwarding() {
        // Forwarding is verbatim: a field this crate does not model must still
        // reach the browser untouched.
        let json = batch_json().replace(r#""seq_no":1"#, r#""seq_no":1,"future_field":42"#);
        let a = classify_datagram(json.as_bytes()).unwrap();
        assert!(a.text.contains("future_field"));
    }

    #[test]
    fn tag_probe_accepts_shapes_the_full_parser_would_reject() {
        // The bridge no longer type-checks payloads: a batch missing every
        // field but its tag is forwarded, because the page defaults absent
        // fields and dropping real telemetry over a schema drift is worse.
        assert!(Envelope::from_json(r#"{"type":"batch"}"#).is_err());
        let a = classify_datagram(br#"{"type":"batch"}"#).unwrap();
        assert!(!a.is_session);
        assert_eq!(a.ts_anchor_us, None);

        // A future wire revision that renames fields still flows through.
        let a = classify_datagram(br#"{"type":"batch","ts_anchor_us":7,"events":"?"}"#).unwrap();
        assert_eq!(a.ts_anchor_us, Some(7));
    }

    #[test]
    fn tag_probe_ignores_escaped_and_reordered_keys() {
        // Tag last rather than first.
        let a =
            classify_datagram(br#"{"session_id":"s","ts_anchor_us":5,"type":"batch"}"#).unwrap();
        assert!(!a.is_session);
        assert_eq!(a.ts_anchor_us, Some(5));

        // A JSON-escaped tag value forces the probe's Cow into its owned arm;
        // the comparison must still succeed.
        let escaped = format!("{{\"type\":\"sessio{}\"}}", "\\u006e");
        let a = classify_datagram(escaped.as_bytes()).unwrap();
        assert!(a.is_session, "escaped tag {escaped} should still classify");
    }

    #[test]
    fn bad_datagrams_are_rejected_with_a_reason() {
        assert_eq!(classify_datagram(b"").unwrap_err(), RejectReason::Empty);
        assert_eq!(
            classify_datagram(b"   \n ").unwrap_err(),
            RejectReason::Empty
        );
        assert_eq!(
            classify_datagram(&[0xff, 0xfe, 0x00]).unwrap_err(),
            RejectReason::NotUtf8
        );
        assert!(matches!(
            classify_datagram(b"not json at all").unwrap_err(),
            RejectReason::BadEnvelope(_)
        ));
        // Well-formed JSON, unrecognised tag.
        assert!(matches!(
            classify_datagram(br#"{"type":"bogus"}"#).unwrap_err(),
            RejectReason::BadEnvelope(_)
        ));
        // Object with no tag at all.
        assert!(matches!(
            classify_datagram(br#"{"session_id":"s-1"}"#).unwrap_err(),
            RejectReason::BadEnvelope(_)
        ));
        // Valid JSON that is not an object.
        assert!(matches!(
            classify_datagram(br#"[1,2,3]"#).unwrap_err(),
            RejectReason::BadEnvelope(_)
        ));
    }

    #[test]
    fn reject_reasons_have_distinct_stable_kinds() {
        let all = [
            RejectReason::Empty,
            RejectReason::NotUtf8,
            RejectReason::BadEnvelope("x".into()),
        ];
        let mut seen = vec![];
        for r in &all {
            assert!(r.kind_index() < RejectReason::KINDS);
            assert!(!seen.contains(&r.kind_index()));
            seen.push(r.kind_index());
            assert!(!r.kind().is_empty());
        }
        // The payload of BadEnvelope must not change its kind.
        assert_eq!(
            RejectReason::BadEnvelope("a".into()).kind_index(),
            RejectReason::BadEnvelope("b".into()).kind_index()
        );
    }

    #[test]
    fn publish_counts_good_and_bad_datagrams() {
        let hub = Hub::new();
        let mut rx = hub.tx.subscribe();

        assert!(hub.publish(batch_json().as_bytes()).is_ok());
        assert!(hub.publish(b"garbage").is_err());
        assert!(hub.publish(session_json().as_bytes()).is_ok());

        let s = hub.stats.snapshot();
        assert_eq!(s.datagrams, 3);
        assert_eq!(s.parse_errors, 1);
        assert_eq!(s.forwarded, 2);

        // Only the two valid envelopes reached subscribers, in order.
        assert_eq!(&*rx.try_recv().unwrap(), batch_json().as_str());
        assert_eq!(&*rx.try_recv().unwrap(), session_json().as_str());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn publish_feeds_the_latency_estimator_from_batches_only() {
        let hub = Hub::new();
        let anchor = 1_756_000_000_000_000;
        // Three batches seen 2ms, 4ms and 6ms after their first event.
        for delay in [2_000, 4_000, 6_000] {
            hub.publish_at(batch_at(anchor).as_bytes(), anchor + delay)
                .unwrap();
        }
        // A session envelope must not contribute a sample.
        hub.publish_at(session_json().as_bytes(), anchor + 999_000)
            .unwrap();

        let lat = hub.stats.latency.snapshot();
        assert_eq!(lat.samples, 3);
        assert_eq!(lat.max_us, 6_000);
        assert_eq!(lat.mean_us, 4_000);
        assert_eq!(lat.negative, 0);
    }

    #[test]
    fn clock_skew_shows_up_as_negative_samples() {
        let hub = Hub::new();
        let anchor = 1_756_000_000_000_000;
        hub.publish_at(batch_at(anchor).as_bytes(), anchor - 3_000)
            .unwrap();
        assert_eq!(hub.stats.latency.snapshot().negative, 1);
    }

    #[test]
    fn frames_on_connect_is_empty_without_a_session() {
        assert!(frames_on_connect(None).is_empty());
        let f: Frame = Frame::from_static("x");
        assert_eq!(frames_on_connect(Some(&f)), vec![f]);
    }

    #[test]
    fn new_client_receives_cached_session_first() {
        let hub = Hub::new();
        // Nothing cached yet.
        let (frames, _rx) = hub.subscribe();
        assert!(frames.is_empty());

        hub.publish(session_json().as_bytes()).unwrap();
        hub.publish(batch_json().as_bytes()).unwrap();

        // A client connecting now gets the session envelope up front, even
        // though the session envelope was broadcast before it subscribed.
        let (frames, mut rx) = hub.subscribe();
        assert_eq!(frames.len(), 1);
        assert_eq!(&*frames[0], session_json().as_str());
        assert!(rx.try_recv().is_err(), "no backlog for a fresh subscriber");

        // And it still sees subsequent live traffic.
        hub.publish(batch_json().as_bytes()).unwrap();
        assert_eq!(&*rx.try_recv().unwrap(), batch_json().as_str());
    }

    #[test]
    fn session_cache_keeps_the_most_recent_session() {
        let hub = Hub::new();
        hub.publish(session_json().as_bytes()).unwrap();
        let second = session_json().replace("s-1", "s-2");
        hub.publish(second.as_bytes()).unwrap();
        assert_eq!(hub.cached_session().as_deref(), Some(second.as_str()));
    }

    #[test]
    fn publish_without_subscribers_still_caches_session() {
        let hub = Hub::new();
        assert_eq!(hub.receiver_count(), 0);
        hub.publish(session_json().as_bytes()).unwrap();
        assert!(hub.cached_session().is_some());
        assert_eq!(hub.stats.snapshot().forwarded, 1);
    }

    #[test]
    fn hub_generated_frames_reach_clients_without_counting_as_telemetry() {
        let hub = Hub::new();
        let (_frames, mut rx) = hub.subscribe();
        hub.broadcast(Frame::from_static(r#"{"type":"viz_stats"}"#));
        assert_eq!(&*rx.try_recv().unwrap(), r#"{"type":"viz_stats"}"#);
        let s = hub.stats.snapshot();
        assert_eq!(s.datagrams, 0);
        assert_eq!(s.forwarded, 0);
    }

    #[test]
    fn slow_subscriber_lags_once_capacity_is_exceeded() {
        let hub = Hub::with_capacity(4);
        let (_frames, mut rx) = hub.subscribe();
        for _ in 0..10 {
            hub.publish(batch_json().as_bytes()).unwrap();
        }
        // A client that never drained sees Lagged, which the WS task treats as
        // "disconnect this client".
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }

    #[test]
    fn broadcast_capacity_is_a_few_seconds_not_a_minute() {
        // ~40 batches/s: keep the backlog bounded to single-digit seconds.
        const { assert!(BROADCAST_CAPACITY <= 256) };
        const { assert!(BROADCAST_CAPACITY >= 64) };
    }
}
