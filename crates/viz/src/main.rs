//! `telemouse-viz` — the live visualization + replay server (plan phases 3–4).
//!
//! Two jobs, one process:
//!
//! * bridge localhost UDP envelopes from the capture agent to browser
//!   WebSockets, forwarding the JSON unchanged (raw counts on the wire; the
//!   browser derives cm and aim-space degrees), and
//! * serve the single-file page that renders them, plus the REST endpoints
//!   replay mode reads recorded `.jsonl` sessions from.
//!
//! Neither half depends on the other: with no capture agent running, replay
//! still works; with no `recordings/` directory, live still works.

mod hub;
mod recordings;
mod server;
mod stats;
mod udp;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use telemouse_core::config::AppConfig;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::hub::Hub;
use crate::server::AppState;

/// How often the `viz_stats` frame is pushed to connected pages.
const PUSH_INTERVAL: Duration = Duration::from_secs(1);
/// How many pushes make up one logged observability line (5s, per conventions).
const LOG_EVERY_PUSHES: u32 = 5;

#[derive(Parser, Debug)]
#[command(name = "telemouse-viz", about = "Live mouse-telemetry visualization and replay server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the UDP→WebSocket bridge and the HTTP viz server.
    Serve(ServeArgs),
}

#[derive(Parser, Debug, Default)]
struct ServeArgs {
    /// Path to telemouse.toml (defaults are used if it does not exist).
    #[arg(long, default_value = "telemouse.toml")]
    config: PathBuf,
    /// Override `udp.addr`: where capture-agent envelopes arrive.
    #[arg(long, value_name = "ADDR")]
    udp: Option<String>,
    /// Override `viz.http_addr`: HTTP + WebSocket listen address.
    #[arg(long, value_name = "ADDR")]
    http: Option<String>,
    /// Override `recording.dir`: where replay looks for `*.jsonl`.
    #[arg(long, value_name = "DIR")]
    recordings: Option<PathBuf>,
}

/// Two workers, not one-per-core. This process owns exactly one UDP socket, one
/// HTTP listener and a handful of WebSockets; the default runtime spawned a
/// worker (and a stack, and a share of every work-stealing scan) per core to
/// idle. Two keeps the ingest loop and a blocking-ish HTTP handler from ever
/// queueing behind each other, and costs nothing when idle. (A single-thread
/// runtime was measured at 1kHz and made no difference: the bridge's ~0.45%
/// is socket syscalls, not scheduling.)
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    let args = match cli.command {
        Some(Command::Serve(a)) => a,
        // No subcommand: serving is the only thing this binary does.
        None => ServeArgs {
            config: PathBuf::from("telemouse.toml"),
            ..Default::default()
        },
    };
    serve(args).await
}

async fn serve(args: ServeArgs) -> Result<()> {
    let cfg = match AppConfig::load_or_default(&args.config) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path = %args.config.display(), "config unreadable; using defaults");
            AppConfig::default()
        }
    };

    let udp_addr_s = args.udp.unwrap_or(cfg.udp.addr);
    let http_addr_s = args.http.unwrap_or(cfg.viz.http_addr);
    let recordings_dir = args.recordings.unwrap_or(cfg.recording.dir);

    let http_addr: SocketAddr = http_addr_s
        .parse()
        .with_context(|| format!("invalid --http address {http_addr_s:?}"))?;

    let hub = Arc::new(Hub::new());

    // UDP ingest. An unbindable port is a degraded environment, not a fatal
    // one: replay mode and the page itself keep working.
    match udp_addr_s.parse::<SocketAddr>() {
        Ok(udp_addr) => {
            let hub = hub.clone();
            tokio::spawn(async move {
                if let Err(e) = udp::listen(udp_addr, hub).await {
                    warn!(error = %e, %udp_addr, "udp listener unavailable; live mode will stay idle");
                }
            });
        }
        Err(e) => warn!(error = %e, addr = %udp_addr_s, "invalid udp address; live mode disabled"),
    }

    tokio::spawn(stats_reporter(hub.clone()));

    let state = AppState {
        hub: hub.clone(),
        recordings_dir: recordings_dir.clone(),
        pages: Arc::new(server::Pages::render(&cfg.viz.obs)),
    };
    let app = server::router(state);

    let listener = NoDelayListener::new(
        tokio::net::TcpListener::bind(http_addr)
            .await
            .with_context(|| format!("failed to bind http {http_addr}"))?,
    );

    info!(
        http = %http_addr,
        udp = %udp_addr_s,
        recordings = %recordings_dir.display(),
        obs_layout = %cfg.viz.obs.layout,
        "telemouse-viz serving; dashboard http://{http_addr}/ — OBS browser source http://{http_addr}/obs"
    );

    axum::serve(listener, app).await.context("http server failed")?;
    Ok(())
}

