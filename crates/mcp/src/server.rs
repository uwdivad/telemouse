//! The tools, and the one place that decides what a tool result looks like.
//!
//! Eleven tools in two groups. The **read-only** six answer from the
//! analyzer library and from `GET`s against ctl and viz. The **control**
//! five are thin proxies over ctl's HTTP API: this process never spawns a
//! child, never opens a handle on a process and never terminates anything
//! itself, so ctl's allow-lists — which flags a component accepts, which
//! pids its own scan would list — stay the only thing that decides what may
//! happen (`docs/ANTICHEAT-2026-09-14.md`). `--read-only` drops the second
//! group from the router entirely, so a client cannot call what it cannot
//! see.
//!
//! Every tool body returns `Result<Value, String>`: `Ok` is the JSON the
//! model gets, `Err` is a finished sentence it can act on. Neither ends the
//! session — a panel that is not running is a tool error, not a transport
//! error — which is the whole of "degraded environments never panic" here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::analysis::Analysis;
use crate::args;
use crate::digest;
use crate::local::{Local, LocalError};

/// The tools that only read.
pub const READ_TOOLS: &[&str] = &[
    "sessions_list",
    "session_summary",
    "trend",
    "health",
    "logs_tail",
    "live_stats",
];

/// The tools that make something happen, all of them through ctl.
/// `--read-only` removes exactly these.
pub const CONTROL_TOOLS: &[&str] = &["capture_start", "capture_stop", "marker", "doctor", "kill"];

/// How long `doctor` waits for the check to finish before answering with
/// whatever it printed so far.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(30);
/// How often `doctor` asks the panel whether the run is over.
const DOCTOR_POLL: Duration = Duration::from_millis(250);
/// Most of a log file this server will read to find its last lines.
const TAIL_WINDOW: u64 = 256 * 1024;

/// Everything a tool call needs, decided once at startup.
#[derive(Debug, Clone)]
pub struct Deps {
    pub analysis: Analysis,
    pub ctl: Local,
    pub viz: Local,
    /// `[ctl] log_dir`. `None` in a build without the `logging` feature,
    /// where there are no log files to tail.
    pub log_dir: Option<PathBuf>,
    /// Drop the control tools.
    pub read_only: bool,
}

/// The MCP server. Cloned per request by the SDK, so the state is shared.
#[derive(Clone)]
pub struct Telemouse {
    deps: Arc<Deps>,
    tool_router: ToolRouter<Telemouse>,
}

// --- results ---------------------------------------------------------------

