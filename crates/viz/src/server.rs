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
#[cfg(not(feature = "observability"))]
use tracing::debug;
#[cfg(feature = "observability")]
use tracing::info;
use tracing::warn;

use crate::hub::Hub;
use crate::recordings::{self, SessionEntry};
use crate::shutdown::Shutdown;
#[cfg(feature = "observability")]
use crate::stats::StatsPayload;
use crate::stats::now_utc_us;

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

/// The port the refusal pages name when the bind address is unreadable: the
/// documented default, and what a user's notes will say.
pub const DEFAULT_HTTP_PORT: u16 = 7879;

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

/// The peer address as an extractor that tolerates its absence. A request
/// that never came off the listener (a test, an in-process call) carries no
/// `ConnectInfo`, and a missing one is a handler that logs `-`, not a 500.
type MaybePeer = Result<ConnectInfo<Peer>, axum::extract::rejection::ExtensionRejection>;

/// The socket address behind [`MaybePeer`], if there was one.
fn peer_addr(peer: MaybePeer) -> Option<SocketAddr> {
    peer.ok().map(|c| c.0.0)
}

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
    pub fn render(obs: &ObsConfig, udp_addr: &str) -> Self {
        let page = page_source();
        Self {
            index: inject_config(&page, obs, false, udp_addr),
            obs: inject_config(&page, obs, true, udp_addr),
        }
    }
}

/// Splice `{ obs_route, udp_addr, obs }` into the page. `<` is escaped so a
/// config string can never close the `<script>` element it is embedded in.
///
/// `udp_addr` is what the page's "waiting for capture" state names: a page
/// that says *where* it is waiting for is the difference between "the overlay
/// is broken" and "the capture agent is not running / is shipping to another
/// port".
fn inject_config(html: &str, obs: &ObsConfig, obs_route: bool, udp_addr: &str) -> String {
    let cfg = serde_json::json!({ "obs_route": obs_route, "udp_addr": udp_addr, "obs": obs });
    let mut json = cfg.to_string();
    // Object literal body: strip the outer braces and drop it into `{...}`.
    json = json[1..json.len() - 1].replace('<', "\\u003c");
    html.replace(CONFIG_PLACEHOLDER, &json)
}

/// Where this process listens, as configured — for `/healthz`, the page's
/// waiting state, and the refusal pages' "open this instead" hint.
#[derive(Debug, Clone, Default)]
pub struct Addrs {
    pub udp: String,
    pub http: String,
}

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub recordings_dir: PathBuf,
    pub pages: Arc<Pages>,
    pub sessions: Arc<SessionsCache>,
    pub addrs: Arc<Addrs>,
    /// Fired when the process is asked to stop. A live WebSocket outlives the
    /// HTTP connection it was upgraded from, so nothing else would tell these
    /// tasks to let go.
    pub shutdown: Shutdown,
}

/// Layers, innermost first: the `Host` rule on every route, then the
/// network-peer gate, then the security headers on every response (the
/// refusals included).
pub fn router(state: AppState) -> Router {
    let router = Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/obs", get(obs_page))
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_upgrade));
    #[cfg(feature = "observability")]
    let router = router.route("/api/stats", get(api_stats));
    router
        .route("/api/sessions", get(api_sessions))
        .route("/api/session/{id}", get(api_session))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_host,
        ))
        .layer(middleware::from_fn(lan_gate))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// A refusal a human can act on. Both gates below answer a browser, so both