/// [`tokio::net::TcpListener`] wrapper that sets `TCP_NODELAY` on every
/// accepted connection. Neither tokio, hyper, nor axum 0.8 sets it (axum 0.8
/// removed `Serve::tcp_nodelay`), and this server's WebSocket traffic — one
/// small frame every ~25ms, one direction — is the worst case for Nagle plus
/// delayed ACK: each frame can sit in the kernel waiting for an ACK timer.
/// A connection whose socket refuses the option still works (just with worse
/// latency), so failure is a once-per-process warning, never an error.
struct NoDelayListener {
    inner: tokio::net::TcpListener,
    warned: bool,
}

impl NoDelayListener {
    fn new(inner: tokio::net::TcpListener) -> Self {
        Self { inner, warned: false }
    }
}

impl axum::serve::Listener for NoDelayListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Delegate to the TcpListener impl: it owns the retry-on-accept-error
        // loop; this wrapper only tunes the stream it hands back.
        let (stream, addr) = axum::serve::Listener::accept(&mut self.inner).await;
        if let Err(e) = stream.set_nodelay(true)
            && !self.warned
        {
            self.warned = true;
            warn!(error = %e, "could not set TCP_NODELAY on an accepted connection; ws frames may be delayed by Nagle");
        }
        (stream, addr)
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Metrics are logs here — plus, now, a push.
///
/// Every [`PUSH_INTERVAL`] this computes the interval rate and broadcasts a
/// `viz_stats` frame so the page can show bridge health (datagram rate, parse
/// errors, lag drops, bridge latency) without polling. Every
/// [`LOG_EVERY_PUSHES`] pushes it also emits the 5s `info` line and rolls the
/// latency window that both the log line and `/api/stats` report.
async fn stats_reporter(hub: Arc<Hub>) {
    let mut prev = hub.stats.snapshot();
    let mut last = Instant::now();
    let mut log_prev = prev;
    let mut log_last = last;
    let mut pushes: u32 = 0;

    let mut ticker = tokio::time::interval(PUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await; // fires immediately; skip the zero-length interval
    loop {
        ticker.tick().await;
        let now = Instant::now();
        let secs = now.duration_since(last).as_secs_f64();
        last = now;
        let cur = hub.stats.snapshot();
        let d = cur.delta(&prev, secs);
        prev = cur;
        hub.stats.set_datagrams_per_s(d.datagrams_per_s);

        // Push first: the page's readout should be live even while the log
        // stays quiet on an idle bridge.
        match serde_json::to_string(&server::stats_payload(&hub)) {
            // `From<String>` for `Utf8Bytes` reuses the String's allocation.
            Ok(json) => hub.broadcast(json.into()),
            Err(e) => warn!(error = %e, "could not serialize viz_stats frame"),
        }

        pushes += 1;
        if pushes < LOG_EVERY_PUSHES {
            continue;
        }
        pushes = 0;

        let log_secs = now.duration_since(log_last).as_secs_f64();
        log_last = now;
        let ld = cur.delta(&log_prev, log_secs);
        log_prev = cur;
        let lat = hub.stats.roll_latency();

        if ld.datagrams == 0 && ld.parse_errors == 0 && cur.clients == 0 {
            // Idle and unobserved: stay quiet rather than spamming the log.
            continue;
        }
        info!(
            datagrams_per_s = format_args!("{:.1}", ld.datagrams_per_s),
            forwarded = ld.forwarded,
            parse_errors = ld.parse_errors,
            parse_errors_total = cur.parse_errors,
            ws_clients = cur.clients,
            session_cached = hub.cached_session().is_some(),
            lag_drops = ld.lag_drops,
            lag_disconnects = ld.lag_disconnects,
            lat_p50_ms = format_args!("{:.1}", lat.p50_us as f64 / 1000.0),
            lat_p99_ms = format_args!("{:.1}", lat.p99_us as f64 / 1000.0),
            lat_max_ms = format_args!("{:.1}", lat.max_us as f64 / 1000.0),
            lat_samples = lat.samples,
            lat_negative = lat.negative,
            "viz stats"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_flags_parse() {
        let cli = Cli::parse_from([
            "telemouse-viz",
            "serve",
            "--config",
            "custom.toml",
            "--udp",
            "127.0.0.1:9000",
            "--http",
            "127.0.0.1:9001",
        ]);
        let Some(Command::Serve(a)) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(a.config, PathBuf::from("custom.toml"));
        assert_eq!(a.udp.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(a.http.as_deref(), Some("127.0.0.1:9001"));
    }

    #[test]
    fn no_subcommand_defaults_to_serve() {
        let cli = Cli::parse_from(["telemouse-viz"]);
        assert!(cli.command.is_none());
    }

    #[tokio::test]
    async fn accepted_connections_have_nodelay_set() {
        use axum::serve::Listener as _;
        let inner = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = inner.local_addr().unwrap();
        let mut listener = NoDelayListener::new(inner);
        assert_eq!(listener.local_addr().unwrap(), addr);

        let (accepted, client) =
            tokio::join!(listener.accept(), tokio::net::TcpStream::connect(addr));
        let _client = client.unwrap();
        let (stream, peer) = accepted;
        assert_eq!(peer.ip(), addr.ip());
        assert!(stream.nodelay().unwrap(), "accepted stream must have TCP_NODELAY");
    }
}