/// A tool-level failure: the model sees the text and the `isError` flag, and
/// the session carries on.
fn failed(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

/// The one timing/outcome line this server logs, and the only place a tool
/// body's `Result<Value, String>` becomes an MCP result.
///
/// Without the `observability` feature there is nothing to time, so the
/// timer has no fields at all rather than fields nobody reads.
#[cfg(feature = "observability")]
struct Call {
    tool: &'static str,
    at: std::time::Instant,
}

#[cfg(not(feature = "observability"))]
struct Call;

impl Call {
    #[cfg(feature = "observability")]
    fn start(tool: &'static str) -> Self {
        Self {
            tool,
            at: std::time::Instant::now(),
        }
    }

    #[cfg(not(feature = "observability"))]
    fn start(_tool: &'static str) -> Self {
        Self
    }

    fn finish(self, out: Result<Value, String>) -> Result<CallToolResult, McpError> {
        let (result, _bytes, _outcome) = match out {
            Ok(value) => match serde_json::to_string_pretty(&value) {
                Ok(text) => {
                    let n = text.len();
                    (
                        CallToolResult::success(vec![ContentBlock::text(text)]),
                        n,
                        "ok",
                    )
                }
                Err(e) => {
                    let msg = format!("the result could not be serialised: {e}");
                    let n = msg.len();
                    (failed(msg), n, "error")
                }
            },
            Err(message) => {
                let n = message.len();
                (failed(message), n, "error")
            }
        };
        #[cfg(feature = "observability")]
        tracing::info!(
            tool = self.tool,
            ms = self.at.elapsed().as_secs_f64() * 1000.0,
            bytes = _bytes,
            outcome = _outcome,
            "tool call"
        );
        Ok(result)
    }
}

/// A `LocalError` as the sentence to hand the model, with the one hint the
/// error itself cannot know: a route that is missing because the peer was
/// built without the feature that serves it.
fn local_message(e: LocalError, missing_route_hint: &str) -> String {
    match &e {
        LocalError::Refused { status: 404, .. } if !missing_route_hint.is_empty() => {
            format!("{e} — {missing_route_hint}")
        }
        _ => e.to_string(),
    }
}

/// The hint for `/api/stats`, which only a full viz build serves.
const NO_STATS: &str =
    "this telemouse-viz was built without the `observability` feature, so it keeps no counters.";

// --- arguments -------------------------------------------------------------

/// `sessions_list`
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SessionsListArgs {
    /// How many recordings to return, newest first. 1-500, default 50.
    /// (`last` is accepted as another spelling; `limit` is preferred.)
    #[serde(default, alias = "last")]
    pub limit: Option<u32>,
}

/// `session_summary`
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SessionSummaryArgs {
    /// The recording id, as `sessions_list` returns it. A trailing `.jsonl`
    /// is accepted and ignored.
    pub id: String,
}

/// `trend`
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct TrendArgs {
    /// Extra columns, a list: dotted paths into the full report, such as
    /// `micro.band_ratio_8_12` or `quality.poll_hz`. (`metrics` is accepted
    /// as another spelling; `metric` is preferred.)
    #[serde(default, alias = "metrics")]
    pub metric: Option<Vec<String>>,
    /// Keep only the last N sessions (the rows are oldest first). 1-500.
    /// (`limit` is accepted as another spelling; `last` is preferred.)
    #[serde(default, alias = "limit")]
    pub last: Option<u32>,
}

/// `logs_tail`
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogsTailArgs {
    /// `ctl`, `capture`, `viz`, `mcp`, `analyze`, `doctor`, `trend` or
    /// `report`.
    pub component: String,
    /// How many trailing lines. 1-500, default 50.
    #[serde(default)]
    pub lines: Option<u32>,
}

/// `live_stats`
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct LiveStatsArgs {
    /// How long to sample, in seconds. 0-60, default 5; 0 takes a single
    /// reading instead of a rate.
    #[serde(default)]
    pub seconds: Option<u32>,
}

/// `capture_start`
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CaptureStartArgs {
    /// Save the recording, whatever `telemouse.toml` says. This is the
    /// panel's own switch and becomes `--record` / `--no-record`.
    #[serde(default)]
    pub save: Option<bool>,
    /// Flags for the capture agent. Only `--print`, `--no-kafka`,
    /// `--no-udp`, `--record` and `--no-record` are accepted.
    #[serde(default)]
    pub flags: Option<Vec<String>>,
}

/// `capture_stop`
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CaptureStopArgs {
    /// Terminate at once instead of sending Ctrl-Break and waiting for the
    /// grace period. A forced stop can cost the last partial batch.
    #[serde(default)]
    pub force: Option<bool>,
}

/// `marker`
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkerArgs {
    /// What this instant is called in the report: one line, at most 120
    /// characters ("trial 1 start").
    pub label: String,
}

/// `kill`
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KillArgs {
    /// The process id, from `health`. The panel refuses anything its own
    /// scan would not list, including itself.
    pub pid: i64,
}

// --- the tools -------------------------------------------------------------

#[tool_router]
impl Telemouse {
    /// Build the server. In read-only mode the control tools are removed
    /// from the router, so `tools/list` never mentions them and a call to
    /// one is an unknown tool.
    pub fn new(deps: Deps) -> Self {
        let mut tool_router = Self::tool_router();
        if deps.read_only {
            for name in CONTROL_TOOLS {
                tool_router.remove_route(name);
            }
        }
        tracing::debug!(
            read_only = deps.read_only,
            read_tools = READ_TOOLS.len(),
            control_tools = if deps.read_only {
                0
            } else {
                CONTROL_TOOLS.len()
            },
            "tool router built"
        );
        Self {
            deps: Arc::new(deps),
            tool_router,
        }
    }

    /// The tool names this server currently offers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    #[tool(
        description = "List the recorded telemouse sessions, newest first: id, start time, duration, event and ring-drop counts, the games seen, and — from the <id>.meta.json sidecar — how the run ended and what each sink failed to deliver. A header-only scan; it never reads the events."
    )]
    async fn sessions_list(
        &self,
        Parameters(a): Parameters<SessionsListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("sessions_list");
        let limit = args::list_limit(a.limit);
        let dir = self.deps.analysis.dir.display().to_string();
        call.finish(
            self.blocking(move |an| an.sessions(limit))
                .await
                .map(|(total, sessions)| {
                    json!({ "dir": dir, "total": total, "returned": sessions.len(), "sessions": sessions })
                }),
        )
    }

    #[tool(
        description = "The headline numbers of one recording (schema telemouse-report-summary/2, about 3 KB): data quality, flicks, micro-corrections, clicks, kinematics, lifts and the warnings. Read `warnings` and `quality.clean` first; if `session.aim_profile_missing` is true every degree-valued number uses a fallback sensitivity and is not comparable across sessions."
    )]
    async fn session_summary(
        &self,
        Parameters(a): Parameters<SessionSummaryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("session_summary");
        let id = a.id.trim().to_string();
        call.finish(self.blocking(move |an| {
            let summary = an.summary(&id)?;
            serde_json::to_value(summary).map_err(|e| {
                format!("session_summary: the summary of recording {id:?} could not be turned into JSON: {e}")
            })
        })
        .await)
    }

    #[tool(
        description = "One row per recorded session, oldest first, for comparing days: flicks per minute, overshoot and settle medians, tremor, path efficiency, clicks per minute, distance, and cm/360. Sessions are only comparable at the same cm_per_360. `metric` adds any dotted path into the full report as an extra column."
    )]
    async fn trend(
        &self,
        Parameters(a): Parameters<TrendArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("trend");
        let metrics = a.metric.unwrap_or_default();
        let last = args::last_rows(a.last);
        call.finish(
            self.blocking(move |an| an.trend(&metrics, last))
                .await
                .map(|rows| json!({ "rows": rows.len(), "sessions": rows })),
        )
    }

    #[tool(
        description = "Is telemouse healthy right now: the control panel's components (running, pid, last exit, the capture agent's live counters), every telemouse process on the machine, and the viz server's feed and bridge counters. Either server being down is reported, not an error."
    )]
    async fn health(&self) -> Result<CallToolResult, McpError> {
        let call = Call::start("health");
        call.finish(Ok(self.health_impl().await))
    }

    #[tool(
        description = "The last lines a telemouse component logged. Live output comes from the control panel's ring for a component it launched; otherwise the component's own log file in the log directory is read."
    )]
    async fn logs_tail(
        &self,
        Parameters(a): Parameters<LogsTailArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("logs_tail");
        call.finish(
            self.logs_impl(&a.component, args::tail_lines(a.lines))
                .await,
        )
    }

    #[tool(
        description = "Sample the viz server's counters over a few seconds and report rates: datagrams and forwarded frames per second, parse errors, lag drops, latency percentiles and sequence gaps. Needs telemouse-viz running with the observability feature."
    )]
    async fn live_stats(
        &self,
        Parameters(a): Parameters<LiveStatsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("live_stats");
        call.finish(self.live_impl(args::sample_seconds(a.seconds)).await)
    }

    #[tool(
        description = "Start the capture agent through the control panel. Only do this when the user asked for it. The agent names its own session (sessions_list shows it once it is written). Returns the new process id."
    )]
    async fn capture_start(
        &self,
        Parameters(a): Parameters<CaptureStartArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("capture_start");
        call.finish(self.capture_start_impl(a).await)
    }

    #[tool(
        description = "Stop the capture agent through the control panel: Ctrl-Break, then terminate after the panel's grace period. Exit code -1073741510 is a clean Ctrl-Break exit, not a failure."
    )]
    async fn capture_stop(
        &self,
        Parameters(a): Parameters<CaptureStopArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("capture_stop");
        let body = json!({ "force": a.force.unwrap_or(false) });
        call.finish(
            self.ctl_post("/api/components/capture/stop", &body)
                .await
                .map_err(|e| local_message(e, "")),
        )
    }

    #[tool(
        description = "Drop a labelled marker into the running recording, timestamped when it arrives. Markers are how an experiment is segmented: `report --split-by-marker` gives one sub-report per interval. A marker sent this way is a few milliseconds late, which is fine for trials and not for per-event alignment."
    )]
    async fn marker(
        &self,
        Parameters(a): Parameters<MarkerArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = Call::start("marker");
        let out = match args::marker_label(&a.label) {
            Ok(label) => self
                .ctl_post("/api/components/capture/marker", &json!({ "label": label }))
                .await
                .map_err(|e| local_message(e, "")),
            Err(e) => Err(e),
        };
        call.finish(out)
    }

    #[tool(
        description = "Run `telemouse doctor` through the control panel and return its telemouse-doctor/1 report under `report`: `verdict` (pass/warn/fail), one `checks` row per clock, monitor, device, UDP, recording and Kafka check (id, status, title, detail, hint), and the resolved `config`. Decide on `verdict`, not on the exit code. If the structured report cannot be had, `lines` carries the text report instead and `note` says why. Waits up to 30 seconds."
    )]
    async fn doctor(&self) -> Result<CallToolResult, McpError> {
        let call = Call::start("doctor");
        call.finish(self.doctor_impl().await)
    }

    #[tool(
        description = "Terminate a telemouse process by pid, through the control panel. The panel refuses any pid its own process scan would not list, and refuses itself. Only do this when the user names the process."
    )]
    async fn kill(&self, Parameters(a): Parameters<KillArgs>) -> Result<CallToolResult, McpError> {
        let call = Call::start("kill");
        let out = match args::pid(a.pid) {
            Ok(pid) => self
                .ctl_post(&format!("/api/processes/{pid}/kill"), &json!({}))
                .await
                .map_err(|e| local_message(e, "")),
            Err(e) => Err(e),
        };
        call.finish(out)
    }
}

