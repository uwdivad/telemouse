//! HTTP + WebSocket surface: the embedded page, the replay REST endpoints, and
//! the `/ws` live fan-out.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use crate::hub::Hub;
use crate::recordings;
use crate::stats::StatsPayload;

/// The single-file browser app. No external assets, no CDN — everything the
/// page needs is inlined so the viz works on a machine with no internet.
pub const INDEX_HTML: &str = include_str!("index.html");

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub recordings_dir: PathBuf,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_upgrade))
        .route("/api/stats", get(api_stats))
        .route("/api/sessions", get(api_sessions))
        .route("/api/session/{id}", get(api_session))
        .with_state(state)
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

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX_HTML,
    )
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

async fn ws_upgrade(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| client_loop(socket, st))
}

/// One shared `Arc<str>` becomes one WebSocket text frame per client. The hub
/// side of the fan-out is a refcount bump; only this last hop copies, and only
/// into the socket's own buffer.
fn text_frame(frame: &crate::hub::Frame) -> Message {
    Message::Text(axum::extract::ws::Utf8Bytes::from(&**frame))
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
        }
    }

    async fn get(state: AppState, uri: &str) -> (StatusCode, HeaderMap, String) {
        let res = router(state)
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), 64 * 1024 * 1024).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).to_string())
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
        let state = AppState {
            hub: Arc::new(Hub::new()),
            recordings_dir: PathBuf::from("recordings"),
        };
        let _ = router(state);
    }
}
