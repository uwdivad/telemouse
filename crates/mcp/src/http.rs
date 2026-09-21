//! One HTTP/1.1 request over one TCP connection, and nothing else.
//!
//! This server talks to exactly two peers — `telemouse-ctl` and
//! `telemouse-viz`, both on loopback, both plain HTTP, both answering small
//! JSON bodies. An HTTP client crate would bring a TLS stack, a connection
//! pool, a DNS resolver and a redirect policy for a job that is a socket, a
//! request line and a response to parse, so the request is written by hand
//! the way the process table is read by hand in `crates/ctl/src/procs.rs`.
//!
//! Every request says `Connection: close`, so there is nothing to pool and
//! the server closes when it is done; the response is read to EOF and framed
//! by `Content-Length` or `Transfer-Encoding: chunked` when either is
//! present. `Host` is the socket address, which is an IP literal and
//! therefore passes the trust rule in `telemouse_core::localhost`.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long a whole request may take. The peers are on this machine and
/// answer in microseconds; a wait longer than this means something is stuck,
/// and a stuck peer must not hang the tool call that asked about it.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Most a response body may be. `/api/state` with 120 log lines per
/// component is a few hundred kilobytes at worst; this only exists so a
/// misdirected request at something that streams cannot fill memory.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// What came back: the status and the body, both of which the caller needs
/// (ctl says *why* it refused in the body of a 400 or a 409).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Everything that can go wrong below the application layer. The messages
/// are written for a model reading a tool result, so they name the address.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("nothing is listening on {addr} ({source})")]
    Connect {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("{addr} did not answer within {}s", TIMEOUT.as_secs())]
    Timeout { addr: SocketAddr },
    #[error("the connection to {addr} failed: {source}")]
    Io {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("{addr} sent a response this client cannot read: {reason}")]
    Protocol { addr: SocketAddr, reason: String },
}

/// One request. `body` is JSON when present; `headers` carries the ctl guard
/// header and nothing else so far.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: Option<&'a str>,
    pub headers: &'a [(&'a str, &'a str)],
}