// --- the work behind the tools ---------------------------------------------

impl Telemouse {
    /// Run an analyzer call on a blocking thread. A report over a long
    /// session is seconds of CPU; the stdio transport must keep answering.
    async fn blocking<T, F>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(Analysis) -> Result<T, String> + Send + 'static,
        T: Send + 'static,
    {
        let analysis = self.deps.analysis.clone();
        match tokio::task::spawn_blocking(move || f(analysis)).await {
            Ok(r) => r,
            Err(e) => Err(format!("the analyzer task did not finish: {e}")),
        }
    }

    /// A mutating call to ctl, with its guard header.
    async fn ctl_post(&self, path: &str, body: &Value) -> Result<Value, LocalError> {
        self.deps.ctl.post(path, body).await
    }

    async fn health_impl(&self) -> Value {
        let ctl = match self.deps.ctl.get("/api/state").await {
            Ok(v) => digest::ctl_health(&v),
            Err(e) => json!({ "reachable": false, "error": e.to_string() }),
        };
        // `/healthz` answers 503 with a JSON body while the UDP listener is
        // not bound: that is an answer about being unhealthy, not a refusal.
        let viz = match self.deps.viz.get_accepting("/healthz", &[503]).await {
            Ok(health) => {
                let stats = self.deps.viz.get("/api/stats").await;
                let mut v = digest::viz_health(&health, stats.as_ref().ok());
                if let (Some(obj), Err(e)) = (v.as_object_mut(), stats) {
                    obj.insert("stats_error".into(), json!(local_message(e, NO_STATS)));
                }
                v
            }
            Err(e) => json!({ "reachable": false, "error": e.to_string() }),
        };
        json!({ "ctl": ctl, "viz": viz })
    }

    async fn logs_impl(&self, component: &str, lines: usize) -> Result<Value, String> {
        let component = args::log_component(component)?;
        let mut tried = Vec::new();
        // The panel's ring is the live output of the child it launched —
        // including the one-shot tasks, which have no log file of their own.
        if args::RING_COMPONENTS.contains(&component) {
            match self.deps.ctl.get("/api/state").await {
                Ok(state) => match digest::component_log(&state, component, lines) {
                    Some(l) if !l.is_empty() => {
                        return Ok(json!({
                            "component": component,
                            "source": "the control panel's live output for this component",
                            "lines": l,
                        }));
                    }
                    _ => tried.push(format!(
                        "the control panel has no output for {component} in this run"
                    )),
                },
                Err(e) => tried.push(e.to_string()),
            }
        }
        let Some(dir) = self.deps.log_dir.as_deref() else {
            tried.push(
                "this telemouse-mcp was built without the `logging` feature, so it knows no log directory".into(),
            );
            return Err(tried.join("; "));
        };
        if !args::LOG_FILES.contains(&component) {
            tried.push(format!("{component} has no log file of its own"));
            return Err(tried.join("; "));
        }
        let path = dir.join(format!("{component}.log"));
        match tail_file(&path, lines) {
            Ok(l) => Ok(json!({
                "component": component,
                "source": path.display().to_string(),
                "lines": l,
            })),
            Err(e) => {
                tried.push(e);
                Err(tried.join("; "))
            }
        }
    }

    async fn live_impl(&self, seconds: u64) -> Result<Value, String> {
        let first = self
            .deps
            .viz
            .get("/api/stats")
            .await
            .map_err(|e| local_message(e, NO_STATS))?;
        if seconds == 0 {
            return Ok(json!({ "sampled_s": 0.0, "latest": digest::stats_digest(&first) }));
        }
        let at = std::time::Instant::now();
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        let second = self
            .deps
            .viz
            .get("/api/stats")
            .await
            .map_err(|e| local_message(e, NO_STATS))?;
        Ok(digest::stats_delta(
            &first,
            &second,
            at.elapsed().as_secs_f64(),
        ))
    }

