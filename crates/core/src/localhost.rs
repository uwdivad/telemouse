//! Browser-side guards for the local HTTP servers.
//!
//! Both `telemouse-viz` and `telemouse-ctl` are meant to be reached from a
//! browser on the same machine. Two attacks work against a localhost server
//! from a web page the user merely has open, and both are decided by request
//! headers the browser sets and a page cannot forge:
//!
//! * **DNS rebinding.** `evil.com` resolves to the attacker, serves a page,
//!   then re-resolves to `127.0.0.1`. The page's `fetch("/api/...")` now hits
//!   the local server *same-origin*, so custom-header CSRF guards no longer
//!   help. The tell is the `Host` header: it still says `evil.com`. A server
//!   that only answers to `Host`s naming the local machine is immune.
//! * **Cross-site WebSocket hijacking.** WebSocket upgrades are exempt from
//!   CORS, so any page can open `ws://127.0.0.1:7879/ws` and read the stream.
//!   The tell is `Origin`, which browsers always send on an upgrade.
//!
//! The rule for both is the same and is deliberately permissive about IP
//! literals: rebinding needs a *name* the attacker controls, so an IP-literal
//! `Host`/`Origin` is never a rebinding, and a user who binds viz to a LAN
//! address on purpose (an OBS overlay on a second PC) keeps working. Only
//! `localhost` and `*.localhost` (RFC 6761, always loopback in browsers) are
//! accepted as names.

use std::net::{Ipv4Addr, Ipv6Addr};

/// Whether a `Host` header value (`host` or `host:port`) names this machine
/// in a way an attacker's DNS name cannot: an IP literal, `localhost`, or a
/// `*.localhost` subdomain. Case-insensitive; a trailing dot is tolerated.
pub fn host_is_trusted(host: &str) -> bool {
    let Some(name) = split_host_port(host.trim()) else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    if let Some(v6) = name.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return v6.parse::<Ipv6Addr>().is_ok();
    }
    if name.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    let lower = name.to_ascii_lowercase();
    lower == "localhost" || lower.ends_with(".localhost")
}

/// Whether an `Origin` header value (`scheme://host[:port]`) is a trusted
/// browsing context per [`host_is_trusted`]. The opaque origin `null` and
/// non-http(s) schemes are rejected.
pub fn origin_is_trusted(origin: &str) -> bool {
    let origin = origin.trim();
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .or_else(|| origin.strip_prefix("ws://"))
        .or_else(|| origin.strip_prefix("wss://"));
    match rest {
        Some(host) if !host.contains('/') => host_is_trusted(host),
        _ => false,
    }
}

/// `host[:port]` → `host`, with bracketed IPv6 kept intact. `None` when the
/// port part is present but not a number.
fn split_host_port(s: &str) -> Option<&str> {
    if let Some(end) = s.strip_prefix('[').and_then(|_| s.find(']')) {
        let (name, port) = s.split_at(end + 1);
        return match port.strip_prefix(':') {
            None if port.is_empty() => Some(name),
            Some(p) if p.parse::<u16>().is_ok() => Some(name),
            _ => None,
        };
    }
    match s.rsplit_once(':') {
        Some((name, port)) => port.parse::<u16>().ok().map(|_| name),
        None => Some(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts_are_trusted() {
        for h in [
            "127.0.0.1",
            "127.0.0.1:7879",
            "10.0.0.5:7879",
            "[::1]",
            "[::1]:7880",
            "localhost",
            "LOCALHOST:7880",
            "localhost.",
            "viz.localhost:7879",
        ] {
            assert!(host_is_trusted(h), "{h}");
        }
    }

    #[test]
    fn dns_names_are_not() {
        for h in [
            "evil.com",
            "evil.com:7879",
            "localhost.evil.com",
            "notlocalhost",
            "127.0.0.1.evil.com",
            "",
            ":7879",
            "[::1",
            "localhost:notaport",
            "[::1]x",
        ] {
            assert!(!host_is_trusted(h), "{h}");
        }
    }

    #[test]
    fn origins_follow_the_host_rule() {
        assert!(origin_is_trusted("http://127.0.0.1:7879"));
        assert!(origin_is_trusted("http://localhost"));
        assert!(origin_is_trusted("https://[::1]:7879"));
        assert!(!origin_is_trusted("http://evil.com"));
        assert!(!origin_is_trusted("null"));
        assert!(!origin_is_trusted("file://"));
        assert!(!origin_is_trusted("http://127.0.0.1:7879/path"));
        assert!(!origin_is_trusted(""));
    }
}
