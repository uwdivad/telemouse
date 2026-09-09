//! HTTP + WebSocket surface: the embedded page, the replay REST endpoints, and
//! the `/ws` live fan-out.
//!
//! There is no authentication: this is a local tool. What it does defend
//! against is the browser itself being used against it — a web page the user
//! happens to have open reading the live stream or the recordings. Every
//! request must carry a `Host` naming this machine (so a DNS-rebound name is
//! refused), and a WebSocket upgrade must come from a local `Origin` or from
//! no browser at all. See [`telemouse_core::localhost`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{
    Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use telemouse_core::config::ObsConfig;
use telemouse_core::localhost::{SECURITY_HEADERS, host_is_trusted, origin_is_trusted};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::hub::Hub;
use crate::recordings::{self, SessionEntry};
use crate::stats::{StatsPayload, now_utc_us};

/// The page shell: markup and CSS, with one `<script src="app.js">` tag where
/// the app goes. Opened from disk it loads `app.js` as a file; served, the
/// script is inlined by [`page_source`] so the page is one self-contained
/// response with no external assets and no CDN.
pub const INDEX_HTML: &str = include_str!("index.html");
/// The app: engine, panels, transport, OBS mode. Kept as its own file so it
/// can be syntax-checked and unit-tested with Node without a browser.
pub const APP_JS: &str = include_str!("app.js");
/// The tag [`page_source`] replaces with the inlined script.
const APP_SCRIPT_TAG: &str = "<script src=\"app.js\"></script>";

/// The complete single-file page: [`INDEX_HTML`] with [`APP_JS`] inlined.
pub fn page_source() -> String {
    INDEX_HTML.replacen(APP_SCRIPT_TAG, &format!("<script>\n{APP_JS}</script>"), 1)
}

/// Routes a peer that is *not* on this machine may reach when the server is
/// bound off loopback: the OBS overlay, its live socket, and health. The
/// dashboard, the recording list and the recordings themselves stay on the
/// machine — a second PC needs the overlay, not the archive.
pub const LAN_ROUTES: &[&str] = &["/obs", "/ws", "/healthz"];

/// Most WebSocket clients served at once. Each costs ~72 µs of send work per
/// batch; a dashboard, an OBS source or two and a script is a handful, and
/// anything past this is a loop somebody left running.
pub const MAX_WS_CLIENTS: i64 = 16;

/// Server-side keepalive: a client whose machine vanished without a FIN
/// (a second PC losing power) otherwise holds its slot and inflates the
/// client gauge until the TCP timeout, tens of minutes on Windows.
pub const WS_PING_INTERVAL: Duration = Duration::from_secs(20);

/// How long a `/api/sessions` listing is reused. Listing probes 128 KB of
/// every recording, so a poll loop against a LAN bind would otherwise cost
/// the disk on every request; five seconds is invisible to a human picking
/// a session from the dropdown.
pub const SESSIONS_CACHE_TTL: Duration = Duration::from_secs(5);

/// The address of the peer behind one accepted connection, attached to every
/// request as `ConnectInfo<Peer>` (a local type, because the listener that
/// produces it is local — axum only ships the impl for its own listener).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer(pub SocketAddr);

/// A time-bounded copy of the last recording listing.
#[derive(Default)]
pub struct SessionsCache {
    inner: Mutex<Option<(Instant, Vec<SessionEntry>)>>,
}

impl SessionsCache {
    /// The cached listing if it was taken less than `ttl` before `now`.
    pub fn get(&self, now: Instant, ttl: Duration) -> Option<Vec<SessionEntry>> {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some((at, list)) if now.saturating_duration_since(*at) < ttl => Some(list.clone()),
            _ => None,
        }
    }

    pub fn put(&self, now: Instant, list: Vec<SessionEntry>) {
        *self.inner.lock().unwrap_or_else(|p| p.into_inner()) = Some((now, list));
    }
}