    async fn capture_start_impl(&self, a: CaptureStartArgs) -> Result<Value, String> {
        let flags = args::capture_flags(&a.flags.unwrap_or_default())?;
        let mut body = serde_json::Map::new();
        if !flags.is_empty() {
            body.insert("flags".into(), json!(flags));
        }
        if let Some(save) = a.save {
            body.insert("save".into(), json!(save));
        }
        self.ctl_post("/api/components/capture/start", &Value::Object(body))
            .await
            .map_err(|e| local_message(e, ""))
    }

    /// `doctor`: the structured `telemouse-doctor/1` report when it can be
    /// had, the text report otherwise.
    ///
    /// The document comes back through the panel's output ring like any
    /// other child output, and `/api/state` hands out at most the last 120
    /// lines of it per component, so a pretty-printed document longer than
    /// that arrives without its head and does not parse. Rather than guess
    /// at a fragment, that case — and a panel too old to accept `--json` —
    /// runs doctor once more in text mode inside the same 30-second budget.
    async fn doctor_impl(&self) -> Result<Value, String> {
        let deadline = std::time::Instant::now() + DOCTOR_TIMEOUT;
        let why_text = match self.doctor_run(&["--json"], deadline).await {
            Ok(run) if !run.finished => return Ok(run.unfinished()),
            Ok(run) => match doctor_document(&run.lines) {
                Some(report) => {
                    return Ok(json!({
                        "finished": true,
                        "last_exit": run.last_exit,
                        "report": report,
                    }));
                }
                // Somebody else's run was in flight and we waited for it:
                // whatever it printed is the answer, not a reason to start
                // a second one.
                None if run.joined => {
                    return Ok(run.finished_text(
                        "a doctor run was already in progress; this is its output",
                    ));
                }
                None => {
                    "the telemouse-doctor/1 document did not come through the control panel's output intact (it keeps the last 120 lines per component); this is the text report instead"
                }
            },
            Err(DoctorError::Start(e @ LocalError::Refused { status: 400, .. })) => {
                tracing::debug!(error = %e, "ctl refused doctor --json; running the text report");
                "this control panel does not accept `doctor --json`; this is the text report"
            }
            Err(DoctorError::Start(e)) => return Err(local_message(e, "")),
            Err(DoctorError::Other(e)) => return Err(e),
        };
        match self.doctor_run(&[], deadline).await {
            Ok(run) if !run.finished => Ok(run.unfinished()),
            Ok(run) => Ok(run.finished_text(why_text)),
            Err(DoctorError::Start(e)) => Err(local_message(e, "")),
            Err(DoctorError::Other(e)) => Err(e),
        }
    }

    /// Start the doctor component with `flags` (or join the run already in
    /// flight), wait until it exits or `deadline` passes, and collect what
    /// that run printed.
    async fn doctor_run(
        &self,
        flags: &[&str],
        deadline: std::time::Instant,
    ) -> Result<DoctorRun, DoctorError> {
        let local = |e: LocalError| DoctorError::Other(local_message(e, ""));
        // Where this run's output will start in the ring, so earlier runs'
        // lines are not mistaken for this one's.
        let before = self.deps.ctl.get("/api/state").await.map_err(local)?;
        let cursor = digest::component(&before, "doctor")
            .ok_or_else(|| {
                DoctorError::Other("the control panel does not know a `doctor` component".into())
            })?
            .get("log_seq")
            .and_then(Value::as_u64);

        let body = if flags.is_empty() {
            json!({})
        } else {
            json!({ "flags": flags })
        };
        let joined = match self.ctl_post("/api/components/doctor/start", &body).await {
            Ok(_) => false,
            // Already running: there is a check in flight, so wait for it
            // rather than telling the caller to try again. Its output began
            // before the cursor, so read the whole ring instead.
            Err(LocalError::Refused { status: 409, .. }) => true,
            Err(e) => return Err(DoctorError::Start(e)),
        };
        let path = match cursor {
            Some(n) if !joined => format!("/api/state?log_since={n}"),
            _ => "/api/state".to_string(),
        };
        loop {
            tokio::time::sleep(DOCTOR_POLL).await;
            let state = self.deps.ctl.get(&path).await.map_err(local)?;
            let Some(c) = digest::component(&state, "doctor") else {
                return Err(DoctorError::Other(
                    "the control panel does not know a `doctor` component".into(),
                ));
            };
            let running = c.get("running").and_then(Value::as_bool).unwrap_or(false);
            if !running || std::time::Instant::now() >= deadline {
                return Ok(DoctorRun {
                    finished: !running,
                    joined,
                    last_exit: c.get("last_exit").cloned().unwrap_or(Value::Null),
                    lines: digest::component_log(&state, "doctor", usize::MAX).unwrap_or_default(),
                });
            }
        }
    }
}

/// What one doctor run left behind.
struct DoctorRun {
    finished: bool,
    /// The run was already in flight; we waited for it rather than starting
    /// it, so its flags are not known.
    joined: bool,
    last_exit: Value,
    lines: Vec<String>,
}

impl DoctorRun {
    fn finished_text(self, note: &str) -> Value {
        json!({
            "finished": true,
            "last_exit": self.last_exit,
            "note": note,
            "lines": self.lines,
        })
    }

    fn unfinished(self) -> Value {
        json!({
            "finished": false,
            "note": format!("doctor was still running after {}s; this is what it had printed", DOCTOR_TIMEOUT.as_secs()),
            "lines": self.lines,
        })
    }
}

/// Why [`Telemouse::doctor_run`] produced no run.
enum DoctorError {
    /// ctl refused to start it (not counting "already running").
    Start(LocalError),
    /// Anything else, already worded for the model.
    Other(String),
}

/// The `telemouse-doctor/1` document in a run's output, if it is all there.
///
/// The output is the panel's view of the child: its own `--- started ---`
/// and `--- exited ---` lines around whatever doctor printed. The document
/// runs from the first line that opens an object to the last that closes
/// one; a document missing its head starts at a nested `{` and does not
/// parse, which is the point — a fragment is never passed off as the report.
fn doctor_document(lines: &[String]) -> Option<Value> {
    let start = lines.iter().position(|l| l.trim_start().starts_with('{'))?;
    let end = lines.iter().rposition(|l| l.trim_end().ends_with('}'))?;
    if end < start {
        return None;
    }
    let doc: Value = serde_json::from_str(&lines[start..=end].join("\n")).ok()?;
    doc.get("schema")?
        .as_str()?
        .starts_with("telemouse-doctor/")
        .then_some(doc)
}