/// Serialise `req` into the bytes to put on the wire.
///
/// Split out from [`send`] so the framing is testable without a socket:
/// the `Host` rule and the guard header are security-relevant, and a typo in
/// either is a 403 at runtime rather than a compile error.
pub fn encode(addr: SocketAddr, req: &Request<'_>) -> Vec<u8> {
    let mut head = String::with_capacity(256);
    head.push_str(req.method);
    head.push(' ');
    head.push_str(req.path);
    head.push_str(" HTTP/1.1\r\nHost: ");
    // An IP literal with a port: `127.0.0.1:7880`, `[::1]:7880`. Both are
    // trusted by `telemouse_core::localhost::host_is_trusted`.
    head.push_str(&addr.to_string());
    head.push_str("\r\nUser-Agent: telemouse-mcp/");
    head.push_str(env!("CARGO_PKG_VERSION"));
    head.push_str("\r\nAccept: application/json\r\nConnection: close\r\n");
    for (name, value) in req.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if let Some(body) = req.body {
        head.push_str("Content-Type: application/json\r\nContent-Length: ");
        head.push_str(&body.len().to_string());
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    if let Some(body) = req.body {
        out.extend_from_slice(body.as_bytes());
    }
    out
}

/// Send `req` to `addr` and read the whole response.
pub async fn send(addr: SocketAddr, req: Request<'_>) -> Result<Response, HttpError> {
    let raw = encode(addr, &req);
    let work = async {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(|source| HttpError::Connect { addr, source })?;
        // Requests are one datagram's worth; the extra round trip Nagle
        // would add is pure latency on a loopback tool call.
        let _ = stream.set_nodelay(true);
        stream
            .write_all(&raw)
            .await
            .map_err(|source| HttpError::Io { addr, source })?;
        stream
            .flush()
            .await
            .map_err(|source| HttpError::Io { addr, source })?;
        let mut buf = Vec::with_capacity(8 * 1024);
        // `take` bounds the read; a peer that keeps sending is cut off
        // rather than allowed to grow this process without limit.
        let mut reader = (&mut stream).take(MAX_BODY as u64 + 64 * 1024);
        reader
            .read_to_end(&mut buf)
            .await
            .map_err(|source| HttpError::Io { addr, source })?;
        Ok(buf)
    };
    let buf = match tokio::time::timeout(TIMEOUT, work).await {
        Ok(r) => r?,
        Err(_) => return Err(HttpError::Timeout { addr }),
    };
    parse(addr, &buf)
}

/// Split a raw response into its status and its body.
///
/// Separate from the socket so every framing case — `Content-Length`,
/// `chunked`, and close-delimited — is covered by a test.
pub fn parse(addr: SocketAddr, raw: &[u8]) -> Result<Response, HttpError> {
    let bad = |reason: &str| HttpError::Protocol {
        addr,
        reason: reason.to_string(),
    };
    let split = find(raw, b"\r\n\r\n").ok_or_else(|| bad("no end of headers"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| bad("non-UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or_else(|| bad("empty response"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("no status code in the first line"))?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }

    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest).ok_or_else(|| bad("malformed chunked body"))?
    } else if let Some(n) = content_length {
        if rest.len() < n {
            return Err(bad("the body is shorter than Content-Length says"));
        }
        rest[..n].to_vec()
    } else {
        // No framing header: `Connection: close` makes EOF the terminator.
        rest.to_vec()
    };
    if body.len() > MAX_BODY {
        return Err(bad("the response body is larger than this client accepts"));
    }
    Ok(Response {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// First index of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .filter(|_| !needle.is_empty())
}

/// Reassemble a `Transfer-Encoding: chunked` body, or `None` if it is
/// malformed or truncated.
fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(rest.len());
    loop {
        let eol = find(rest, b"\r\n")?;
        let header = std::str::from_utf8(&rest[..eol]).ok()?;
        // A chunk-size line may carry `;ext=value` after the size.
        let size_hex = header.split(';').next()?.trim();
        let size = usize::from_str_radix(size_hex, 16).ok()?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size + 2 || out.len() + size > MAX_BODY {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:7880".parse().unwrap()
    }

    #[test]
    fn the_request_carries_a_trusted_host_and_the_guard_header() {
        let raw = encode(
            addr(),
            &Request {
                method: "POST",
                path: "/api/components/capture/start",
                body: Some("{\"save\":true}"),
                headers: &[("X-Telemouse-Ctl", "1")],
            },
        );
        let text = String::from_utf8(raw).unwrap();
        assert!(text.starts_with("POST /api/components/capture/start HTTP/1.1\r\n"));
        assert!(text.contains("\r\nHost: 127.0.0.1:7880\r\n"));
        assert!(
            telemouse_core::localhost::host_is_trusted("127.0.0.1:7880"),
            "the Host we send must be one ctl accepts"
        );
        assert!(text.contains("\r\nX-Telemouse-Ctl: 1\r\n"));
        assert!(text.contains("\r\nContent-Length: 13\r\n"));
        assert!(text.ends_with("\r\n\r\n{\"save\":true}"));
    }

    #[test]
    fn a_get_has_no_content_length() {
        let raw = encode(
            addr(),
            &Request {
                method: "GET",
                path: "/api/state",
                body: None,
                headers: &[],
            },
        );
        let text = String::from_utf8(raw).unwrap();
        assert!(!text.contains("Content-Length"));
        assert!(text.contains("\r\nConnection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn an_ipv6_host_is_bracketed() {
        let raw = encode(
            "[::1]:7879".parse().unwrap(),
            &Request {
                method: "GET",
                path: "/healthz",
                body: None,
                headers: &[],
            },
        );
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("\r\nHost: [::1]:7879\r\n"), "{text}");
    }

    #[test]
    fn content_length_frames_the_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}trailing";
        let r = parse(addr(), raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{}");
    }

    #[test]
    fn a_close_delimited_body_runs_to_the_end() {
        let raw = b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\nnot ready";
        let r = parse(addr(), raw).unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(r.body, "not ready");
    }

    #[test]
    fn a_chunked_body_is_reassembled() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4\r\n\":1}\r\n0\r\n\r\n";
        let r = parse(addr(), raw).unwrap();
        assert_eq!(r.body, "{\"a\":1}");
    }

    #[test]
    fn a_truncated_response_is_a_protocol_error_not_a_panic() {
        for raw in [
            &b"HTTP/1.1 200 OK\r\n"[..],
            &b"HTTP/1.1\r\n\r\n"[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\nshort"[..],
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nno"[..],
            &b""[..],
        ] {
            assert!(
                matches!(parse(addr(), raw), Err(HttpError::Protocol { .. })),
                "{:?} should be a protocol error",
                String::from_utf8_lossy(raw)
            );
        }
    }

    /// The whole point of the module: a real socket, a real request, a real
    /// response — with the peer played by a listener this test owns.
    #[tokio::test]
    async fn a_round_trip_over_a_real_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            // Read until the request is complete (the client sends a body,
            // so the headers alone are not the end of it).
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
                if n == 0 || seen.ends_with(b"}") {
                    break;
                }
            }
            sock.write_all(
                b"HTTP/1.1 409 Conflict\r\nContent-Length: 30\r\n\r\n{\"error\":\"already running\"}\n\n\n",
            )
            .await
            .unwrap();
            sock.shutdown().await.unwrap();
            String::from_utf8(seen).unwrap()
        });

        let r = send(
            at,
            Request {
                method: "POST",
                path: "/api/components/capture/start",
                body: Some("{}"),
                headers: &[("X-Telemouse-Ctl", "1")],
            },
        )
        .await
        .unwrap();
        assert_eq!(r.status, 409);
        assert!(r.body.starts_with("{\"error\":\"already running\"}"));
        let request = server.await.unwrap();
        assert!(request.contains("X-Telemouse-Ctl: 1"));
        assert!(request.ends_with("\r\n\r\n{}"));
    }

    #[tokio::test]
    async fn a_closed_port_is_a_connect_error() {
        // Bind to learn a port nothing will be listening on, then drop it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        drop(listener);
        let e = send(
            at,
            Request {
                method: "GET",
                path: "/healthz",
                body: None,
                headers: &[],
            },
        )
        .await
        .expect_err("nothing is listening");
        assert!(matches!(e, HttpError::Connect { .. }), "{e}");
        assert!(e.to_string().contains(&at.to_string()));
    }
}