/// Where the server splices its config into the page. Sits inside a
/// `<script>` object literal, so the raw file (placeholder intact) is still
/// valid JavaScript and an unconfigured page just sees `{}`.
const CONFIG_PLACEHOLDER: &str = "/*__TELEMOUSE_CONFIG__*/";

/// Amortize filesystem reads and HTTP body polling for large replay files.
/// `ReaderStream` otherwise defaults to 4 KiB, which leaves substantial
/// syscall and stream-wakeup overhead on recordings hundreds of MiB large.
const SESSION_STREAM_CAPACITY: usize = 256 * 1024;

/// The two flavours of the page, rendered once at startup: the dashboard and
/// the OBS browser-source variant. Same HTML, different injected config.
pub struct Pages {
    pub index: String,
    pub obs: String,
}

impl Pages {
    pub fn render(obs: &ObsConfig) -> Self {
        let page = page_source();
        Self {
            index: inject_config(&page, obs, false),
            obs: inject_config(&page, obs, true),
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
    pub sessions: Arc<SessionsCache>,
}

/// Layers, innermost first: the `Host` rule on every route, then the
/// network-peer gate, then the security headers on every response (the
/// refusals included).
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
        .layer(middleware::from_fn(lan_gate))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Whether a request from `peer` for `path` is served. Loopback peers (and
/// requests with no peer address, which only happens in tests) get
/// everything; anyone else gets [`LAN_ROUTES`]. Pure, so the policy is
/// tested without sockets.
pub fn peer_may_reach(peer: Option<IpAddr>, path: &str) -> bool {
    match peer {
        Some(ip) if !ip.is_loopback() => LAN_ROUTES.contains(&path),
        _ => true,
    }
}

/// Refuse a network peer anything but the overlay routes. The bind decides
/// whether a network peer can connect at all; this decides what it sees.
/// Refusals are logged at most once a minute per address, so a scanner on
/// the LAN cannot fill the log.
async fn lan_gate(req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .map(|c| c.0.0.ip());
    let path = req.uri().path();
    if !peer_may_reach(peer, path) {
        static LAST_WARNED: Mutex<Option<HashMap<IpAddr, Instant>>> = Mutex::new(None);
        let ip = peer.unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let mut guard = LAST_WARNED.lock().unwrap_or_else(|p| p.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        let now = Instant::now();
        let quiet = map
            .get(&ip)
            .is_some_and(|at| now.duration_since(*at) < Duration::from_secs(60));
        if !quiet {
            map.insert(ip, now);
            warn!(
                peer = %ip,
                path,
                "refusing a network peer; only /obs, /ws and /healthz are served off this machine (further refusals from this address are quiet for a minute)"
            );
        }
        return (StatusCode::FORBIDDEN, "not served to the network").into_response();
    }
    next.run(req).await
}

/// Add [`SECURITY_HEADERS`] to every response.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    for (name, value) in SECURITY_HEADERS {
        res.headers_mut().insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    res
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

/// Live-mode health: 200 while the UDP listener is bound, 503 otherwise,
/// with the seconds since the last datagram either way. A constant "ok"
/// would report a bridge whose listener never came up as healthy.
async fn healthz(State(st): State<AppState>) -> Response {
    let h = st.hub.stats.health(now_utc_us());
    let status = if h.ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(h)).into_response()
}

async fn api_sessions(State(st): State<AppState>) -> impl IntoResponse {
    if let Some(list) = st.sessions.get(Instant::now(), SESSIONS_CACHE_TTL) {
        return axum::Json(list);
    }
    let dir = st.recordings_dir.clone();
    // Directory scan is blocking I/O; keep it off the async worker.
    let list = tokio::task::spawn_blocking(move || recordings::list_recordings(&dir))
        .await
        .unwrap_or_default();
    st.sessions.put(Instant::now(), list.clone());
    axum::Json(list)
}

/// Serve one recording as a stream.
///
/// Real sessions are hundreds of megabytes; reading one into a `String` would
/// cost that much resident memory per request and delay the page's first line
/// until the whole file had been read. `ReaderStream` hands the socket bounded
/// 256 KiB chunks as they come off disk, and the page parses lines as they
/// arrive.
async fn api_session(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let dir = st.recordings_dir.clone();
    // Path resolution performs filesystem metadata I/O; keep it off the async
    // worker even though it now looks up only the requested recording.
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
    (
        headers,
        Body::from_stream(ReaderStream::with_capacity(file, SESSION_STREAM_CAPACITY)),
    )
        .into_response()
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
    let clients = st.hub.stats.snapshot().clients;
    if clients >= MAX_WS_CLIENTS {
        warn!(
            clients,
            max = MAX_WS_CLIENTS,
            "refusing websocket: too many live clients"
        );
        return (StatusCode::SERVICE_UNAVAILABLE, "too many live clients").into_response();
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

    let mut ping = tokio::time::interval(WS_PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick fires at once; the client just connected

    'relay: {
        for frame in catch_up {
            if sink.send(text_frame(&frame)).await.is_err() {
                break 'relay;
            }
        }

        loop {
            tokio::select! {
                // A ping the peer never answers surfaces as a failed send or a
                // closed stream within a few intervals, instead of a TCP
                // timeout; browsers answer pongs automatically.
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                },
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
        let hub = Arc::new(Hub::new());
        // A running server has its listener bound; the tests that care about
        // the unbound case clear this themselves.
        hub.stats.set_udp_bound(true);
        AppState {
            hub,
            recordings_dir: dir,
            pages: Arc::new(Pages::render(&ObsConfig::default())),
            sessions: Arc::new(SessionsCache::default()),
        }
    }

    /// A request as it arrives from a real socket: with the peer address
    /// that `into_make_service_with_connect_info` attaches.
    async fn get_from_peer(
        state: AppState,
        uri: &str,
        peer: SocketAddr,
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder()
            .uri(uri)
            .header("host", "192.168.1.168:7879")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(Peer(peer)));
        let res = router(state).oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), 64 * 1024 * 1024).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).to_string())
    }