/// The last `lines` lines of `path`, reading only the tail of the file.
///
/// A rotated log is still megabytes; nothing here needs more than the end of
/// it, and a partial first line from the seek is dropped rather than shown.
fn tail_file(path: &Path, lines: usize) -> Result<Vec<String>, String> {
    use std::io::{Read, Seek, SeekFrom};

    let cannot = |e: std::io::Error| format!("cannot read {}: {e}", path.display());
    let mut file = std::fs::File::open(path).map_err(cannot)?;
    let len = file.metadata().map_err(cannot)?.len();
    let from = len.saturating_sub(TAIL_WINDOW);
    file.seek(SeekFrom::Start(from)).map_err(cannot)?;
    let mut buf = Vec::with_capacity(TAIL_WINDOW.min(len) as usize + 1);
    file.read_to_end(&mut buf).map_err(cannot)?;
    let text = String::from_utf8_lossy(&buf);
    let mut all: Vec<&str> = text.lines().collect();
    // The seek almost certainly landed mid-line; that fragment is not a line.
    if from > 0 && !all.is_empty() {
        all.remove(0);
    }
    let start = all.len().saturating_sub(lines);
    Ok(all[start..]
        .iter()
        .map(|l| telemouse_core::logging::strip_ansi(l).into_owned())
        .collect())
}

// --- the handler -----------------------------------------------------------

