//! HTTP surface: the embedded page and the JSON API the page drives.
//!
//! Every mutating route is a `POST` that must carry `X-Telemouse-Ctl: 1`. A
//! browser will not attach a custom header to a cross-origin request without
//! a CORS preflight — which this server never answers — so a web page the user
//! happens to have open cannot stop the capture agent or kill a process by
//! poking `localhost`.
//!
//! That guard assumes the attacker's page is cross-origin. DNS rebinding makes
//! it same-origin: a name the attacker controls re-resolves to `127.0.0.1`,
//! and the page may then set any header it likes. The `Host` header still
//! carries the attacker's name, so every request — GET included, `/api/state`
//! returns child command lines and logs — is refused unless `Host` names this
//! machine (see [`telemouse_core::localhost`]).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use telemouse_core::localhost::{SECURITY_HEADERS, host_is_trusted};
use tracing::{info, warn};

use crate::manager::{Manager, MarkerError, StartError, StartRequest, StopError, SummaryError};
use crate::places::Places;
use crate::procs::{KillError, Scanner};
use crate::settings::{self, SaveError};

/// The single-file browser app. No external assets, no CDN.
pub const INDEX_HTML: &str = include_str!("index.html");

const CONFIG_PLACEHOLDER: &str = "/*__TELEMOUSE_CTL_CONFIG__*/";

/// Header every mutating request must carry.
pub const GUARD_HEADER: &str = "x-telemouse-ctl";

/// Log lines returned per component in `/api/state`.
const LOG_LINES: usize = 120;
/// How old a process scan may be and still be served. The page polls every
/// 2s; a scan is now ~100µs (see `procs.rs`), but the list does not change
/// at poll rate, so one scan per two polls is still plenty.
const SCAN_TTL: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<Manager>,
    pub scanner: Arc<Scanner>,
    pub page: Arc<String>,
    /// Version, config, logs and docs, as absolute strings the page shows.
    pub places: Arc<Places>,
    /// One settings save at a time: read, check the token, write.
    pub settings_lock: Arc<tokio::sync::Mutex<()>>,
}

/// What the page needs to know at load: links and limits, not secrets.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageConfig {
    pub viz_http: String,
    pub stop_grace_secs: u64,
    /// The features this panel was built with (`logging,observability`,
    /// `minimal`, ...): shown next to the version.
    pub features: String,
    pub places: Places,
    /// `marker_hotkey`: the page says "or press F9 in game" next to its
    /// marker field. Empty when none.
    pub marker_hotkey: String,
    /// `[ctl] hotkey`: the new-session chord, for the same hint.
    pub hotkey: String,
}

pub fn render_page(cfg: &PageConfig) -> String {
    let mut json = serde_json::to_string(cfg).unwrap_or_else(|_| "{}".into());
    json = json[1..json.len() - 1].replace('<', "\\u003c");
    INDEX_HTML.replace(CONFIG_PLACEHOLDER, &json)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/healthz", get(healthz))
        .route("/api/state", get(api_state))
        .route("/api/sessions", get(api_sessions))
        .route("/api/components/{id}/start", post(api_start))
        .route("/api/components/{id}/stop", post(api_stop))
        .route("/api/components/{id}/marker", post(api_marker))
        .route("/api/processes/{pid}/kill", post(api_kill))
        .route("/api/open", post(api_open))
        .route("/api/config", get(api_config).post(api_config_save))
        .route("/api/reports/{id}", get(api_report).post(api_report_run))
        .layer(middleware::from_fn(require_local_host))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Add [`SECURITY_HEADERS`] to every response. `X-Frame-Options: DENY`
/// matters most here: an `http://` page framing the panel could otherwise
/// position a one-click *Stop* under the user's cursor.
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
/// is what a DNS-rebinding page looks like from here. IP literals always pass.
async fn require_local_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_is_trusted(host) {
        warn!(host, path = %req.uri().path(), "refusing request with a non-local Host header");
        return error(StatusCode::FORBIDDEN, "host not allowed");
    }
    next.run(req).await
}

async fn index(State(st): State<AppState>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        st.page.as_str().to_owned(),
    )
        .into_response()
}

async fn healthz() -> &'static str {
    "ok"
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, axum::Json(json!({ "error": msg.to_string() }))).into_response()
}

