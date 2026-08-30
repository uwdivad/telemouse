//! Localhost UDP fan-out for the live viz: one JSON envelope per datagram.

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use telemouse_core::wire::MAX_UDP_PAYLOAD;

use super::Sink;
use crate::stats::Stats;

pub struct UdpSink {
    socket: UdpSocket,
    addr: String,
    stats: Arc<Stats>,
}

impl UdpSink {
    /// Bind an ephemeral local port and connect it to `addr`. Connecting a UDP
    /// socket costs nothing but lets us `send` without re-resolving per packet.
    pub fn connect(addr: &str, stats: Arc<Stats>) -> Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0").context("bind udp socket")?;
        socket
            .connect(addr)
            .with_context(|| format!("connect udp socket to {addr}"))?;
        // Never let a full socket buffer stall the shipping thread.
        socket
            .set_nonblocking(true)
            .context("set udp nonblocking")?;
        Ok(Self {
            socket,
            addr: addr.to_string(),
            stats,
        })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }
}

/// How a failed send should be accounted for. Pure, so the "no viz running is
/// not an error" rule is testable without a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The local socket buffer is full. Best-effort path: drop it silently.
    WouldBlock,
    /// Nothing is listening. On a *connected* UDP socket Windows surfaces the
    /// ICMP port-unreachable from a previous datagram as `WSAECONNRESET` on a
    /// later send — with no viz up that is every single batch, so it is a
    /// counter, never a warning.
    Unreachable,
    Failed,
}

pub fn classify(kind: ErrorKind) -> SendOutcome {
    match kind {
        ErrorKind::WouldBlock => SendOutcome::WouldBlock,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused => SendOutcome::Unreachable,
        _ => SendOutcome::Failed,
    }
}

impl Sink for UdpSink {
    fn name(&self) -> &'static str {
        "udp"
    }

    fn send(&mut self, _topic: &'static str, _key: &str, payload: &str) -> Result<()> {
        if payload.len() > MAX_UDP_PAYLOAD {
            self.stats.udp_oversized.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!(
                "envelope of {} bytes exceeds the {MAX_UDP_PAYLOAD}-byte datagram budget",
                payload.len()
            );
        }
        match self.socket.send(payload.as_bytes()) {
            Ok(_) => Ok(()),
            Err(e) => match classify(e.kind()) {
                SendOutcome::WouldBlock => Ok(()),
                SendOutcome::Unreachable => {
                    self.stats.udp_unreachable.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                SendOutcome::Failed => Err(e).context("udp send"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use telemouse_core::Envelope;

    use super::*;

    fn marker() -> Envelope {
        Envelope::Marker(telemouse_core::Marker {
            session_id: "s-1".into(),
            seq_no: 0,
            ts_qpc: 1,
            ts_utc_us: 2,
            label: "hotkey".into(),
        })
    }

    #[test]
    fn connect_to_a_local_port_and_send() {
        // No listener needed: connected UDP on loopback still accepts sends.
        let stats = Arc::new(Stats::default());
        let mut sink = UdpSink::connect("127.0.0.1:59999", Arc::clone(&stats)).unwrap();
        assert_eq!(sink.name(), "udp");
        let env = marker();
        let payload = env.to_json().unwrap();
        // With nothing listening, Windows may report the earlier datagram's
        // ICMP unreachable on a later send. Either way this is never an error.
        for _ in 0..5 {
            sink.send(env.topic(), env.key(), &payload).unwrap();
        }
        assert_eq!(stats.snapshot().udp_errors, 0);
    }

    #[test]
    fn bad_address_fails_at_construction_not_at_send() {
        assert!(UdpSink::connect("not-an-address", Arc::new(Stats::default())).is_err());
    }

    #[test]
    fn unreachable_is_silent_success_and_a_full_buffer_is_a_drop() {
        assert_eq!(
            classify(ErrorKind::ConnectionReset),
            SendOutcome::Unreachable
        );
        assert_eq!(
            classify(ErrorKind::ConnectionRefused),
            SendOutcome::Unreachable
        );
        assert_eq!(classify(ErrorKind::WouldBlock), SendOutcome::WouldBlock);
        assert_eq!(classify(ErrorKind::PermissionDenied), SendOutcome::Failed);
    }

    #[test]
    fn oversized_payloads_are_counted_and_rejected() {
        let stats = Arc::new(Stats::default());
        let mut sink = UdpSink::connect("127.0.0.1:59998", Arc::clone(&stats)).unwrap();
        let huge = "x".repeat(MAX_UDP_PAYLOAD + 1);
        let env = marker();
        assert!(sink.send(env.topic(), env.key(), &huge).is_err());
        assert_eq!(stats.snapshot().udp_oversized, 1);
    }
}
