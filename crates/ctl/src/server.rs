//! HTTP surface: the embedded page and the JSON API the page drives.
//!
//! Every mutating route is a `POST` that must carry `X-Telemouse-Ctl: 1`. A
//! browser will not attach a custom header to a cross-origin request without
//! a CORS preflight — which this server never answers — so a web page the user
//! happens to have open cannot stop the capture agent or kill a process by
//! poking `localhost`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::manager::{Manager, StartError, StartRequest, StopError};
use crate::procs::{KillError, Scanner};

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
}

/// What the page needs to know at load: links and limits, not secrets.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PageConfig {
    pub viz_http: String,
    pub stop_grace_secs: u64,
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
        .route("/api/processes/{pid}/kill", post(api_kill))
        .with_state(state)
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

fn guard(headers: &HeaderMap) -> Result<(), Response> {
    match headers.get(GUARD_HEADER) {
        Some(v) if v == "1" => Ok(()),
        _ => Err(error(
            StatusCode::FORBIDDEN,
            format!("mutating requests must carry the {GUARD_HEADER}: 1 header"),
        )),
    }
}

async fn api_state(State(st): State<AppState>) -> Response {
    let components = st.manager.snapshot(LOG_LINES).await;
    // The process table is blocking I/O (and on Windows, a fair amount of it).
    let scanner = st.scanner.clone();
    let processes = tokio::task::spawn_blocking(move || scanner.scan_cached(SCAN_TTL))
        .await
        .unwrap_or_default();
    axum::Json(json!({
        "self_pid": st.scanner.self_pid(),
        "now_unix_s": crate::manager::now_unix(),
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
    if let Err(r) = guard(&headers) {
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
    if let Err(r) = guard(&headers) {
        return r;
    }
    let force = body.map(|b| b.0.force).unwrap_or(false);
    match st.manager.stop(&id, force).await {
        Ok(outcome) => {
            st.scanner.invalidate();
            axum::Json(json!({ "ok": true, "outcome": outcome })).into_response()
        }
        Err(StopError::UnknownComponent) => error(StatusCode::NOT_FOUND, StopError::UnknownComponent),
        Err(StopError::NotRunning) => error(StatusCode::CONFLICT, StopError::NotRunning),
    }
}

async fn api_kill(State(st): State<AppState>, Path(pid): Path<u32>, headers: HeaderMap) -> Response {
    if let Err(r) = guard(&headers) {
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
                // An empty directory: every binary is "not found", so start
                // can only fail at spawn — never launch a real agent in tests.
                Some(std::env::temp_dir().join("telemouse-ctl-no-bins")),
                PathBuf::from("telemouse.toml"),
                PathBuf::from("recordings"),
                Duration::from_secs(1),
            )),
            scanner: Arc::new(Scanner::new()),
            page: Arc::new(render_page(&PageConfig {
                viz_http: "127.0.0.1:7879".into(),
                stop_grace_secs: 5,
            })),
        }
    }

    async fn call(
        st: AppState,
        method: Method,
        uri: &str,
        guarded: bool,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value, String) {
        let mut req = Request::builder().method(method).uri(uri);
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
        for forbidden in ["src=\"http", "href=\"http", "//cdn.", "unpkg.com", "jsdelivr"] {
            assert!(!INDEX_HTML.contains(forbidden), "external asset: {forbidden}");
        }
        assert_eq!(INDEX_HTML.matches(CONFIG_PLACEHOLDER).count(), 1);
        // The page must send the guard header, or nothing it does will work.
        assert!(INDEX_HTML.contains("X-Telemouse-Ctl"));
    }

    #[test]
    fn page_config_cannot_break_out_of_the_script_tag() {
        let html = render_page(&PageConfig {
            viz_http: "</script><script>alert(1)</script>".into(),
            stop_grace_secs: 1,
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
        assert!(cap["flags"].as_array().unwrap().iter().any(|f| f["flag"] == "--no-kafka"));
        assert!(v["processes"].is_array());
        for p in v["processes"].as_array().unwrap() {
            assert!(p["pid"].is_number());
            assert!(p["kind"].is_string());
        }
    }

    #[tokio::test]
    async fn mutations_require_the_guard_header() {
        for uri in [
            "/api/components/capture/start",
            "/api/components/capture/stop",
            "/api/processes/1/kill",
        ] {
            let (status, v, _) = call(state(), Method::POST, uri, false, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert!(v["error"].as_str().unwrap().contains("X-Telemouse-Ctl".to_lowercase().as_str()));
        }
        // GETs are open: the page has to load before it can send anything.
        let (status, _, _) = call(state(), Method::GET, "/api/sessions", false, None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn start_and_stop_map_manager_errors_to_statuses() {
        let st = state();
        let (s, _, _) = call(st.clone(), Method::POST, "/api/components/nope/start", true, None).await;
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
        let (s, v, _) = call(st.clone(), Method::POST, "/api/components/viz/start", true, None).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(v["error"].as_str().unwrap().contains("could not start"));

        let (s, _, _) = call(st.clone(), Method::POST, "/api/components/viz/stop", true, None).await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _, _) = call(st, Method::POST, "/api/components/nope/stop", true, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn kill_refuses_self_unrelated_and_unknown_pids() {
        let st = state();
        let me = std::process::id();
        let (s, v, _) = call(st.clone(), Method::POST, &format!("/api/processes/{me}/kill"), true, None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(v["error"].as_str().unwrap().contains("itself"));

        let system_pid = if cfg!(windows) { 4 } else { 1 };
        let (s, _, _) = call(st.clone(), Method::POST, &format!("/api/processes/{system_pid}/kill"), true, None).await;
        assert!(matches!(s, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{s}");

        let (s, _, _) = call(st, Method::POST, "/api/processes/4294967200/kill", true, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (s, _, body) = call(state(), Method::GET, "/healthz", false, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, "ok");
    }
}