/// The refusal to send back, if the request lacks the guard header.
fn guard(headers: &HeaderMap) -> Option<Response> {
    match headers.get(GUARD_HEADER) {
        Some(v) if v == "1" => None,
        _ => Some(error(
            StatusCode::FORBIDDEN,
            format!("mutating requests must carry the {GUARD_HEADER}: 1 header"),
        )),
    }
}

/// `?log_since=<n>`: only log lines newer than that cursor per component.
#[derive(Debug, Default, Deserialize)]
struct StateQuery {
    log_since: Option<u64>,
}

async fn api_state(
    State(st): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<StateQuery>,
) -> Response {
    let components = st.manager.snapshot_since(LOG_LINES, q.log_since).await;
    // The process table is blocking I/O (and on Windows, a fair amount of it).
    let scanner = st.scanner.clone();
    let processes = tokio::task::spawn_blocking(move || scanner.scan_cached(SCAN_TTL))
        .await
        .unwrap_or_default();
    axum::Json(json!({
        "self_pid": st.scanner.self_pid(),
        "now_unix_s": crate::manager::now_unix(),
        "version": crate::places::VERSION,
        "config": st.manager.config_info(),
        "places": &*st.places,
        "recording": st.manager.recording(),
        "components": components,
        "processes": processes,
    }))
    .into_response()
}

async fn api_sessions(State(st): State<AppState>) -> Response {
    let m = st.manager.clone();
    let list = tokio::task::spawn_blocking(move || m.list_sessions())
        .await
        .unwrap_or_default();
    axum::Json(list).into_response()
}