/// answer in HTML: a bare "not served to the network" in a viewport is a
/// mystery, and the remedy is one line of text.
///
/// Self-contained (inline style, no assets) so it renders under the page's own
/// no-external-assets rule and inside an OBS browser source.
fn refusal(title: &str, body: String) -> Response {
    let html = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>telemouse — {title}</title>\
         <style>body{{background:#080a0f;color:#d5deea;font:14px/1.6 system-ui,sans-serif;\
         margin:0;padding:48px 28px;max-width:46em}}\
         h1{{font:600 16px/1.4 ui-monospace,monospace;color:#ffb02e;letter-spacing:.06em;\
         text-transform:uppercase;margin:0 0 14px}}\
         code,a{{font-family:ui-monospace,monospace;color:#35d0e0}}\
         p{{margin:0 0 12px}}</style>\
         <h1>{title}</h1>{body}"
    );
    (
        StatusCode::FORBIDDEN,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

/// The URL a peer that just got refused should try instead, as an HTML
/// fragment: a link to the bound address when it names an interface, and a
/// placeholder when the bind is a wildcard (`0.0.0.0` is where the server
/// listens, not somewhere a browser can go).
fn overlay_url_hint(http_addr: &str) -> String {
    let port = match http_addr.trim().parse::<SocketAddr>() {
        Ok(addr) if !addr.ip().is_unspecified() => {
            let url = format!(
                "http://{}/obs",
                telemouse_core::localhost::browse_addr(addr)
            );
            return format!("<a href=\"{url}\">{url}</a>");
        }
        Ok(addr) => addr.port(),
        Err(_) => DEFAULT_HTTP_PORT,
    };
    format!("<code>http://&lt;this PC's IPv4&gt;:{port}/obs</code>")
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
        return refusal(
            "not served to the network",
            format!(
                "<p>This is the telemouse viz bridge on another machine. Only \
                 <code>/obs</code> (the OBS browser source), <code>/ws</code> (its live \
                 socket) and <code>/healthz</code> are served off that machine — the \
                 dashboard and the recordings stay on the PC that captured them.</p>\
                 <p><a href=\"/obs\">Open the overlay instead → /obs</a></p>\
                 <p>You asked for <code>{}</code>.</p>",
                html_escape(path)
            ),
        );
    }
    next.run(req).await
}

/// The little escaping the refusal pages need: they interpolate a request
/// path and a host header, both attacker-controlled.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
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
async fn require_local_host(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_is_trusted(host) {
        warn!(host, path = %req.uri().path(), "refusing request with a non-local Host header");
        return refusal(
            "host not allowed",
            format!(
                "<p>This server answers only to <code>localhost</code> or to an IP address \
                 literal. A request that arrives under some other name is what a \
                 DNS-rebinding page looks like from here, so it is refused — the name asked \
                 for was <code>{}</code>.</p>\
                 <p>Open {} instead (or <code>http://localhost:{}/</code> on this machine).</p>",
                html_escape(host),
                overlay_url_hint(&st.addrs.http),
                st.addrs
                    .http
                    .trim()
                    .parse::<SocketAddr>()
                    .map(|a| a.port())
                    .unwrap_or(DEFAULT_HTTP_PORT),
            ),
        );
    }
    next.run(req).await
}

/// The bridge's own health, in the same shape the page receives once a second
/// over the WebSocket (`{"type":"viz_stats",...}`) — so a human with `curl` and
/// the page's readout are looking at the same numbers.
#[cfg(feature = "observability")]
pub fn stats_payload(hub: &Hub) -> StatsPayload {
    hub.stats.payload(hub.cached_session().is_some())
}

#[cfg(feature = "observability")]
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
    let h = st
        .hub
        .stats
        .health(now_utc_us(), &st.addrs.udp, &st.addrs.http);
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
    let list = match tokio::task::spawn_blocking(move || recordings::list_recordings(&dir)).await {
        Ok(list) => list,
        // The listing task panicked or was cancelled. An empty list is a
        // survivable answer, but a silent one reads as "no recordings".
        Err(e) => {
            warn!(error = %e, "listing recordings failed; answering with an empty list");
            Vec::new()
        }
    };
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
async fn api_session(
    State(st): State<AppState>,
    peer: MaybePeer,
    Path(id): Path<String>,
) -> Response {
    let dir = st.recordings_dir.clone();
    let want = id.clone();
    // Path resolution performs filesystem metadata I/O; keep it off the async
    // worker even though it now looks up only the requested recording.
    let resolved =
        match tokio::task::spawn_blocking(move || recordings::resolve_recording(&dir, &id)).await {
            Ok(found) => found,
            Err(e) => {
                warn!(error = %e, id = %want, "resolving a recording failed");
                None
            }
        };

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
    let stream = ReaderStream::with_capacity(file, SESSION_STREAM_CAPACITY);
    #[cfg(feature = "observability")]
    let stream = {
        // Logged when the body is *finished* (or dropped), not when the
        // handler returns: a 400 MB recording spends its whole life in the
        // stream, and "how long did that replay take to hand over" is the
        // question this line answers. A client that closed early shows up as
        // fewer bytes than Content-Length.
        let mut log = ReplayLog {
            id: want,
            peer: peer_addr(peer),
            started: Instant::now(),
            bytes: 0,
            total: len,
        };
        futures_util::StreamExt::map(stream, move |item| {
            if let Ok(chunk) = &item {
                // Through a method, so the closure captures the whole guard:
                // capturing just the field would drop the rest of it here.
                log.sent(chunk.len() as u64);
            }
            item
        })
    };
    #[cfg(not(feature = "observability"))]
    let _ = (peer, want);
    (headers, Body::from_stream(stream)).into_response()
}

/// Owned by the replay body's stream adapter: logs the transfer when the
/// stream is dropped, whichever way it ended.
#[cfg(feature = "observability")]
struct ReplayLog {
    id: String,
    peer: Option<SocketAddr>,
    started: Instant,
    bytes: u64,
    total: Option<u64>,
}

#[cfg(feature = "observability")]
impl ReplayLog {
    fn sent(&mut self, bytes: u64) {
        self.bytes += bytes;
    }
}

#[cfg(feature = "observability")]
impl Drop for ReplayLog {
    fn drop(&mut self) {
        let secs = self.started.elapsed().as_secs_f64();
        let mb = self.bytes as f64 / (1024.0 * 1024.0);
        info!(
            id = %self.id,
            peer = self.peer.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            bytes = self.bytes,
            total = self.total,
            complete = self.total.is_none_or(|t| t == self.bytes),
            elapsed_s = format_args!("{secs:.2}"),
            mb_per_s = format_args!("{:.1}", if secs > 0.0 { mb / secs } else { 0.0 }),
            "served a recording"
        );
    }
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
    peer: MaybePeer,
    headers: HeaderMap,
    State(st): State<AppState>,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(origin) = origin.as_deref()
        && !origin_is_trusted(origin)
    {
        warn!(origin, "refusing websocket from a non-local origin");
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    if st.shutdown.fired() {
        // A stop is already in flight, and a socket accepted now would be
        // closed in the same breath — and would hold the stop open while it
        // was. 503 puts the page straight into its reconnect backoff, which
        // is where it wants to be: this process is going away and the next
        // one will answer.
        return (StatusCode::SERVICE_UNAVAILABLE, "stopping").into_response();
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
    let peer = peer_addr(peer);
    match ws {
        Ok(ws) => ws.on_upgrade(move |socket| client_loop(socket, st, peer, origin)),
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
async fn client_loop(
    socket: WebSocket,
    st: AppState,
    peer: Option<SocketAddr>,
    origin: Option<String>,
) {
    let (mut sink, mut stream) = socket.split();
    let (catch_up, mut rx) = st.hub.subscribe();
    st.hub.stats.client_connected();
    let connected_at = Instant::now();
    let mut frames_sent: u64 = 0;
    // Did this client go because the *server* is going? The page treats every
    // close the same (disconnect, then reconnect), but the log should not read
    // as a browser that walked away during a stop.
    let mut stopped = false;
    // Who is watching, and from where: on a LAN bind the answer to "why are
    // there four clients" is a second PC's OBS reconnecting, and the peer is
    // the only thing that says so.
    let _ = (&peer, &origin);
    #[cfg(feature = "observability")]
    info!(
        clients = st.hub.stats.snapshot().clients,
        catch_up = catch_up.len(),
        peer = peer.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
        loopback = peer.is_some_and(|p| p.ip().is_loopback()),
        origin = origin.as_deref().unwrap_or("-"),
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
            frames_sent += 1;
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
                        frames_sent += 1;
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
                // The process is stopping. This socket was upgraded out of
                // its HTTP connection and nothing else will close it, so say
                // goodbye: the page then shows "disconnected" and starts
                // reconnecting, instead of holding a socket that has gone
                // quiet until TCP notices — and this task ends, which is what
                // lets the stop finish in milliseconds.
                () = st.shutdown.wait() => {
                    let _ = sink.send(Message::Close(None)).await;
                    stopped = true;
                    break;
                },
            }
        }
    }

    st.hub.stats.client_disconnected();
    // A disconnect is what an operator actually chases (an OBS source that
    // drops every few minutes, a second PC that never holds the socket), so
    // it carries how long the client lasted and how much it got.
    #[cfg(feature = "observability")]
    info!(
        clients = st.hub.stats.snapshot().clients,
        frames_sent,
        held_s = format_args!("{:.1}", connected_at.elapsed().as_secs_f64()),
        peer = peer.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
        stopped,
        "ws client disconnected"
    );
    #[cfg(not(feature = "observability"))]
    debug!(
        clients = st.hub.stats.snapshot().clients,
        frames_sent,
        held_s = format_args!("{:.1}", connected_at.elapsed().as_secs_f64()),
        stopped,
        "ws client disconnected"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    const TEST_UDP: &str = "127.0.0.1:7878";
    const TEST_HTTP: &str = "192.168.1.168:7879";

    fn state_with(dir: PathBuf) -> AppState {
        let hub = Arc::new(Hub::new());
        // A running server has its listener bound; the tests that care about
        // the unbound case clear this themselves.
        hub.stats.set_udp_bound(true);
        AppState {
            hub,
            recordings_dir: dir,
            pages: Arc::new(Pages::render(&ObsConfig::default(), TEST_UDP)),
            sessions: Arc::new(SessionsCache::default()),
            addrs: Arc::new(Addrs {
                udp: TEST_UDP.into(),
                http: TEST_HTTP.into(),
            }),
            shutdown: Shutdown::new(),
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
        let html = inject_config(&page_source(), &cfg, true, "127.0.0.1:7878");
        assert!(!html.contains("</script><script>alert"));
        assert!(html.contains("\\u003c/script>"));
    }

    /// The page says *where* it is waiting for a capture agent, which it can
    /// only know because the server told it.
    #[tokio::test]
    async fn the_page_is_told_which_udp_address_feeds_it() {
        let st = state_with(PathBuf::from("recordings"));
        for uri in ["/", "/obs"] {
            let (status, _, body) = get(st.clone(), uri).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(injected_config(&body)["udp_addr"], TEST_UDP, "{uri}");
        }
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

    /// Every `js-tests/*.test.mjs`: the engine's unit tests against a stub
    /// DOM (unit conversion, unwrapped yaw, loss accounting, the live-buffer
    /// floor, the memory cap, checkpointed seeking, session restarts, OBS
    /// parameter clamping) and the DOM-id check that every element the
    /// script looks up exists in the page.
    #[test]
    fn app_js_engine_tests_pass_under_node_when_available() {
        // Node expands the glob itself, so this works from any shell.
        if let Some(out) = node(&["--test", "js-tests/*.test.mjs"]).unwrap() {
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
        for uri in ["/", "/index.html", "/api/sessions", "/api/session/x"] {
            let (status, headers, body) = get_from_peer(st.clone(), uri, lan).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            // The refusal is a page a human can act on, not a bare string.
            assert!(
                headers[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("text/html"),
                "{uri}"
            );
            assert!(body.contains("not served to the network"), "{uri}");
            assert!(
                body.contains("href=\"/obs\""),
                "{uri}: points at the remedy"
            );
            assert!(body.contains("/healthz"), "{uri}: names what is served");
            assert!(!body.contains("<script"), "{uri}: no script in a refusal");
        }
        // `/api/stats` is only routed with the feature on; without it the
        // gate never sees the request (404 is the router's answer).
        #[cfg(feature = "observability")]
        {
            let (status, _, _) = get_from_peer(st.clone(), "/api/stats", lan).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
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
        assert!(v["uptime_s"].as_f64().unwrap() >= 0.0);

        st.hub.stats.set_udp_bound(true);
        st.hub.publish(b"junk").unwrap_err();
        let (status, _, body) = get(st, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
    }

    /// The feed fields are what a second PC's operator (or a supervisor)
    /// reads to tell "the bridge is up" from "the capture agent is gone".
    #[cfg(feature = "observability")]
    #[tokio::test]
    async fn healthz_reports_the_feed_and_the_addresses() {
        let st = state_with(PathBuf::from("recordings"));
        let (_, _, body) = get(st.clone(), "/healthz").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["last_datagram_age_s"].is_null());
        assert_eq!(v["feed"], "never");
        assert_eq!(v["stalled"], false);
        assert_eq!(v["clients"], 0);
        assert_eq!(v["udp_addr"], TEST_UDP);
        assert_eq!(v["http_addr"], TEST_HTTP);
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));

        st.hub.publish(b"junk").unwrap_err();
        let (_, _, body) = get(st, "/healthz").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["last_datagram_age_s"].as_f64().unwrap() >= 0.0);
        assert_eq!(v["feed"], "live", "even a rejected datagram is a live feed");
    }

    /// Without `observability` the health check still answers, with the three
    /// fields anything watching this process depends on.
    #[cfg(not(feature = "observability"))]
    #[tokio::test]
    async fn healthz_stays_minimal_without_observability() {
        let (_, _, body) = get(state_with(PathBuf::from("recordings")), "/healthz").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v.as_object().unwrap().len(),
            3,
            "exactly ok, udp_bound and uptime_s"
        );
        assert_eq!(v["ok"], true);
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

    /// While the process is stopping, a page that reconnects is told to try
    /// again rather than handed a socket that is about to close.
    #[tokio::test]
    async fn a_stopping_server_refuses_new_websockets() {
        let st = state_with(PathBuf::from("recordings"));
        let headers = [
            ("host", "127.0.0.1:7879"),
            ("connection", "upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("origin", "http://127.0.0.1:7879"),
        ];
        let (status, _, _) = get_with(st.clone(), "/ws", &headers).await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "serving normally");

        st.shutdown.fire();
        let (status, _, body) = get_with(st, "/ws", &headers).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "stopping");
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

    /// A refused Host is usually somebody typing a machine name into OBS, so
    /// the answer says what this server does answer to and offers the URL
    /// that works — with the bound address in it.
    #[tokio::test]
    async fn the_host_refusal_explains_itself_and_suggests_a_url() {
        let st = state_with(PathBuf::from("recordings"));
        let (status, headers, body) =
            get_with(st, "/obs", &[("host", "my-gaming-pc.local:7879")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        // Security headers apply to refusals too.
        assert_eq!(headers["x-frame-options"], "DENY");
        assert!(body.contains("localhost"));
        assert!(body.contains("http://192.168.1.168:7879/obs"), "{body}");
        // The host it refused is echoed, escaped.
        assert!(body.contains("my-gaming-pc.local:7879"));
        let (_, _, body) = get_with(
            state_with(PathBuf::from("recordings")),
            "/",
            &[("host", "<script>alert(1)</script>")],
        )
        .await;
        assert!(!body.contains("<script>alert"), "the host is escaped");
    }

    #[test]
    fn the_url_hint_names_the_bound_address_or_says_it_cannot() {
        assert_eq!(
            overlay_url_hint("192.168.1.168:7879"),
            "<a href=\"http://192.168.1.168:7879/obs\">http://192.168.1.168:7879/obs</a>"
        );
        // A wildcard bind is not a destination; say so instead of printing
        // a URL nobody can open.
        let wild = overlay_url_hint("0.0.0.0:9000");
        assert!(wild.contains(":9000/obs"), "{wild}");
        assert!(wild.contains("this PC"), "{wild}");
        assert!(!wild.contains("0.0.0.0"), "{wild}");
        // An unreadable address still produces something actionable.
        let unknown = overlay_url_hint("");
        assert!(
            unknown.contains(&format!(":{DEFAULT_HTTP_PORT}/obs")),
            "{unknown}"
        );
        // Loopback stays loopback.
        assert!(overlay_url_hint("127.0.0.1:7879").contains("http://127.0.0.1:7879/obs"));
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

    #[cfg(feature = "observability")]
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
        assert_eq!(v["seq_gaps"], 0);
        assert_eq!(v["queue_depth"], 0);
        assert!(v["bytes_forwarded"].as_u64().unwrap() > 0);
        assert!(v["gap_p99_ms"].is_number());
    }

    /// Without `observability` the route does not exist at all — the page
    /// must not be able to make the bridge do this work.
    #[cfg(not(feature = "observability"))]
    #[tokio::test]
    async fn api_stats_is_absent_without_observability() {
        let (status, _, _) = get(state_with(PathBuf::from("recordings")), "/api/stats").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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