    /// The injected config object literal, parsed back out of the page.
    fn injected_config(html: &str) -> serde_json::Value {
        let start = html
            .find("window.TELEMOUSE_CONFIG = {")
            .expect("config script")
            + "window.TELEMOUSE_CONFIG = ".len();
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
        assert!(
            h2[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
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
        let html = inject_config(&page_source(), &cfg, true);
        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains("\\u003c/script>"));
    }

    #[test]
    fn index_carries_the_config_placeholder() {
        assert_eq!(INDEX_HTML.matches(CONFIG_PLACEHOLDER).count(), 1);
        assert_eq!(page_source().matches(CONFIG_PLACEHOLDER).count(), 1);
    }

    /// The shell references the app exactly once; the served page inlines it
    /// and references nothing.
    #[test]
    fn the_served_page_inlines_the_app() {
        assert_eq!(INDEX_HTML.matches(APP_SCRIPT_TAG).count(), 1);
        let page = page_source();
        assert!(!page.contains(APP_SCRIPT_TAG));
        assert!(
            !page.contains("src=\""),
            "the served page must not fetch anything"
        );
        assert!(page.contains(APP_JS.trim_end()));
        assert!(APP_JS.starts_with("\"use strict\";"));
        // The app's own script closes cleanly before the body does.
        let script_end = page.rfind("</script>").unwrap();
        assert!(script_end < page.rfind("</body>").unwrap());
    }

    /// Run `node` with `args` from the crate root when Node is installed (CI
    /// has it); `Ok(None)` when it is not.
    fn node(args: &[&str]) -> std::io::Result<Option<std::process::Output>> {
        match std::process::Command::new("node")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
        {
            Ok(out) => Ok(Some(out)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("node not installed; skipping {}", args.join(" "));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// `node --check` over the app: the syntax gate a 2,400-line script
    /// embedded in a Rust binary would otherwise not have.
    #[test]
    fn app_js_parses_under_node_when_available() {
        if let Some(out) = node(&["--check", "src/app.js"]).unwrap() {
            assert!(
                out.status.success(),
                "node --check failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The engine's unit tests (`js-tests/engine.test.mjs`) against a stub
    /// DOM: unit conversion, unwrapped yaw, loss accounting, the live-buffer
    /// floor, the memory cap, checkpointed seeking, session restarts and OBS
    /// parameter clamping.
    #[test]
    fn app_js_engine_tests_pass_under_node_when_available() {
        if let Some(out) = node(&["--test", "js-tests/engine.test.mjs"]).unwrap() {
            assert!(
                out.status.success(),
                "node --test failed:\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn network_peers_only_reach_the_overlay_routes() {
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        for path in LAN_ROUTES {
            assert!(peer_may_reach(Some(lan), path), "{path}");
        }
        for path in [
            "/",
            "/index.html",
            "/api/sessions",
            "/api/session/x",
            "/api/stats",
        ] {
            assert!(!peer_may_reach(Some(lan), path), "{path}");
            assert!(peer_may_reach(Some(local), path), "{path}");
            assert!(peer_may_reach(None, path), "{path}");
        }
        assert!(peer_may_reach(
            Some("::1".parse().unwrap()),
            "/api/sessions"
        ));
        assert!(!peer_may_reach(
            Some("fe80::1".parse().unwrap()),
            "/api/sessions"
        ));
    }

    #[tokio::test]
    async fn a_lan_peer_is_refused_everything_but_the_overlay() {
        let st = state_with(PathBuf::from("recordings"));
        let lan: SocketAddr = "192.168.1.20:51000".parse().unwrap();
        for uri in [
            "/",
            "/index.html",
            "/api/sessions",
            "/api/session/x",
            "/api/stats",
        ] {
            let (status, _, body) = get_from_peer(st.clone(), uri, lan).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(body, "not served to the network", "{uri}");
        }
        let (status, _, _) = get_from_peer(st.clone(), "/obs", lan).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = get_from_peer(st.clone(), "/healthz", lan).await;
        assert_eq!(status, StatusCode::OK);
        // The same machine, through its LAN address, still gets everything.
        let here: SocketAddr = "127.0.0.1:51000".parse().unwrap();
        let (status, _, _) = get_from_peer(st, "/api/sessions", here).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        let st = state_with(PathBuf::from("recordings"));
        let (status, headers, _) = get(st.clone(), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        // Refusals too.
        let (status, headers, _) = get_with(st, "/", &[("host", "evil.com")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(headers["x-frame-options"], "DENY");
    }

    #[tokio::test]
    async fn healthz_reports_the_udp_listener_state() {
        let st = state_with(PathBuf::from("recordings"));
        st.hub.stats.set_udp_bound(false);
        let (status, _, body) = get(st.clone(), "/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["udp_bound"], false);
        assert!(v["last_datagram_age_s"].is_null());

        st.hub.stats.set_udp_bound(true);
        st.hub.publish(b"junk").unwrap_err();
        let (status, _, body) = get(st, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["last_datagram_age_s"].as_f64().unwrap() >= 0.0);
        assert_eq!(v["clients"], 0);
    }

    #[tokio::test]
    async fn the_client_cap_answers_503_before_the_handshake() {
        let st = state_with(PathBuf::from("recordings"));
        for _ in 0..MAX_WS_CLIENTS {
            st.hub.stats.client_connected();
        }
        let headers = [
            ("host", "127.0.0.1:7879"),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("origin", "http://127.0.0.1:7879"),
        ];
        let (status, _, body) = get_with(st.clone(), "/ws", &headers).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "too many live clients");
        st.hub.stats.client_disconnected();
        let (status, _, _) = get_with(st, "/ws", &headers).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "one slot free again");
    }

    #[test]
    fn the_sessions_cache_expires_by_ttl() {
        let cache = SessionsCache::default();
        let t0 = Instant::now();
        assert!(cache.get(t0, SESSIONS_CACHE_TTL).is_none());
        cache.put(t0, Vec::new());
        assert_eq!(cache.get(t0, SESSIONS_CACHE_TTL), Some(Vec::new()));
        assert!(
            cache
                .get(t0 + Duration::from_secs(4), SESSIONS_CACHE_TTL)
                .is_some()
        );
        assert!(
            cache
                .get(t0 + SESSIONS_CACHE_TTL, SESSIONS_CACHE_TTL)
                .is_none()
        );
    }

    #[tokio::test]
    async fn api_sessions_reuses_a_fresh_listing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("one.jsonl"), "{}").unwrap();
        let st = state_with(tmp.path().to_path_buf());
        let (_, _, body) = get(st.clone(), "/api/sessions").await;
        assert!(body.contains("\"one\""));
        // A recording that appears inside the TTL is not listed yet...
        std::fs::write(tmp.path().join("two.jsonl"), "{}").unwrap();
        let (_, _, body) = get(st.clone(), "/api/sessions").await;
        assert!(!body.contains("\"two\""), "served from the cache");
        // ...and is once the cache is stale.
        st.sessions
            .put(Instant::now() - SESSIONS_CACHE_TTL * 2, Vec::new());
        let (_, _, body) = get(st, "/api/sessions").await;
        assert!(body.contains("\"two\""));
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
        for uri in [
            "/",
            "/healthz",
            "/api/stats",
            "/api/sessions",
            "/api/session/x",
            "/ws",
        ] {
            let (status, _, _) = get_with(st.clone(), uri, &[("host", "evil.com:7879")]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
        // A missing Host (HTTP/1.0 client) is not a browser and not a rebinding.
        let (status, _, _) = get_with(st.clone(), "/healthz", &[]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        for host in [
            "localhost:7879",
            "127.0.0.1",
            "[::1]:7879",
            "192.168.1.20:7879",
        ] {
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
        let (status, _, body) =
            get_with(st.clone(), "/ws", &ws_headers(Some("http://evil.com"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, "origin not allowed");
        let (status, _, _) = get_with(st.clone(), "/ws", &ws_headers(Some("null"))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Same-machine browser, and a non-browser client with no Origin at all,
        // both get past the origin check and into the handshake. `oneshot`
        // carries no upgradable connection, so the handshake itself answers
        // 426 — which is the extractor speaking, not the origin check.
        for origin in [
            Some("http://127.0.0.1:7879"),
            Some("http://localhost:7879"),
            None,
        ] {
            let (status, _, body) = get_with(st.clone(), "/ws", &ws_headers(origin)).await;
            assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "{origin:?}");
            assert_ne!(body, "origin not allowed");
        }
    }

    #[tokio::test]
    async fn healthz_is_ok_with_the_listener_bound() {
        let (status, headers, body) =
            get(state_with(PathBuf::from("recordings")), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        assert!(body.contains("\"ok\":true"));
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
        let page = page_source();
        assert!(page.contains("<html"));
        assert!(page.len() > 10_000, "page looks truncated");
        // No external assets: nothing may be fetched from another origin.
        for forbidden in [
            "src=\"http",
            "href=\"http",
            "//cdn.",
            "unpkg.com",
            "jsdelivr",
        ] {
            assert!(
                !page.contains(forbidden),
                "page references external asset: {forbidden}"
            );
        }
    }

    #[test]
    fn index_live_buffer_defaults_are_consistent() {
        assert!(INDEX_HTML.contains(r#"id="buf" min="10" max="200" step="5" value="35""#));
        assert!(INDEX_HTML.contains(r#"id="bufVal">35ms"#));
        assert!(APP_JS.contains("buffer_ms: 35,"));
        assert!(APP_JS.contains("const LIVE_BUFFER_DEFAULT = 0.035;"));
    }

    #[test]
    fn router_builds_with_a_state() {
        let _ = router(state_with(PathBuf::from("recordings")));
    }
}