async fn api_start(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<axum::Json<StartRequest>>,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let req = body.map(|b| b.0).unwrap_or_default();
    match st.manager.start(&id, &req).await {
        Ok(pid) => {
            // The process table changed by our own hand: show it next poll.
            st.scanner.invalidate();
            axum::Json(json!({ "ok": true, "pid": pid })).into_response()
        }
        Err(e) => {
            let status = match e {
                StartError::UnknownComponent => StatusCode::NOT_FOUND,
                StartError::AlreadyRunning => StatusCode::CONFLICT,
                StartError::FlagNotAllowed(_)
                | StartError::SessionRequired
                | StartError::BadSession(_) => StatusCode::BAD_REQUEST,
                StartError::Spawn(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            warn!(component = %id, error = %e, "start refused");
            error(status, e)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct StopBody {
    #[serde(default)]
    force: bool,
}

async fn api_stop(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<axum::Json<StopBody>>,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let force = body.map(|b| b.0.force).unwrap_or(false);
    match st.manager.stop(&id, force).await {
        Ok(outcome) => {
            st.scanner.invalidate();
            axum::Json(json!({ "ok": true, "outcome": outcome })).into_response()
        }
        Err(StopError::UnknownComponent) => {
            error(StatusCode::NOT_FOUND, StopError::UnknownComponent)
        }
        Err(StopError::NotRunning) => error(StatusCode::CONFLICT, StopError::NotRunning),
    }
}

#[derive(Debug, Default, Deserialize)]
struct MarkerBody {
    #[serde(default)]
    label: String,
}

/// `POST /api/components/{id}/marker` `{ label }`: drop a labelled marker
/// into a running capture, the way the hotkey does — for a script or an
/// agent that wants to say "trial 3 starts here" in the recording.
async fn api_marker(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<axum::Json<MarkerBody>>,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let label = body.map(|b| b.0.label).unwrap_or_default();
    match st.manager.marker(&id, &label).await {
        Ok(label) => axum::Json(json!({ "ok": true, "label": label })).into_response(),
        Err(e) => {
            let status = match e {
                MarkerError::UnknownComponent => StatusCode::NOT_FOUND,
                MarkerError::Unsupported | MarkerError::BadLabel(_) => StatusCode::BAD_REQUEST,
                MarkerError::NotRunning => StatusCode::CONFLICT,
                MarkerError::Write(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            warn!(component = %id, error = %e, "marker refused");
            error(status, e)
        }
    }
}

async fn api_kill(
    State(st): State<AppState>,
    Path(pid): Path<u32>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let scanner = st.scanner.clone();
    let result = tokio::task::spawn_blocking(move || scanner.kill(pid)).await;
    match result {
        Ok(Ok(info)) => {
            info!(pid, name = %info.name, kind = ?info.kind, "killed");
            axum::Json(json!({ "ok": true, "killed": info })).into_response()
        }
        Ok(Err(e)) => {
            let status = match e {
                KillError::NotFound => StatusCode::NOT_FOUND,
                KillError::IsSelf | KillError::NotRelated => StatusCode::FORBIDDEN,
                KillError::Failed => StatusCode::INTERNAL_SERVER_ERROR,
            };
            warn!(pid, error = %e, "kill refused");
            error(status, e)
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct OpenBody {
    #[serde(default)]
    target: String,
}

/// `POST /api/open` `{ target }`: open one of the places the panel talks
/// about with the shell's default handler — the config in an editor, the
/// recordings or logs folder in Explorer, the docs. The target is a name;
/// the path comes from this server, never from the request.
async fn api_open(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Option<axum::Json<OpenBody>>,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let target = body.map(|b| b.0.target).unwrap_or_default();
    let path = match target.as_str() {
        "config" => st.places.config.clone(),
        "recordings" => st.manager.recording().dir,
        "logs" => st.places.logs.clone(),
        "docs" => st.places.docs.clone(),
        other => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("unknown target {other:?}: one of config, recordings, logs, docs"),
            );
        }
    };
    if path.is_empty() {
        return error(
            StatusCode::NOT_FOUND,
            format!("nothing to open for {target}: this build has no such place"),
        );
    }
    let shown = path.clone();
    let opened = tokio::task::spawn_blocking(move || crate::gui::open_url(&path))
        .await
        .unwrap_or(false);
    if opened {
        info!(target, path = %shown, "opened from the page");
        axum::Json(json!({ "ok": true, "target": target, "path": shown })).into_response()
    } else {
        warn!(target, path = %shown, "the shell refused to open it");
        error(StatusCode::INTERNAL_SERVER_ERROR, "could not open it")
    }
}

/// `GET /api/config`: the settings the page may edit, as the file on disk
/// has them now, and the token a save must echo. A file that cannot be used
/// comes back with `settings: null` and the reason.
async fn api_config(State(st): State<AppState>) -> Response {
    let path = st.manager.config_path().to_path_buf();
    let shown = path.display().to_string();
    let view = tokio::task::spawn_blocking(move || settings::view(&path)).await;
    let Ok(view) = view else {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read the settings",
        );
    };
    axum::Json(json!({
        "path": shown,
        "status": st.manager.config_info().status,
        "exists": view.exists,
        "token": view.token,
        "settings": view.settings,
        "error": view.error,
        "choices": settings::CHOICES,
        "not_editable": settings::NOT_EDITABLE,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSaveBody {
    /// The `token` of the `GET /api/config` the edit was made against.
    token: String,
    patch: settings::Patch,
}

/// `POST /api/config` `{ token, patch }`: change allow-listed settings in
/// `telemouse.toml`, keeping the rest of the file as the user wrote it.
/// `400` (nothing written) when the result would not be a valid config,
/// `409` when the file changed since `token` was handed out.
async fn api_config_save(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Result<axum::Json<ConfigSaveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    let body = match body {
        Ok(b) => b.0,
        // Unknown fields land here: the patch shape is the allow-list.
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let _one = st.settings_lock.lock().await;
    let path = st.manager.config_path().to_path_buf();
    let saved =
        tokio::task::spawn_blocking(move || settings::save(&path, &body.token, &body.patch)).await;
    let saved = match saved {
        Ok(Ok(s)) => s,
        Ok(Err(SaveError::Stale(token))) => {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "error": "telemouse.toml was changed by something else since this page read it; your edit was not saved. The form now shows the file as it is.",
                    "token": token,
                })),
            )
                .into_response();
        }
        Ok(Err(SaveError::Invalid(why))) => {
            warn!(error = %why, "settings not saved: invalid");
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": why.reason, "field": why.field })),
            )
                .into_response();
        }
        Ok(Err(SaveError::Io(why))) => {
            warn!(error = %why, "settings not saved");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not write the settings: {why}"),
            );
        }
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // Whether recordings are saved and where is in force from here on; the
    // rest belongs to processes that read the file when they start.
    st.manager.reload_config();
    let changed = match &saved.old {
        Some(old) => settings::changed(old, &saved.new),
        None => Vec::new(),
    };
    let effects = settings::effects(
        &changed,
        st.manager.is_running("capture").await,
        st.manager.is_running("viz").await,
    );
    info!(changed = ?changed, created = saved.created, "settings saved from the page");
    axum::Json(json!({
        "ok": true,
        "token": saved.token,
        "created": saved.created,
        "settings": settings::Settings::from(&saved.new),
        "changed": changed,
        "restart_required": effects.restart_required,
        "next_start": effects.next_start,
    }))
    .into_response()
}

fn summary_error(e: SummaryError) -> Response {
    let status = match e {
        SummaryError::BadId => StatusCode::BAD_REQUEST,
        SummaryError::NoRecording => StatusCode::NOT_FOUND,
        SummaryError::NoAnalyzer => StatusCode::SERVICE_UNAVAILABLE,
        SummaryError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error(status, e)
}

/// `GET /api/reports/{id}`: the stored `ReportSummary` of a recording, if
/// one was made since the recording last changed. Reads a file; `404` when
/// there is none yet (`POST` makes it).
async fn api_report(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let m = st.manager.clone();
    match tokio::task::spawn_blocking(move || m.stored_summary(&id)).await {
        Ok(Ok(Some(v))) => axum::Json(v).into_response(),
        Ok(Ok(None)) => error(StatusCode::NOT_FOUND, "no summary yet"),
        Ok(Err(e)) => summary_error(e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// `POST /api/reports/{id}`: run the analyzer in summary mode over that
/// recording and return the `ReportSummary` JSON it prints.
async fn api_report_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = guard(&headers) {
        return r;
    }
    match st.manager.report_summary(&id).await {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => summary_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use std::path::PathBuf;
    use std::time::Duration;
    use tower::ServiceExt;

    fn state() -> AppState {
        AppState {
            manager: Arc::new(Manager::new(
                crate::manager::COMPONENTS,
                crate::manager::ManagerConfig {
                    // An empty directory: every binary is "not found", so start
                    // can only fail at spawn — never launch a real agent in tests.
                    bin_dir: Some(std::env::temp_dir().join("telemouse-ctl-no-bins")),
                    config_path: PathBuf::from("telemouse.toml"),
                    recordings_dir: PathBuf::from("recordings"),
                    recording_enabled: true,
                    config_status: crate::manager::ConfigStatus::Defaults,
                    grace: Duration::from_secs(1),
                    log_dir: None,
                },
            )),
            scanner: Arc::new(Scanner::new()),
            page: Arc::new(render_page(&PageConfig {
                viz_http: "127.0.0.1:7879".into(),
                stop_grace_secs: 5,
                features: "test".into(),
                places: Places {
                    version: "0.0.0-test".into(),
                    panel_url: "http://127.0.0.1:7880/".into(),
                    ..Default::default()
                },
                marker_hotkey: "f9".into(),
                hotkey: "ctrl+alt+r".into(),
            })),
            places: Arc::new(Places::default()),
            settings_lock: Arc::default(),
        }
    }

    #[tokio::test]
    async fn open_needs_the_guard_a_known_target_and_a_place() {
        let body = Some(serde_json::json!({ "target": "logs" }));
        let (status, _, _) = call(state(), Method::POST, "/api/open", false, body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, v, _) = call(
            state(),
            Method::POST,
            "/api/open",
            true,
            Some(serde_json::json!({ "target": "C:\\Windows\\System32\\cmd.exe" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        // Places::default(): no logs folder in this build → nothing opens.
        let (status, v, _) = call(state(), Method::POST, "/api/open", true, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    }

    async fn call(
        st: AppState,
        method: Method,
        uri: &str,
        guarded: bool,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:7880");
        if guarded {
            req = req.header(GUARD_HEADER, "1");
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = router(st).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 16 * 1024 * 1024).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        (status, json, text)
    }

    #[tokio::test]
    async fn page_is_self_contained_and_configured() {
        let (status, _, html) = call(state(), Method::GET, "/", false, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("<html"));
        assert!(!html.contains(CONFIG_PLACEHOLDER));
        assert!(html.contains("\"viz_http\":\"127.0.0.1:7879\""));
        for forbidden in [
            "src=\"http",
            "href=\"http",
            "//cdn.",
            "unpkg.com",
            "jsdelivr",
        ] {
            assert!(
                !INDEX_HTML.contains(forbidden),
                "external asset: {forbidden}"
            );
        }
        assert_eq!(INDEX_HTML.matches(CONFIG_PLACEHOLDER).count(), 1);
        // The page must send the guard header, or nothing it does will work.
        assert!(INDEX_HTML.contains("X-Telemouse-Ctl"));
        // Light and dark, a first-run banner, and the shell-open route.
        for needed in [
            "data-theme",
            "prefers-color-scheme",
            "/api/open",
            "id=\"firstRun\"",
            "/api/components/capture/marker",
            // The settings editor, the report card and the first-run guide.
            "/api/config",
            "/api/reports/",
            "id=\"settingsCard\"",
            "id=\"reportCard\"",
            "id=\"wizStep2\"",
            "foreground_seen",
        ] {
            assert!(INDEX_HTML.contains(needed), "page lacks {needed}");
        }
        // Nobody is sent to a TOML editor on first run any more, and nobody
        // is ever told to elevate.
        let first_run = INDEX_HTML
            .split("id=\"firstRun\"")
            .nth(1)
            .and_then(|s| s.split("</section>").next())
            .expect("the first-run section");
        assert!(!first_run.contains("restart the panel"));
        assert!(!first_run.contains("[games]"));
        assert!(!INDEX_HTML.to_lowercase().contains("as administrator"));
    }

    #[test]
    fn page_config_cannot_break_out_of_the_script_tag() {
        let html = render_page(&PageConfig {
            viz_http: "</script><script>alert(1)</script>".into(),
            stop_grace_secs: 1,
            features: "</script>".into(),
            places: Places {
                docs: "</script><script>alert(2)</script>".into(),
                ..Default::default()
            },
            marker_hotkey: "</script><script>alert(3)</script>".into(),
            hotkey: String::new(),
        });
        assert!(!html.contains("</script><script>alert"));
    }

    #[tokio::test]
    async fn state_has_the_documented_shape() {
        let (status, v, _) = call(state(), Method::GET, "/api/state", false, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["self_pid"], std::process::id());
        assert!(v["now_unix_s"].as_u64().unwrap() > 1_700_000_000);
        let comps = v["components"].as_array().unwrap();
        assert_eq!(comps.len(), crate::manager::COMPONENTS.len());
        let cap = &comps[0];
        assert_eq!(cap["id"], "capture");
        assert_eq!(cap["kind"], "service");
        assert_eq!(cap["running"], false);
        assert_eq!(cap["bin_found"], false);
        assert!(
            cap["flags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["flag"] == "--no-kafka")
        );
        assert!(
            cap["flags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["flag"] == "--record")
        );
        assert_eq!(cap["saving"], false);
        assert_eq!(cap["exits"], 0);
        assert_eq!(cap["unexpected_exits"], 0);
        assert!(cap["args"].is_array());
        assert_eq!(v["recording"]["enabled"], true);
        assert_eq!(v["recording"]["dir"], "recordings");
        assert!(v["processes"].is_array());
        for p in v["processes"].as_array().unwrap() {
            assert!(p["pid"].is_number());
            assert!(p["kind"].is_string());
        }
    }

    /// A DNS-rebound page is same-origin and can set the guard header; the
    /// `Host` it sends is the only thing that gives it away.
    #[tokio::test]
    async fn rebound_host_names_are_refused_even_with_the_guard_header() {
        for (uri, method) in [
            ("/", Method::GET),
            ("/api/state", Method::GET),
            ("/api/components/capture/stop", Method::POST),
            ("/api/processes/1/kill", Method::POST),
            ("/api/config", Method::GET),
            ("/api/config", Method::POST),
            ("/api/reports/s-1", Method::GET),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header(header::HOST, "evil.com")
                .header(GUARD_HEADER, "1")
                .body(Body::empty())
                .unwrap();
            let res = router(state()).oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{uri}");
        }
        let req = Request::builder()
            .uri("/healthz")
            .header(header::HOST, "localhost:7880")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router(state()).oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn mutations_require_the_guard_header() {
        for uri in [
            "/api/components/capture/start",
            "/api/components/capture/stop",
            "/api/processes/1/kill",
            "/api/config",
            "/api/reports/s-1",
        ] {
            let (status, v, _) = call(state(), Method::POST, uri, false, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap()
                    .contains("X-Telemouse-Ctl".to_lowercase().as_str())
            );
        }
        // GETs are open: the page has to load before it can send anything.
        let (status, _, _) = call(state(), Method::GET, "/api/sessions", false, None).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// `save` is accepted for capture (and fails only at spawn, since the
    /// test bin dir is empty) and refused as a disallowed flag elsewhere.
    #[tokio::test]
    async fn save_switch_is_a_request_field() {
        let st = state();
        let (s, v, _) = call(
            st.clone(),
            Method::POST,
            "/api/components/capture/start",
            true,
            Some(serde_json::json!({ "save": false })),
        )
        .await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{v}");
        assert!(v["error"].as_str().unwrap().contains("--no-record"), "{v}");
        let (s, v, _) = call(
            st,
            Method::POST,
            "/api/components/viz/start",
            true,
            Some(serde_json::json!({ "save": false })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"].as_str().unwrap().contains("--no-record"), "{v}");
    }

    #[tokio::test]
    async fn start_and_stop_map_manager_errors_to_statuses() {
        let st = state();
        let (s, _, _) = call(
            st.clone(),
            Method::POST,
            "/api/components/nope/start",
            true,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, v, _) = call(
            st.clone(),
            Method::POST,
            "/api/components/capture/start",
            true,
            Some(json!({ "flags": ["--rm-rf"] })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("--rm-rf"));
        let (s, _, _) = call(
            st.clone(),
            Method::POST,
            "/api/components/report/start",
            true,
            Some(json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // Binary directory is empty: the spawn itself fails.
        let (s, v, _) = call(
            st.clone(),
            Method::POST,
            "/api/components/viz/start",
            true,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(v["error"].as_str().unwrap().contains("could not start"));

        let (s, _, _) = call(
            st.clone(),
            Method::POST,
            "/api/components/viz/stop",
            true,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, _) = call(st, Method::POST, "/api/components/nope/stop", true, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn markers_map_manager_errors_to_statuses() {
        let st = state();
        let marker = |st: AppState, id: &'static str, guarded: bool, body| async move {
            call(
                st,
                Method::POST,
                &format!("/api/components/{id}/marker"),
                guarded,
                body,
            )
            .await
        };
        let (s, _, _) = marker(st.clone(), "capture", false, Some(json!({ "label": "x" }))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "guard header required");
        let (s, _, _) = marker(st.clone(), "nope", true, Some(json!({ "label": "x" }))).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, v, _) = marker(st.clone(), "viz", true, Some(json!({ "label": "x" }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("does not take markers")
        );
        let (s, v, _) = marker(st.clone(), "capture", true, Some(json!({ "label": "  " }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("blank"));
        let (s, v, _) = marker(st.clone(), "capture", true, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "no body is an empty label");
        assert!(v["error"].as_str().unwrap().contains("blank"));
        // Nothing is running in tests (no binaries), so a good label is 409.
        let (s, v, _) = marker(st, "capture", true, Some(json!({ "label": "round 1" }))).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(v["error"], "not running");
    }

    #[tokio::test]
    async fn kill_refuses_self_unrelated_and_unknown_pids() {
        let st = state();
        let me = std::process::id();
        let (s, v, _) = call(
            st.clone(),
            Method::POST,
            &format!("/api/processes/{me}/kill"),
            true,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(v["error"].as_str().unwrap().contains("itself"));

        let system_pid = if cfg!(windows) { 4 } else { 1 };
        let (s, _, _) = call(
            st.clone(),
            Method::POST,
            &format!("/api/processes/{system_pid}/kill"),
            true,
            None,
        )
        .await;
        assert!(
            matches!(s, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
            "{s}"
        );

        let (s, _, _) = call(
            st,
            Method::POST,
            "/api/processes/4294967200/kill",
            true,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    /// A state whose config and recordings live in a throwaway directory.
    fn state_in(dir: &std::path::Path) -> AppState {
        let mut st = state();
        st.manager = Arc::new(Manager::new(
            crate::manager::COMPONENTS,
            crate::manager::ManagerConfig {
                bin_dir: Some(std::env::temp_dir().join("telemouse-ctl-no-bins")),
                config_path: dir.join("telemouse.toml"),
                recordings_dir: dir.join("recordings"),
                recording_enabled: true,
                config_status: crate::manager::ConfigStatus::Defaults,
                grace: Duration::from_secs(1),
                log_dir: None,
            },
        ));
        st
    }

    const USER_FILE: &str = "# mine\nmouse_cpi = 1600.0  # the white one\n\n[recording]\nenabled = true\ndir = \"recordings\"\n\n# main game\n[games.\"one.exe\"]\nsens = 1.0\nyaw_coeff = 0.022\npitch_coeff = 0.022\n";

    async fn get_config(st: &AppState) -> serde_json::Value {
        let (s, v, _) = call(st.clone(), Method::GET, "/api/config", false, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    #[tokio::test]
    async fn config_is_read_with_a_token_and_saved_keeping_comments() {
        let d = crate::manager::tmpdir("srv-config");
        let p = d.join("telemouse.toml");
        std::fs::write(&p, USER_FILE).unwrap();
        let st = state_in(&d);

        let v = get_config(&st).await;
        assert_eq!(v["exists"], true);
        assert_eq!(v["settings"]["mouse_cpi"], 1600.0);
        assert_eq!(v["settings"]["recording"]["dir"], "recordings");
        assert_eq!(v["settings"]["games"]["one.exe"]["sens"], 1.0);
        assert_eq!(v["settings"]["ctl"]["hotkey"], "ctrl+alt+r");
        assert!(v["choices"]["obs_layouts"].as_array().unwrap().len() >= 4);
        assert!(v["settings"].get("kafka").is_none(), "{v}");
        let token = v["token"].as_str().unwrap().to_string();

        // No guard header: refused before anything is read.
        let body = json!({ "token": token, "patch": { "mouse_cpi": 800 } });
        let (s, _, _) = call(
            st.clone(),
            Method::POST,
            "/api/config",
            false,
            Some(body.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), USER_FILE);

        let (s, v, _) = call(st.clone(), Method::POST, "/api/config", true, Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["settings"]["mouse_cpi"], 800.0);
        assert_eq!(v["changed"], json!(["mouse_cpi"]));
        assert_eq!(v["restart_required"], json!([]));
        assert_eq!(v["next_start"], json!([]), "nothing is running");
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            text,
            USER_FILE.replace("1600.0", "800.0"),
            "only the value moved"
        );
        assert_eq!(get_config(&st).await["token"], v["token"]);

        // The old token is now stale: 409, the file untouched, the new token offered.
        let (s, v2, _) = call(
            st.clone(),
            Method::POST,
            "/api/config",
            true,
            Some(json!({ "token": token, "patch": { "mouse_cpi": 400 } })),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{v2}");
        assert_eq!(v2["token"], v["token"]);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), text);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn an_invalid_or_disallowed_save_writes_nothing() {
        let d = crate::manager::tmpdir("srv-config-bad");
        let p = d.join("telemouse.toml");
        std::fs::write(&p, USER_FILE).unwrap();
        let st = state_in(&d);
        let token = get_config(&st).await["token"].clone();

        for (patch, field) in [
            (json!({ "mouse_cpi": -1 }), "mouse_cpi"),
            (json!({ "marker_hotkey": "ctrl+alt+r" }), "ctl.hotkey"),
            (json!({ "ctl": { "hotkey": "banana" } }), "ctl.hotkey"),
            (
                json!({ "games": { "C:\\x\\game.exe": { "sens": 1 } } }),
                "games",
            ),
            (json!({ "games": { "new.exe": { "sens": 0 } } }), "games"),
            (json!({ "obs": { "layout": "sideways" } }), "viz.obs.layout"),
        ] {
            let (s, v, _) = call(
                st.clone(),
                Method::POST,
                "/api/config",
                true,
                Some(json!({ "token": token, "patch": patch })),
            )
            .await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{patch} → {v}");
            assert_eq!(v["field"], field, "{patch} → {v}");
            assert!(!v["error"].as_str().unwrap().is_empty());
            assert_eq!(std::fs::read_to_string(&p).unwrap(), USER_FILE, "{patch}");
        }
        // What the form may not touch is not a patch at all.
        for patch in [
            json!({ "ctl": { "http_addr": "0.0.0.0:7880" } }),
            json!({ "ctl": { "bin_dir": "C:/elsewhere" } }),
            json!({ "kafka": { "enabled": true } }),
            json!({ "viz": { "http_addr": "0.0.0.0:7879" } }),
        ] {
            let (s, v, _) = call(
                st.clone(),
                Method::POST,
                "/api/config",
                true,
                Some(json!({ "token": token, "patch": patch })),
            )
            .await;
            assert!(s.is_client_error(), "{patch} → {s} {v}");
            assert_eq!(std::fs::read_to_string(&p).unwrap(), USER_FILE, "{patch}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn games_are_added_and_removed_through_the_route() {
        let d = crate::manager::tmpdir("srv-config-games");
        let p = d.join("telemouse.toml");
        std::fs::write(&p, USER_FILE).unwrap();
        let st = state_in(&d);
        let token = get_config(&st).await["token"].clone();
        let (s, v, _) = call(
            st.clone(),
            Method::POST,
            "/api/config",
            true,
            Some(json!({ "token": token, "patch": { "games": {
                "My Game": { "sens": 2.5, "yaw_coeff": 0.0066 },
                "one.exe": null,
            }}})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["changed"], json!(["games"]));
        let games = v["settings"]["games"].as_object().unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(games["my game.exe"]["pitch_coeff"], 0.0066);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("[games.\"my game.exe\"]"), "{text}");
        assert!(!text.contains("one.exe"), "{text}");
        assert!(text.starts_with("# mine\nmouse_cpi = 1600.0  # the white one\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn the_first_save_creates_the_file_from_the_sample() {
        let d = crate::manager::tmpdir("srv-config-seed");
        let p = d.join("telemouse.toml");
        let st = state_in(&d);
        let v = get_config(&st).await;
        assert_eq!(v["exists"], false);
        assert_eq!(v["status"], "defaults");
        assert_eq!(v["token"], "none");
        let (s, v, _) = call(
            st.clone(),
            Method::POST,
            "/api/config",
            true,
            Some(json!({ "token": "none", "patch": { "mouse_cpi": 3200, "recording": { "enabled": false } } })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["created"], true);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# telemouse configuration"), "{text}");
        assert!(text.contains("mouse_cpi = 3200.0"));
        // The panel's own copy follows at once.
        let (_, state, _) = call(st.clone(), Method::GET, "/api/state", false, None).await;
        assert_eq!(state["recording"]["enabled"], false);
        assert_eq!(state["config"]["status"], "loaded");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn report_summaries_are_by_safe_id_only() {
        let d = crate::manager::tmpdir("srv-reports");
        std::fs::create_dir_all(d.join("recordings")).unwrap();
        std::fs::write(d.join("recordings").join("s-1.jsonl"), "{}\n").unwrap();
        let st = state_in(&d);

        let (s, _, _) = call(st.clone(), Method::POST, "/api/reports/s-1", false, None).await;
        assert_eq!(
            s,
            StatusCode::FORBIDDEN,
            "running the analyzer needs the guard"
        );
        for bad in ["..%5Cx", "C:s-1", "s%201", ".hidden", "s-1.jsonl"] {
            for method in [Method::GET, Method::POST] {
                let (s, v, _) = call(
                    st.clone(),
                    method,
                    &format!("/api/reports/{bad}"),
                    true,
                    None,
                )
                .await;
                assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} → {v}");
            }
        }
        let (s, _, _) = call(st.clone(), Method::POST, "/api/reports/nope", true, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // The test bin dir is empty: no analyzer, said plainly, no panic.
        let (s, v, _) = call(st.clone(), Method::POST, "/api/reports/s-1", true, None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert!(v["error"].as_str().unwrap().contains("telemouse-analyze"));

        // Nothing stored yet; then a stored summary newer than the recording is served.
        let (s, _, _) = call(st.clone(), Method::GET, "/api/reports/s-1", false, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let cache = d.join("recordings").join(crate::manager::REPORTS_DIR);
        std::fs::create_dir_all(&cache).unwrap();
        // A summary in a shape this panel does not know is not served: the
        // recording will never change again, so only the schema tag can
        // retire a cache written before the analyzer grew a field.
        std::fs::write(
            cache.join("s-1.summary.json"),
            r#"{"schema":"telemouse-report-summary/1"}"#,
        )
        .unwrap();
        let (s, _, _) = call(st.clone(), Method::GET, "/api/reports/s-1", false, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "a stale summary shape is ignored");
        std::fs::write(
            cache.join("s-1.summary.json"),
            format!(
                r#"{{"schema":"{}","markers":[]}}"#,
                crate::manager::SUMMARY_SCHEMA
            ),
        )
        .unwrap();
        let (s, v, _) = call(st.clone(), Method::GET, "/api/reports/s-1", false, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["schema"], crate::manager::SUMMARY_SCHEMA);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (s, _, body) = call(state(), Method::GET, "/healthz", false, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        for (uri, host) in [("/", "127.0.0.1:7880"), ("/", "evil.com")] {
            let req = Request::builder()
                .uri(uri)
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap();
            let res = router(state()).oneshot(req).await.unwrap();
            assert_eq!(res.headers()["x-frame-options"], "DENY", "{host}");
            assert_eq!(res.headers()["x-content-type-options"], "nosniff", "{host}");
        }
    }
}