// `router = self.tool_router` is load-bearing: the default is
// `Self::tool_router()`, a *fresh* router, which would list and happily call
// the control tools that `--read-only` removed from this instance's.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Telemouse {
    fn get_info(&self) -> ServerConfig {
        let instructions = if self.deps.read_only {
            "telemouse: raw mouse telemetry for gaming. Read-only tools over recorded sessions and the running processes. \
             Start with `sessions_list`, then `session_summary(id)` for one session or `trend` to compare days; `health`, \
             `logs_tail` and `live_stats` say what is happening right now. Numbers are raw HID counts unless the session \
             has a CPI and a matching game profile. This server was started with --read-only, so it cannot start, mark or \
             stop anything."
        } else {
            "telemouse: raw mouse telemetry for gaming. Start with `sessions_list`, then `session_summary(id)` for one \
             session or `trend` to compare days; `health`, `logs_tail` and `live_stats` say what is happening right now. \
             `capture_start`, `marker`, `capture_stop`, `doctor` and `kill` go through the local control panel — only use \
             them when the user asked for it, and never `kill` a pid the user did not name. An experiment is: \
             capture_start(save=true), a marker at each trial boundary, capture_stop, then session_summary."
        };
        // Spelled out rather than `Implementation::from_build_env()`, which
        // reads the *SDK's* package name and would introduce this server as
        // "rmcp".
        let mut me = Implementation::default();
        me.name = env!("CARGO_PKG_NAME").into();
        me.version = env!("CARGO_PKG_VERSION").into();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(me)
            .with_instructions(instructions.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::net::SocketAddr;

    /// A stand-in for ctl or viz: it answers every request with one canned
    /// response and hands the request back so a test can assert on it.
    struct FakeServer {
        addr: SocketAddr,
        seen: tokio::sync::oneshot::Receiver<String>,
    }

    impl FakeServer {
        async fn answering(status: u16, body: &'static str) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, seen) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let request = read_request(&mut sock).await;
                let _ = tx.send(request);
                respond(&mut sock, status, body).await;
            });
            Self { addr, seen }
        }
    }

    /// Read one whole request: the headers, then as many body bytes as its
    /// `Content-Length` says — however the client's writes were split.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if seen.len() >= end + 4 + len {
                    break;
                }
            }
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    async fn respond(sock: &mut tokio::net::TcpStream, status: u16, body: &str) {
        use tokio::io::AsyncWriteExt;
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(body.as_bytes()).await;
        let _ = sock.shutdown().await;
    }

    /// A stand-in for ctl that answers any number of requests, each from
    /// `answer(request)`, and records every request it saw.
    struct FakeCtl {
        addr: SocketAddr,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeCtl {
        async fn start<F>(answer: F) -> Self
        where
            F: Fn(&str) -> (u16, String) + Send + Sync + 'static,
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = seen.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let request = read_request(&mut sock).await;
                    let (status, body) = answer(&request);
                    log.lock().unwrap().push(request);
                    respond(&mut sock, status, &body).await;
                }
            });
            Self { addr, seen }
        }

        fn requests(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    /// `/api/state` with only a doctor component in it.
    fn doctor_state(running: bool, log: &[&str]) -> String {
        json!({
            "components": [{
                "id": "doctor",
                "running": running,
                "last_exit": if running { Value::Null } else { json!({ "code": 0 }) },
                "log": log,
                "log_seq": 7,
            }]
        })
        .to_string()
    }

    /// A pretty-printed `telemouse-doctor/1` document as ctl's ring holds
    /// it: one line per line, between the panel's own markers.
    fn doctor_json_output() -> Vec<String> {
        let doc = json!({
            "schema": "telemouse-doctor/1",
            "capture_version": "0.0.0",
            "verdict": "warn",
            "checks": [
                { "id": "build", "status": "warn", "title": "Build", "detail": "debug" },
                { "id": "udp", "status": "pass", "title": "UDP sink", "detail": "ready" },
            ],
            "config": { "mouse_cpi": 1600.0 },
        });
        let mut out = vec!["--- started pid 9: telemouse.exe doctor --json ---".to_string()];
        out.extend(
            serde_json::to_string_pretty(&doc)
                .unwrap()
                .lines()
                .map(str::to_string),
        );
        out.push("--- exited: code 0 ---".into());
        out
    }

    /// An address nothing is listening on.
    async fn dead() -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    }

    fn deps(ctl: SocketAddr, viz: SocketAddr, read_only: bool) -> Deps {
        Deps {
            analysis: Analysis::new(PathBuf::from("recordings"), read_only),
            ctl: Local::ctl(ctl),
            viz: Local::viz(viz),
            log_dir: None,
            read_only,
        }
    }

    /// The text of a tool result, whatever kind it is.
    fn text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn value(r: &CallToolResult) -> Value {
        serde_json::from_str(&text(r)).expect("a successful tool result is JSON")
    }

    #[tokio::test]
    async fn the_full_router_has_every_tool_and_read_only_has_none_of_the_control_ones() {
        let (c, v) = (dead().await, dead().await);
        let full = Telemouse::new(deps(c, v, false)).tool_names();
        for name in READ_TOOLS.iter().chain(CONTROL_TOOLS) {
            assert!(full.contains(&(*name).to_string()), "{name} is missing");
        }
        assert_eq!(full.len(), READ_TOOLS.len() + CONTROL_TOOLS.len());

        let ro = Telemouse::new(deps(c, v, true)).tool_names();
        for name in READ_TOOLS {
            assert!(ro.contains(&(*name).to_string()), "{name} is missing");
        }
        for name in CONTROL_TOOLS {
            assert!(!ro.contains(&(*name).to_string()), "{name} must be gone");
        }
        assert_eq!(ro.len(), READ_TOOLS.len());
    }

    #[tokio::test]
    async fn every_tool_is_described_and_has_a_schema() {
        let (c, v) = (dead().await, dead().await);
        for tool in Telemouse::new(deps(c, v, false)).tool_router.list_all() {
            assert!(
                tool.description.as_deref().is_some_and(|d| d.len() > 40),
                "{} needs a description a model can choose on",
                tool.name
            );
            assert_eq!(
                tool.input_schema.get("type").and_then(Value::as_str),
                Some("object"),
                "{} has no object input schema",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn health_reports_both_servers_being_down_without_failing() {
        let (c, v) = (dead().await, dead().await);
        let out = Telemouse::new(deps(c, v, false)).health().await.unwrap();
        assert_ne!(out.is_error, Some(true), "a dead panel is an answer");
        let h = value(&out);
        assert_eq!(h["ctl"]["reachable"], false);
        assert_eq!(h["viz"]["reachable"], false);
        assert!(
            h["ctl"]["error"]
                .as_str()
                .unwrap()
                .contains("telemouse-ctl"),
            "{h:#}"
        );
        assert!(
            h["viz"]["error"]
                .as_str()
                .unwrap()
                .contains("telemouse-viz"),
            "{h:#}"
        );
    }

    #[tokio::test]
    async fn a_started_capture_returns_the_pid_the_panel_reports() {
        let ctl = FakeServer::answering(200, r#"{"ok":true,"pid":4321}"#).await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let out = s
            .capture_start(Parameters(CaptureStartArgs {
                save: Some(true),
                flags: Some(vec!["--no-kafka".into()]),
            }))
            .await
            .unwrap();
        assert_ne!(out.is_error, Some(true), "{}", text(&out));
        assert_eq!(value(&out)["pid"], 4321);

        // The request must be the one ctl accepts: the guard header, a
        // trusted Host, and only allow-listed flags.
        let request = ctl.seen.await.unwrap();
        assert!(request.starts_with("POST /api/components/capture/start"));
        assert!(request.contains("X-Telemouse-Ctl: 1"), "{request}");
        assert!(request.contains("Host: 127.0.0.1:"), "{request}");
        let body: Value = serde_json::from_str(request.rsplit("\r\n\r\n").next().unwrap()).unwrap();
        assert_eq!(body["save"], true);
        assert_eq!(body["flags"], json!(["--no-kafka"]));
        assert!(body.get("session").is_none());
    }

    #[tokio::test]
    async fn a_flag_the_panel_would_reject_never_reaches_it() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s
            .capture_start(Parameters(CaptureStartArgs {
                flags: Some(vec!["--duration-secs".into()]),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        let msg = text(&out);
        assert!(msg.contains("--no-kafka"), "{msg}");
        // It failed on the argument, not on the unreachable panel.
        assert!(!msg.contains("not answering"), "{msg}");
    }

    #[tokio::test]
    async fn the_panels_refusal_is_relayed_with_its_own_words() {
        let ctl = FakeServer::answering(409, r#"{"error":"capture is already running"}"#).await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let out = s
            .capture_start(Parameters(CaptureStartArgs::default()))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        let msg = text(&out);
        assert!(msg.contains("409"), "{msg}");
        assert!(msg.contains("capture is already running"), "{msg}");
    }

    #[tokio::test]
    async fn a_marker_is_validated_here_and_posted_as_the_panel_wants_it() {
        let ctl = FakeServer::answering(200, r#"{"ok":true,"label":"trial 1"}"#).await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let out = s
            .marker(Parameters(MarkerArgs {
                label: "  trial 1  ".into(),
            }))
            .await
            .unwrap();
        assert_ne!(out.is_error, Some(true), "{}", text(&out));
        let request = ctl.seen.await.unwrap();
        assert!(request.starts_with("POST /api/components/capture/marker"));
        assert!(request.ends_with(r#"{"label":"trial 1"}"#), "{request}");
    }

    #[tokio::test]
    async fn a_blank_marker_label_is_refused_before_the_panel_is_called() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s
            .marker(Parameters(MarkerArgs {
                label: " \t ".into(),
            }))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("needs a label"), "{}", text(&out));
    }

    #[tokio::test]
    async fn kill_refuses_a_pid_that_is_not_one_and_relays_the_panels_refusal_otherwise() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s.kill(Parameters(KillArgs { pid: 0 })).await.unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("not a process id"));

        let ctl = FakeServer::answering(403, r#"{"error":"that is the panel itself"}"#).await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let out = s.kill(Parameters(KillArgs { pid: 900 })).await.unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("that is the panel itself"));
        assert!(
            ctl.seen
                .await
                .unwrap()
                .starts_with("POST /api/processes/900/kill")
        );
    }

    #[tokio::test]
    async fn live_stats_explains_a_viz_built_without_the_counters() {
        let viz = FakeServer::answering(404, "not found").await;
        let s = Telemouse::new(deps(dead().await, viz.addr, false));
        let out = s
            .live_stats(Parameters(LiveStatsArgs { seconds: Some(0) }))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("observability"), "{}", text(&out));
    }

    #[tokio::test]
    async fn live_stats_with_zero_seconds_is_one_reading() {
        let viz = FakeServer::answering(
            200,
            r#"{"uptime_s":9.0,"datagrams":400,"forwarded":400,"parse_errors":0,"lag_drops":0,"clients":1,"latency":{"samples":10,"p50_us":800,"p99_us":2000,"max_us":3000,"mean_us":900,"negative":0},"seq_gaps":0}"#,
        )
        .await;
        let s = Telemouse::new(deps(dead().await, viz.addr, false));
        let out = s
            .live_stats(Parameters(LiveStatsArgs { seconds: Some(0) }))
            .await
            .unwrap();
        assert_ne!(out.is_error, Some(true), "{}", text(&out));
        let v = value(&out);
        assert_eq!(v["sampled_s"], 0.0);
        assert_eq!(v["latest"]["datagrams"], 400);
        assert!(v["latest"]["latency"].get("mean_us").is_none());
    }

    #[tokio::test]
    async fn logs_tail_names_an_unknown_component() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s
            .logs_tail(Parameters(LogsTailArgs {
                component: "kafka".into(),
                lines: None,
            }))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("capture"), "{}", text(&out));
    }

    #[tokio::test]
    async fn logs_tail_says_both_places_it_looked() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s
            .logs_tail(Parameters(LogsTailArgs {
                component: "capture".into(),
                lines: Some(5),
            }))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        let msg = text(&out);
        assert!(msg.contains("not answering"), "{msg}");
        assert!(msg.contains("logging"), "{msg}");
    }

    #[tokio::test]
    async fn a_session_id_that_could_name_a_path_is_refused() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s
            .session_summary(Parameters(SessionSummaryArgs {
                id: "../telemouse".into(),
            }))
            .await
            .unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("not a recording id"), "{}", text(&out));
    }

    #[test]
    fn the_other_spellings_of_count_and_metric_are_accepted() {
        let t: TrendArgs =
            serde_json::from_value(json!({ "metrics": ["quality.poll_hz"], "limit": 3 })).unwrap();
        assert_eq!(t.metric, Some(vec!["quality.poll_hz".to_string()]));
        assert_eq!(t.last, Some(3));
        let t: TrendArgs = serde_json::from_value(json!({ "metric": ["a"], "last": 2 })).unwrap();
        assert_eq!((t.metric.unwrap().len(), t.last), (1, Some(2)));
        let s: SessionsListArgs = serde_json::from_value(json!({ "last": 4 })).unwrap();
        assert_eq!(s.limit, Some(4));
        let s: SessionsListArgs = serde_json::from_value(json!({ "limit": 5 })).unwrap();
        assert_eq!(s.limit, Some(5));
    }

    #[test]
    fn a_capture_start_that_still_names_a_session_is_not_refused() {
        // `session` was dropped from the schema; an older client sending it
        // must still get its capture.
        let a: CaptureStartArgs =
            serde_json::from_value(json!({ "save": true, "session": "s-1" })).unwrap();
        assert_eq!(a.save, Some(true));
    }

    #[test]
    fn the_doctor_document_is_found_whole_or_not_at_all() {
        let whole = doctor_json_output();
        let doc = doctor_document(&whole).expect("the whole document parses");
        assert_eq!(doc["schema"], "telemouse-doctor/1");
        assert_eq!(doc["checks"][1]["id"], "udp");

        // What a 120-line ring keeps of a longer document: no head.
        let headless: Vec<String> = whole[6..].to_vec();
        assert!(doctor_document(&headless).is_none());

        // The same document on one line works too.
        let one_line = vec![
            "--- started ---".to_string(),
            serde_json::to_string(&doc).unwrap(),
            "--- exited: code 0 ---".into(),
        ];
        assert_eq!(doctor_document(&one_line), Some(doc));

        // Text output, or JSON that is not a doctor report, is not one.
        assert!(doctor_document(&["  pass  OS : Windows".to_string()]).is_none());
        assert!(doctor_document(&[r#"{"schema":"other/1"}"#.to_string()]).is_none());
        assert!(doctor_document(&[]).is_none());
    }

    #[tokio::test]
    async fn doctor_asks_for_json_and_returns_the_structured_report() {
        let output = doctor_json_output();
        let ctl = FakeCtl::start(move |req| {
            if req.starts_with("POST /api/components/doctor/start") {
                (200, r#"{"ok":true,"pid":9}"#.into())
            } else if req.starts_with("GET /api/state?log_since=7 ") {
                let lines: Vec<&str> = output.iter().map(String::as_str).collect();
                (200, doctor_state(false, &lines))
            } else {
                (200, doctor_state(false, &["an earlier run"]))
            }
        })
        .await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let out = s.doctor().await.unwrap();
        assert_ne!(out.is_error, Some(true), "{}", text(&out));
        let v = value(&out);
        assert_eq!(v["finished"], true);
        assert_eq!(v["report"]["schema"], "telemouse-doctor/1");
        assert_eq!(v["report"]["verdict"], "warn");
        assert!(v.get("lines").is_none(), "{v:#}");

        let posts: Vec<String> = ctl
            .requests()
            .into_iter()
            .filter(|r| r.starts_with("POST"))
            .collect();
        assert_eq!(posts.len(), 1, "one run: {posts:?}");
        assert!(posts[0].contains("X-Telemouse-Ctl: 1"), "{}", posts[0]);
        assert!(
            posts[0].ends_with(r#"{"flags":["--json"]}"#),
            "{}",
            posts[0]
        );
    }

    #[tokio::test]
    async fn doctor_falls_back_to_text_when_the_document_does_not_survive_the_ring() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let output = doctor_json_output();
        let posts = Arc::new(AtomicUsize::new(0));
        let counted = posts.clone();
        let ctl = FakeCtl::start(move |req| {
            if req.starts_with("POST /api/components/doctor/start") {
                counted.fetch_add(1, Ordering::SeqCst);
                (200, r#"{"ok":true,"pid":9}"#.into())
            } else if req.starts_with("GET /api/state?log_since=") {
                if counted.load(Ordering::SeqCst) == 1 {
                    // The ring kept only the tail of the document.
                    let lines: Vec<&str> = output[6..].iter().map(String::as_str).collect();
                    (200, doctor_state(false, &lines))
                } else {
                    (
                        200,
                        doctor_state(false, &["  pass  OS : Windows", "verdict: pass"]),
                    )
                }
            } else {
                (200, doctor_state(false, &[]))
            }
        })
        .await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let out = s.doctor().await.unwrap();
        assert_ne!(out.is_error, Some(true), "{}", text(&out));
        let v = value(&out);
        assert_eq!(v["finished"], true);
        assert!(v.get("report").is_none(), "{v:#}");
        assert_eq!(v["lines"][1], "verdict: pass");
        assert!(v["note"].as_str().unwrap().contains("120 lines"), "{v:#}");
        assert_eq!(posts.load(Ordering::SeqCst), 2);
        let last_post = ctl
            .requests()
            .into_iter()
            .rfind(|r| r.starts_with("POST"))
            .unwrap();
        assert!(last_post.ends_with("\r\n\r\n{}"), "{last_post}");
    }

    #[tokio::test]
    async fn doctor_runs_the_text_report_on_a_panel_that_refuses_json() {
        let ctl = FakeCtl::start(|req| {
            if req.starts_with("POST /api/components/doctor/start") {
                if req.contains("--json") {
                    (400, r#"{"error":"flag \"--json\" is not allowed"}"#.into())
                } else {
                    (200, r#"{"ok":true,"pid":9}"#.into())
                }
            } else {
                (200, doctor_state(false, &["verdict: pass"]))
            }
        })
        .await;
        let s = Telemouse::new(deps(ctl.addr, dead().await, false));
        let v = value(&s.doctor().await.unwrap());
        assert_eq!(v["lines"][0], "verdict: pass");
        assert!(v["note"].as_str().unwrap().contains("--json"), "{v:#}");
    }

    #[tokio::test]
    async fn doctor_with_the_panel_down_says_so() {
        let s = Telemouse::new(deps(dead().await, dead().await, false));
        let out = s.doctor().await.unwrap();
        assert_eq!(out.is_error, Some(true));
        assert!(text(&out).contains("not answering"), "{}", text(&out));
    }

    #[test]
    fn the_tail_of_a_log_file_is_its_last_lines_without_escape_noise() {
        let dir = std::env::temp_dir().join(format!("telemouse-mcp-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("capture.log");
        let mut text = String::new();
        for i in 0..1_000 {
            text.push_str(&format!("line \u{1b}[31m{i}\u{1b}[0m\n"));
        }
        std::fs::write(&path, &text).unwrap();
        let lines = tail_file(&path, 3).unwrap();
        assert_eq!(lines, ["line 997", "line 998", "line 999"]);
        assert_eq!(tail_file(&path, 10_000).unwrap().len(), 1_000);

        let missing = dir.join("nope.log");
        assert!(tail_file(&missing, 5).unwrap_err().contains("nope.log"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole thing, over a pipe: the handshake, the tool list and a
    /// call — the same three messages an MCP client sends, framed the same
    /// way, without a client library or a child process.
    #[tokio::test]
    async fn the_server_speaks_mcp_over_a_pipe() {
        use rmcp::ServiceExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (client, transport) = tokio::io::duplex(64 * 1024);
        let server = Telemouse::new(deps(dead().await, dead().await, true));
        let running = tokio::spawn(async move {
            let service = server.serve(transport).await.expect("serve over the pipe");
            let _ = service.waiting().await;
        });

        let (rx, mut tx) = tokio::io::split(client);
        let mut lines = BufReader::new(rx).lines();
        /// One newline-delimited JSON-RPC frame, the way the stdio
        /// transport frames them.
        macro_rules! send {
            ($line:literal) => {
                tx.write_all(concat!($line, "\n").as_bytes()).await.unwrap()
            };
        }

        send!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#
        );
        let init: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(init["id"], 1, "{init:#}");
        assert_eq!(init["result"]["serverInfo"]["name"], "telemouse-mcp");
        assert!(
            init["result"]["capabilities"]["tools"].is_object(),
            "{init:#}"
        );
        assert!(
            init["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("telemouse")
        );

        send!(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        send!(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
        let list: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .expect("a tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for name in READ_TOOLS {
            assert!(names.contains(name), "{name} missing from {names:?}");
        }
        for name in CONTROL_TOOLS {
            assert!(
                !names.contains(name),
                "{name} must not be offered read-only"
            );
        }
        assert!(
            list["result"]["tools"][0]["inputSchema"].is_object(),
            "{list:#}"
        );

        // A call that fails on its argument: an `isError` result, not a
        // JSON-RPC error and not a dropped session.
        send!(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"session_summary","arguments":{"id":"../etc"}}}"#
        );
        let call: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(call["id"], 3, "{call:#}");
        assert_eq!(call["result"]["isError"], true, "{call:#}");
        assert!(
            call["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not a recording id"),
            "{call:#}"
        );

        // A control tool is simply not there in this build.
        send!(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"kill","arguments":{"pid":1}}}"#
        );
        let refused: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert!(refused["error"].is_object(), "{refused:#}");

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
    }

    #[test]
    fn the_instructions_say_what_this_build_can_do() {
        let a: SocketAddr = "127.0.0.1:7880".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:7879".parse().unwrap();
        let full = Telemouse::new(deps(a, b, false)).get_info();
        let ro = Telemouse::new(deps(a, b, true)).get_info();
        assert!(
            full.instructions
                .as_ref()
                .unwrap()
                .contains("capture_start")
        );
        assert!(ro.instructions.as_ref().unwrap().contains("--read-only"));
        assert!(!ro.instructions.as_ref().unwrap().contains("capture_start"));
        assert!(full.capabilities.tools.is_some());
    }
}
