//! The two local servers, as seen from a tool call.
//!
//! `telemouse-ctl` and `telemouse-viz` are the only things this server talks
//! to, and neither is required to be running: a panel that is not up is a
//! tool result that says so, never a panic and never a dead MCP session. So
//! every call comes back as a [`LocalError`] whose `Display` is already the
//! sentence to hand the model — which server, on which address, and what to
//! do about it.
//!
//! Control calls go through ctl's HTTP API, with its guard header, exactly as
//! the panel page does. This server never spawns or terminates anything
//! itself: ctl's allow-lists (which flags a component accepts, which pids its
//! scan would list) are the safety model, and proxying keeps them in force.

use std::net::SocketAddr;

use serde_json::Value;

use crate::http::{self, HttpError, Request};

/// The guard header ctl requires on every mutating call
/// (`docs/API.md`, "Two rules every HTTP client must follow").
const GUARD: (&str, &str) = ("X-Telemouse-Ctl", "1");

/// One of the two servers, with the words to use when it is not there.
#[derive(Debug, Clone)]
pub struct Local {
    addr: SocketAddr,
    /// What to call it in a message a model reads.
    label: &'static str,
    /// How the user starts it.
    hint: &'static str,
    /// Send the ctl guard header on mutating calls.
    guard: bool,
}

/// Why a call to ctl or viz did not produce a JSON answer.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    /// Nothing answered: the server is not running, or not where the config
    /// says it is.
    #[error("{label} is not answering on {addr}: {source}. {hint}")]
    Unreachable {
        label: &'static str,
        addr: SocketAddr,
        hint: &'static str,
        #[source]
        source: HttpError,
    },
    /// It answered, and refused. ctl's own explanation is in `message`.
    #[error("{label} refused the request ({status}): {message}")]
    Refused {
        label: &'static str,
        status: u16,
        message: String,
    },
    /// It answered with something that is not the JSON we expected.
    #[error("{label} answered {status} with a body this tool cannot read: {detail}")]
    Unreadable {
        label: &'static str,
        status: u16,
        detail: String,
    },
}

impl Local {
    /// The control panel: `[ctl] http_addr`.
    pub fn ctl(addr: SocketAddr) -> Self {
        Self {
            addr,
            label: "the telemouse control panel (telemouse-ctl)",
            hint: "Start it (telemouse-ctl.exe) or check [ctl] http_addr in telemouse.toml.",
            guard: true,
        }
    }

    /// The viz server: `[viz] http_addr`.
    pub fn viz(addr: SocketAddr) -> Self {
        Self {
            addr,
            label: "the telemouse viz server (telemouse-viz)",
            hint: "Start it from the control panel, or check [viz] http_addr in telemouse.toml.",
            guard: false,
        }
    }

    /// `GET path` → the parsed JSON body.
    pub async fn get(&self, path: &str) -> Result<Value, LocalError> {
        self.get_accepting(path, &[]).await
    }

    /// `GET path`, treating the statuses in `accept` as answers rather than
    /// refusals. Viz reports "the UDP listener is not bound" as a `503` with
    /// a JSON body that says so, and that body is exactly what `health`
    /// wants to show.
    pub async fn get_accepting(&self, path: &str, accept: &[u16]) -> Result<Value, LocalError> {
        self.call(
            Request {
                method: "GET",
                path,
                body: None,
                headers: &[],
            },
            accept,
        )
        .await
    }

    /// `POST path` with a JSON body → the parsed JSON answer. The guard
    /// header is added for ctl; viz has no mutating routes.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, LocalError> {
        let body = body.to_string();
        let headers: &[(&str, &str)] = if self.guard { &[GUARD] } else { &[] };
        self.call(
            Request {
                method: "POST",
                path,
                body: Some(&body),
                headers,
            },
            &[],
        )
        .await
    }

    async fn call(&self, req: Request<'_>, accept: &[u16]) -> Result<Value, LocalError> {
        let res = http::send(self.addr, req)
            .await
            .map_err(|source| LocalError::Unreachable {
                label: self.label,
                addr: self.addr,
                hint: self.hint,
                source,
            })?;
        interpret(self.label, res.status, &res.body, accept)
    }
}

