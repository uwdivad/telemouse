//! HTTP + WebSocket surface: the embedded page, the replay REST endpoints, and
//! the `/ws` live fan-out.
//!
//! There is no authentication: this is a local tool. What it does defend
//! against is the browser itself being used against it — a web page the user
//! happens to have open reading the live stream or the recordings. Every
//! request must carry a `Host` naming this machine (so a DNS-rebound name is
//! refused), and a WebSocket upgrade must come from a local `Origin` or from
//! no browser at all. See [`telemouse_core::localhost`].

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use telemouse_core::config::ObsConfig;
use telemouse_core::localhost::{host_is_trusted, origin_is_trusted};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::hub::Hub;
use crate::recordings;
use crate::stats::StatsPayload;

/// The single-file browser app. No external assets, no CDN — everything the
/// page needs is inlined so the viz works on a machine with no internet.
pub const INDEX_HTML: &str = include_str!("index.html");

/// Where the server splices its config into the page. Sits inside a
/// `<script>` object literal, so the raw file (placeholder intact) is still
/// valid JavaScript and an unconfigured page just sees `{}`.
const CONFIG_PLACEHOLDER: &str = "/*__TELEMOUSE_CONFIG__*/";

/// The two flavours of the page, rendered once at startup: the dashboard and
/// the OBS browser-source variant. Same HTML, different injected config.
pub struct Pages {
    pub index: String,
    pub obs: String,
}

impl Pages {
    pub fn render(obs: &ObsConfig) -> Self {
        Self {
            index: inject_config(INDEX_HTML, obs, false),
            obs: inject_config(INDEX_HTML, obs, true),
        }
    }
}

/// Splice `{ obs_route, obs }` into the page. `<` is escaped so a config
/// string can never close the `<script>` element it is embedded in.
fn inject_config(html: &str, obs: &ObsConfig, obs_route: bool) -> String {
    let cfg = serde_json::json!({ "obs_route": obs_route, "obs": obs });
    let mut json = cfg.to_string();
    // Object literal body: strip the outer braces and drop it into `{...}`.
    json = json[1..json.len() - 1].replace('<', "\\u003c");
    html.replace(CONFIG_PLACEHOLDER, &json)
}

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub recordings_dir: PathBuf,
    pub pages: Arc<Pages>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/obs", get(obs_page))
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_upgrade))
        .route("/api/stats", get(api_stats))
        .route("/api/sessions", get(api_sessions))
        .route("/api/session/{id}", get(api_session))
        .layer(middleware::from_fn(require_local_host))
        .with_state(state)
}

/// Refuse any request whose `Host` is a DNS name other than `localhost`: that
/// is what a DNS-rebinding page looks like from here. IP literals always pass
/// so a deliberate LAN bind keeps working.
async fn require_local_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_is_trusted(host) {
        warn!(host, path = %req.uri().path(), "refusing request with a non-local Host header");
        return (StatusCode::FORBIDDEN, "host not allowed").into_response();
    }
    next.run(req).await
}

/// The bridge's own health, in the same shape the page receives once a second
/// over the WebSocket (`{"type":"viz_stats",...}`) — so a human with `curl` and
/// the page's readout are looking at the same numbers.
pub fn stats_payload(hub: &Hub) -> StatsPayload {
    hub.stats
        .payload(hub.stats.datagrams_per_s(), hub.cached_session().is_some())
}

async fn api_stats(State(st): State<AppState>) -> impl IntoResponse {
    axum::Json(stats_payload(&st.hub))
}

fn html_response(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn index(State(st): State<AppState>) -> Response {
    html_response(st.pages.index.clone())
}

/// The OBS browser-source page: same app, chrome hidden, `[viz.obs]`
/// defaults applied. URL query parameters override them per source.
async fn obs_page(State(st): State<AppState>) -> Response {
    html_response(st.pages.obs.clone())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn api_sessions(State(st): State<AppState>) -> impl IntoResponse {
    let dir = st.recordings_dir.clone();
    // Directory scan is blocking I/O; keep it off the async worker.
    let list = tokio::task::spawn_blocking(move || recordings::list_recordings(&dir))
        .await
        .unwrap_or_default();
    axum::Json(list)
}

/// Serve one recording as a stream.
///
/// Real sessions are hundreds of megabytes; reading one into a `String` would
/// cost that much resident memory per request and delay the page's first line
/// until the whole file had been read. `ReaderStream` hands the socket 8KB
/// chunks as they come off disk, and the page parses lines as they arrive.
async fn api_session(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let dir = st.recordings_dir.clone();
    // Path resolution scans the directory: blocking I/O, off the async worker.
    let resolved = tokio::task::spawn_blocking(move || recordings::resolve_recording(&dir, &id))
        .await
        .unwrap_or(None);

    let Some(path) = resolved else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "recording vanished between listing and open");
            return (StatusCode::NOT_FOUND, "no such session").into_response();
        }
    };
    // Content-Length lets the page show a percentage while it streams.
    let len = file.metadata().await.ok().map(|m| m.len());

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if let Some(len) = len
        && let Ok(v) = HeaderValue::from_str(&len.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, v);
    }
    (headers, Body::from_stream(ReaderStream::new(file))).into_response()
}

