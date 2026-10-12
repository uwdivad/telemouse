//! The UDP ingest side of the bridge.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tracing::{info, warn};

use telemouse_core::wire::MAX_UDP_PAYLOAD;

use crate::hub::{Hub, RejectReason};

/// Receive buffer: comfortably above `telemouse-core`'s datagram cap so a
/// full batch never truncates.
const RECV_BUF: usize = MAX_UDP_PAYLOAD + 8 * 1024;

/// How often a *repeating* reject reason may be logged again.
const REJECT_WARN_EVERY: Duration = Duration::from_secs(60);

/// Bytes of a rejected payload to show in the log line.
const PREFIX_BYTES: usize = 96;

/// Rate limiter for reject warnings: the first datagram of each distinct
/// reason is loud, and after that one per reason per minute.
///
/// A silently-incrementing `parse_errors` counter tells you *that* the capture
/// agent and the bridge disagree but never *how*; a bounded warn with the byte
/// count and payload prefix usually tells you in one line.
#[derive(Debug, Default)]
pub struct RejectLog {
    last: [Option<Instant>; RejectReason::KINDS],
}

impl RejectLog {
    /// True if this reason should be logged at `now`.
    pub fn should_warn(&mut self, reason: &RejectReason, now: Instant) -> bool {
        let slot = &mut self.last[reason.kind_index()];
        match *slot {
            Some(prev) if now.duration_since(prev) < REJECT_WARN_EVERY => false,
            _ => {
                *slot = Some(now);
                true
            }
        }
    }
}

/// A short, log-safe rendering of a rejected payload: printable ASCII kept,
/// everything else escaped, truncated to [`PREFIX_BYTES`].
pub fn payload_prefix(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(PREFIX_BYTES + 8);
    for &b in bytes.iter().take(PREFIX_BYTES) {
        match b {
            0x20..=0x7e => out.push(b as char),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    if bytes.len() > PREFIX_BYTES {
        out.push('…');
    }
    out
}

/// Listen for capture-agent envelopes forever.
///
/// A datagram that fails to parse is counted, and logged at `warn` the first
/// time (and once a minute after that, per reason); the loop keeps running. A
/// bind failure is returned so the caller can warn and continue serving HTTP
/// (replay still works without a live feed).
pub async fn listen(addr: SocketAddr, hub: Arc<Hub>) -> std::io::Result<()> {
    let sock = UdpSocket::bind(addr).await?;
    hub.stats.set_udp_bound(true);
    info!(%addr, "udp listener bound");
    let mut buf = vec![0u8; RECV_BUF];
    let mut rejects = RejectLog::default();
    let mut recv_errors = RecvErrorLog::default();
    loop {
        match sock.recv_from(&mut buf).await {
            Ok((n, from)) => {
                if let Err(reason) = hub.publish(&buf[..n])
                    && rejects.should_warn(&reason, Instant::now())
                {
                    warn!(
                        %from,
                        bytes = n,
                        kind = reason.kind(),
                        %reason,
                        payload = %payload_prefix(&buf[..n]),
                        "rejected datagram (further ones of this kind are counted, \
                         and logged at most once a minute)"
                    );
                }
            }
            Err(e) => {
                // On Windows a UDP send to a closed port can surface as
                // ConnectionReset on the *receiving* socket. Never fatal, but
                // a persistent error would spin this loop: warn at most once
                // a minute and back off briefly before the next receive.
                if recv_errors.should_warn(Instant::now()) {
                    warn!(
                        error = %e,
                        "udp recv error (further ones are logged at most once a minute)"
                    );
                }
                tokio::time::sleep(RECV_ERROR_BACKOFF).await;
            }
        }
    }
}

/// Pause after a failed `recv_from`, so a socket stuck in an error state
/// cannot turn the receive loop into a busy loop.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Once-a-minute gate for receive errors (the same cadence as [`RejectLog`]).
#[derive(Debug, Default)]
pub struct RecvErrorLog {
    last: Option<Instant>,
}

impl RecvErrorLog {
    /// True if a receive error at `now` should be logged.
    pub fn should_warn(&mut self, now: Instant) -> bool {
        match self.last {
            Some(prev) if now.duration_since(prev) < REJECT_WARN_EVERY => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_reject_of_each_kind_warns_then_backs_off() {
        let mut log = RejectLog::default();
        let t0 = Instant::now();

        assert!(log.should_warn(&RejectReason::Empty, t0), "first is loud");
        assert!(!log.should_warn(&RejectReason::Empty, t0), "second is not");
        assert!(
            !log.should_warn(&RejectReason::Empty, t0 + Duration::from_secs(59)),
            "still inside the window"
        );
        assert!(
            log.should_warn(&RejectReason::Empty, t0 + Duration::from_secs(61)),
            "one per minute after that"
        );
    }

    #[test]
    fn reject_kinds_are_rate_limited_independently() {
        let mut log = RejectLog::default();
        let t0 = Instant::now();
        assert!(log.should_warn(&RejectReason::Empty, t0));
        assert!(log.should_warn(&RejectReason::NotUtf8, t0));
        assert!(log.should_warn(&RejectReason::BadEnvelope("x".into()), t0));
        // ...and each is now quiet on its own.
        assert!(!log.should_warn(&RejectReason::Empty, t0));
        assert!(!log.should_warn(&RejectReason::NotUtf8, t0));
        assert!(!log.should_warn(&RejectReason::BadEnvelope("y".into()), t0));
    }

    #[test]
    fn recv_errors_warn_once_a_minute() {
        let mut log = RecvErrorLog::default();
        let t0 = Instant::now();
        assert!(log.should_warn(t0));
        assert!(!log.should_warn(t0 + Duration::from_secs(1)));
        assert!(!log.should_warn(t0 + Duration::from_secs(59)));
        assert!(log.should_warn(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn payload_prefix_is_printable_bounded_and_truncated() {
        assert_eq!(payload_prefix(br#"{"type":"x"}"#), r#"{"type":"x"}"#);
        assert_eq!(payload_prefix(b"a\nb\tc\r"), "a\\nb\\tc\\r");
        assert_eq!(payload_prefix(&[0x00, 0xff]), "\\x00\\xff");

        let long = vec![b'a'; PREFIX_BYTES * 3];
        let p = payload_prefix(&long);
        assert!(p.starts_with("aaaa"));
        assert!(p.ends_with('…'));
        assert_eq!(p.chars().filter(|c| *c == 'a').count(), PREFIX_BYTES);

        // Never emits a control character that could corrupt a terminal.
        let all_bytes: Vec<u8> = (0u8..=255).collect();
        assert!(
            payload_prefix(&all_bytes).chars().all(|c| !c.is_control()),
            "log line must stay printable"
        );
    }
}