/// Turn a status and a body into either JSON or the sentence to show.
///
/// Pulled out of [`Local::call`] because this is the mapping worth testing:
/// ctl answers its refusals as `{"error": "..."}`, viz answers `/healthz`
/// with a JSON body on a 503, and a route that does not exist in a minimal
/// build answers with plain text.
pub fn interpret(
    label: &'static str,
    status: u16,
    body: &str,
    accept: &[u16],
) -> Result<Value, LocalError> {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    if (200..300).contains(&status) || accept.contains(&status) {
        return match parsed {
            Some(v) => Ok(v),
            None => Err(LocalError::Unreadable {
                label,
                status,
                detail: snippet(body),
            }),
        };
    }
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| snippet(body));
    Err(LocalError::Refused {
        label,
        status,
        message,
    })
}

/// A body, cut to something a tool result can carry.
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(empty body)".to_string();
    }
    let mut out: String = trimmed.chars().take(300).collect();
    if out.len() < trimmed.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: &str = "the telemouse control panel (telemouse-ctl)";

    #[test]
    fn a_json_success_is_the_value() {
        let v = interpret(L, 200, r#"{"ok":true,"pid":1234}"#, &[]).unwrap();
        assert_eq!(v["pid"], 1234);
    }

    #[test]
    fn ctls_error_field_becomes_the_message() {
        let e = interpret(L, 409, r#"{"error":"capture is already running"}"#, &[]).unwrap_err();
        assert!(matches!(e, LocalError::Refused { status: 409, .. }));
        let text = e.to_string();
        assert!(text.contains("409"), "{text}");
        assert!(text.contains("capture is already running"), "{text}");
    }

    #[test]
    fn a_non_json_refusal_still_says_something_useful() {
        let e = interpret(L, 403, "forbidden", &[]).unwrap_err();
        assert!(e.to_string().contains("forbidden"));
    }

    #[test]
    fn an_empty_refusal_body_does_not_produce_an_empty_message() {
        let e = interpret(L, 500, "   ", &[]).unwrap_err();
        assert!(e.to_string().contains("(empty body)"));
    }

    #[test]
    fn a_page_where_json_was_expected_is_unreadable_not_success() {
        let e = interpret(L, 200, "<!doctype html>", &[]).unwrap_err();
        assert!(matches!(e, LocalError::Unreadable { .. }));
        assert!(e.to_string().contains("<!doctype html>"));
    }

    #[test]
    fn a_long_body_is_cut() {
        let long = "x".repeat(5000);
        let e = interpret(L, 500, &long, &[]).unwrap_err();
        assert!(e.to_string().len() < 500, "the message must stay small");
        assert!(e.to_string().ends_with('…'));
    }

    /// Viz says "not ready" with a `503` and a JSON body; `health` wants
    /// that body, not a refusal.
    #[test]
    fn an_accepted_status_yields_its_body_instead_of_an_error() {
        let body = r#"{"ok":false,"udp_bound":false,"uptime_s":2.0}"#;
        assert!(interpret(L, 503, body, &[]).is_err());
        let v = interpret(L, 503, body, &[503]).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["udp_bound"], false);
    }

    /// The degraded case the task cares about most: the panel is not up, and
    /// the tool result has to say which panel, where, and what to do.
    #[tokio::test]
    async fn an_unreachable_panel_names_itself_its_address_and_the_fix() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        drop(listener);
        let e = Local::ctl(at).get("/api/state").await.unwrap_err();
        let text = e.to_string();
        assert!(text.contains("telemouse-ctl"), "{text}");
        assert!(text.contains(&at.to_string()), "{text}");
        assert!(text.contains("[ctl] http_addr"), "{text}");
        assert!(matches!(e, LocalError::Unreachable { .. }));
    }

    #[tokio::test]
    async fn an_unreachable_viz_points_at_its_own_config_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        drop(listener);
        let e = Local::viz(at).get("/api/stats").await.unwrap_err();
        assert!(e.to_string().contains("[viz] http_addr"), "{e}");
    }
}