/// WebSocket upgrades are exempt from CORS, so a page anywhere on the web
/// could otherwise open this socket and read every mouse delta. A browser
/// always sends `Origin` on an upgrade; a non-browser client (a script, a
/// second bridge) sends none and is let through.
///
/// The extractor is taken as a `Result` so the origin is judged before any
/// handshake validation: a foreign page gets 403, never a hint about what a
/// well-formed upgrade would look like.
async fn ws_upgrade(
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    headers: HeaderMap,
    State(st): State<AppState>,
) -> Response {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        if !origin_is_trusted(origin) {
            warn!(origin, "refusing websocket from a non-local origin");
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }
    match ws {
        Ok(ws) => ws.on_upgrade(move |socket| client_loop(socket, st)),
        Err(rejection) => rejection.into_response(),
    }
}

/// One shared `Utf8Bytes` frame becomes one WebSocket text frame per client.
/// Every hop of the fan-out — the broadcast clone and this per-client send —
/// is a refcount bump on the single buffer built at acceptance; nothing here
/// copies the payload.
fn text_frame(frame: &crate::hub::Frame) -> Message {
    Message::Text(frame.clone())
}

/// One connected browser. Sends the cached `session` envelope first (so a
/// late-joining page can convert units), then relays broadcast traffic verbatim
/// until either side goes away — or until this client falls behind, which costs
/// it the connection rather than costing every other client memory.
async fn client_loop(socket: WebSocket, st: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (catch_up, mut rx) = st.hub.subscribe();
    st.hub.stats.client_connected();
    info!(
        clients = st.hub.stats.snapshot().clients,
        catch_up = catch_up.len(),
        "ws client connected"
    );

    'relay: {
        for frame in catch_up {
            if sink.send(text_frame(&frame)).await.is_err() {
                break 'relay;
            }
        }

        loop {
            tokio::select! {
                received = rx.recv() => match received {
                    Ok(text) => {
                        if sink.send(text_frame(&text)).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        st.hub.stats.record_lag(missed);
                        warn!(missed, "ws client lagged behind the broadcast channel; disconnecting");
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                    Err(RecvError::Closed) => break,
                },
                // Drain client→server traffic so pings/closes are handled; the
                // browser never sends anything meaningful.
                incoming = stream.next() => match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                },
            }
        }
    }

    st.hub.stats.client_disconnected();
    debug!(
        clients = st.hub.stats.snapshot().clients,
        "ws client disconnected"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    fn state_with(dir: PathBuf) -> AppState {
        AppState {
            hub: Arc::new(Hub::new()),
            recordings_dir: dir,
            pages: Arc::new(Pages::render(&ObsConfig::default())),
        }
    }

    /// The injected config object literal, parsed back out of the page.
    fn injected_config(html: &str) -> serde_json::Value {
        let start = html.find("window.TELEMOUSE_CONFIG = {").expect("config script") + "window.TELEMOUSE_CONFIG = ".len();
        let end = html[start..].find("};").expect("literal end") + start + 1;
        serde_json::from_str(&html[start..end]).expect("injected config is valid JSON")
    }

    #[tokio::test]
    async fn dashboard_and_obs_pages_share_html_but_differ_in_config() {
        let st = state_with(PathBuf::from("recordings"));
        let (s1, _, dash) = get(st.clone(), "/").await;
        let (s2, h2, obs) = get(st, "/obs").await;
        assert_eq!(s1, StatusCode::OK);
        assert_eq!(s2, StatusCode::OK);
        assert!(h2[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
        assert!(!dash.contains(CONFIG_PLACEHOLDER));
        assert!(!obs.contains(CONFIG_PLACEHOLDER));

        let d = injected_config(&dash);
        let o = injected_config(&obs);
        assert_eq!(d["obs_route"], false);
        assert_eq!(o["obs_route"], true);
        // Both carry the defaults, so `?obs=1` on the dashboard works too.
        assert_eq!(d["obs"]["layout"], "split");
        assert_eq!(o["obs"]["hud"], serde_json::json!(["speed", "aim", "cpm"]));
    }

    #[test]
    fn injected_config_cannot_break_out_of_the_script_tag() {
        // Validation would reject this; the renderer must be safe regardless.
        let cfg = ObsConfig {
            background: "</script><script>alert(1)</script>".into(),
            ..Default::default()
        };
        let html = inject_config(INDEX_HTML, &cfg, true);
        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains("\\u003c/script>"));
    }

    #[test]
    fn index_carries_the_config_placeholder() {
        assert_eq!(INDEX_HTML.matches(CONFIG_PLACEHOLDER).count(), 1);
    }

    async fn get(state: AppState, uri: &str) -> (StatusCode, HeaderMap, String) {
        get_with(state, uri, &[("host", "127.0.0.1:7879")]).await
    }

    async fn get_with(
        state: AppState,
        uri: &str,
        extra: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder().uri(uri);
        for (k, v) in extra {
            req = req.header(*k, *v);
        }
        let res = router(state)
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), 64 * 1024 * 1024).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn rebound_host_names_are_refused_everywhere() {
        let st = state_with(PathBuf::from("recordings"));
        for uri in ["/", "/healthz", "/api/stats", "/api/sessions", "/api/session/x", "/ws"] {
            let (status, _, _) = get_with(st.clone(), uri, &[("host", "evil.com:7879")]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
        // A missing Host (HTTP/1.0 client) is not a browser and not a rebinding.
        let (status, _, _) = get_with(st.clone(), "/healthz", &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        for host in ["localhost:7879", "127.0.0.1", "[::1]:7879", "192.168.1.20:7879"] {
            let (status, _, _) = get_with(st.clone(), "/healthz", &[("host", host)]).await;
            assert_eq!(status, StatusCode::OK, "{host}");
        }
    }

    /// The `/ws` route is reached through `require_local_host` and then the
    /// Origin check; both must answer before any upgrade handshake happens.
    #[tokio::test]
    async fn websocket_refuses_foreign_origins() {
        let st = state_with(PathBuf::from("recordings"));
        let ws_headers = |origin: Option<&'static str>| {
            let mut h = vec![
                ("host", "127.0.0.1:7879"),
                ("connection", "upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-version", "13"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ];
            if let Some(o) = origin {
                h.push(("origin", o));
            }
            h
        };
        let (status, _, body) = get_with(st.clone(), "/ws", &ws_headers(Some("http://evil.com"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "origin not allowed");
        let (status, _, _) = get_with(st.clone(), "/ws", &ws_headers(Some("null"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Same-machine browser, and a non-browser client with no Origin at all,
        // both get past the origin check and into the handshake. `oneshot`
        // carries no upgradable connection, so the handshake itself answers
        // 426 — which is the extractor speaking, not the origin check.
        for origin in [Some("http://127.0.0.1:7879"), Some("http://localhost:7879"), None] {
            let (status, _, body) = get_with(st.clone(), "/ws", &ws_headers(origin)).await;
            assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "{origin:?}");
            assert_ne!(body, "origin not allowed");
        }
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (status, _, body) = get(state_with(PathBuf::from("recordings")), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn api_stats_returns_the_documented_shape() {
        let st = state_with(PathBuf::from("recordings"));
        // Give the bridge something to report.
        let anchor = crate::stats::now_utc_us();
        let batch = format!(
            r#"{{"type":"batch","session_id":"s-1","seq_no":0,"ts_anchor_us":{},"events":[]}}"#,
            anchor - 3_000
        );
        st.hub.publish(batch.as_bytes()).unwrap();
        st.hub.publish(b"junk").unwrap_err();
        st.hub.stats.set_datagrams_per_s(40.0);

        let (status, headers, body) = get(st, "/api/stats").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );

        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["type"], "viz_stats");
        assert_eq!(v["datagrams"], 2);
        assert_eq!(v["forwarded"], 1);
        assert_eq!(v["parse_errors"], 1);
        assert_eq!(v["datagrams_per_s"], 40.0);
        assert_eq!(v["clients"], 0);
        assert_eq!(v["session_cached"], false);
        assert_eq!(v["latency"]["samples"], 1);
        assert!(v["uptime_s"].as_f64().unwrap() >= 0.0);
    }

    #[tokio::test]
    async fn api_session_streams_a_recording_with_a_length() {
        let tmp = tempfile::tempdir().unwrap();
        let body_text = "{\"type\":\"session\"}\n{\"type\":\"batch\"}\n";
        std::fs::write(tmp.path().join("s-1.jsonl"), body_text).unwrap();

        let (status, headers, body) =
            get(state_with(tmp.path().to_path_buf()), "/api/session/s-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, body_text);
        // Content-Length lets the page render a load percentage while it
        // streams; the body itself arrives in chunks, never as one String.
        assert_eq!(
            headers[header::CONTENT_LENGTH].to_str().unwrap(),
            body_text.len().to_string()
        );
    }

    #[tokio::test]
    async fn api_session_rejects_an_unknown_id() {
        let tmp = tempfile::tempdir().unwrap();
        let (status, _, _) = get(state_with(tmp.path().to_path_buf()), "/api/session/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn index_is_a_self_contained_page() {
        assert!(INDEX_HTML.contains("<html"));
        assert!(INDEX_HTML.len() > 10_000, "page looks truncated");
        // No external assets: nothing may be fetched from another origin.
        for forbidden in ["src=\"http", "href=\"http", "//cdn.", "unpkg.com", "jsdelivr"] {
            assert!(
                !INDEX_HTML.contains(forbidden),
                "page references external asset: {forbidden}"
            );
        }
    }

    #[test]
    fn router_builds_with_a_state() {
        let _ = router(state_with(PathBuf::from("recordings")));
    }
}
