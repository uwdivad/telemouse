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
mod shutdown;
mod stats;
mod udp;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use telemouse_core::config::AppConfig;
use tracing::{info, warn};

use crate::hub::Hub;
use crate::server::AppState;
use crate::shutdown::Shutdown;

#[cfg(feature = "observability")]
use std::time::Instant;

/// How often the `viz_stats` frame is pushed to connected pages.
#[cfg(feature = "observability")]
const PUSH_INTERVAL: Duration = Duration::from_secs(1);
/// How many pushes make up one logged observability line (5s, per conventions).
#[cfg(feature = "observability")]
const LOG_EVERY_PUSHES: u32 = 5;
/// How many 5 s report intervals pass before an idle bridge says so anyway.
/// A log that goes completely silent is indistinguishable from a process that
/// died, so one line a minute is the heartbeat that tells them apart.
#[cfg(feature = "observability")]
const HEARTBEAT_EVERY: u32 = 12;

/// What this build can do, for the startup line — a support answer that would
/// otherwise need the operator to know which zip they unpacked. A feature that
/// is off is listed with a `-`, so the line says what is missing as well as
/// what is there.
const FEATURES: &[&str] = &[
    if cfg!(feature = "logging") {
        "logging"
    } else {
        "-logging"
    },
    if cfg!(feature = "observability") {
        "observability"
    } else {
        "-observability"
    },
    if cfg!(feature = "quiet") {
        "quiet"
    } else {
        "-quiet"
    },
];

/// How long open connections get to finish after a stop is asked for.
///
/// The bridge owns no sinks — there is nothing to flush — so a stop is only
/// the accept loop closing and every socket saying goodbye, which takes
/// milliseconds. What this bounds is the connection that will *not* end on
/// its own: a half-sent request, a keep-alive socket a browser is holding, a
/// replay body still streaming a 400 MB recording. Waiting for those is how a
/// graceful stop turns into `telemouse-ctl` terminating the process after its
/// whole grace period (`ctl.stop_grace_secs`, 8 s by default, 3 s when
/// Windows is the one waiting), so the deadline sits far below both.
const SHUTDOWN_DEADLINE: Duration = Duration::from_millis(750);

/// How long a terminal console event (window close, logoff, shutdown) is held
/// while this process tidies up. Windows takes the process the moment the
/// handler returns, so this has to outlast [`SHUTDOWN_DEADLINE`] — otherwise
/// the handler releases while the server is still closing sockets.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(1500);

#[derive(Parser, Debug)]
#[command(
    name = "telemouse-viz",
    version,
    about = "Live mouse-telemetry visualization and replay server"
)]
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
    /// Also write `<DIR>/viz.log` (rotated). Unset = stderr only.
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
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
    let cli = Cli::parse();
    let args = match cli.command {
        Some(Command::Serve(a)) => a,
        // No subcommand: serving is the only thing this binary does.
        None => ServeArgs {
            config: PathBuf::from("telemouse.toml"),
            ..Default::default()
        },
    };

    #[cfg(feature = "logging")]
    let log = telemouse_core::logging::init(telemouse_core::logging::LogOptions {
        component: "viz",
        log_dir: args.log_dir.as_deref(),
        default_filter: "info",
    });
    telemouse_core::panic_hook::install("viz");

    // The control panel stops this process with a Ctrl-Break, and a handler
    // that reports the event as handled is also a handler that has taken
    // responsibility for leaving: Windows' default "terminate now" no longer
    // runs. So the handler fires the signal and the server below acts on it.
    let stop = Shutdown::new();
    let guard = match telemouse_core::shutdown::install(
        {
            let stop = stop.clone();
            move |signal| {
                info!(
                    %signal,
                    blocking = signal.is_terminal(),
                    "console control event; stopping"
                );
                stop.fire();
            }
        },
        SHUTDOWN_GRACE,
    ) {
        Ok(guard) => Some(guard),
        Err(e) => {
            warn!(error = %e, "could not install the console control handler");
            None
        }
    };

    #[cfg(feature = "logging")]
    if let Some(err) = &log.file_error {
        warn!(error = %err, "could not open the log file; logging to stderr only");
    }
    #[cfg(feature = "logging")]
    let log_file = log.file;
    #[cfg(not(feature = "logging"))]
    let log_file: Option<PathBuf> = None;

    let result = serve(args, log_file, stop).await;
    // Release a handler still blocked on a close, a logoff or a system
    // shutdown: the sockets are closed and Windows may have the process.
    if let Some(guard) = guard {
        guard.finished();
    }
    result
}

async fn serve(args: ServeArgs, log_file: Option<PathBuf>, stop: Shutdown) -> Result<()> {
    // Relative paths inside the config are relative to the config file, and a
    // config named but not found beside the working directory is looked for
    // beside the executable (see `telemouse_core::paths`).
    let config_path = telemouse_core::paths::locate_config(&args.config);
    let config_found = config_path.exists();
    // A config that exists but does not parse, or carries a value the
    // pipeline cannot use, is a refusal to start: running on defaults after
    // an operator edited the file is the failure mode where the overlay comes
    // up on the wrong port and nobody knows why.
    let mut cfg = AppConfig::load_or_default(&config_path)
        .with_context(|| format!("refusing to start on {}", config_path.display()))?;
    // As the file spells them, before relative paths are made absolute.
    let overrides = cfg.non_default_fields();
    cfg.resolve_paths(&telemouse_core::paths::config_base(&config_path));

    let udp_addr_s = args.udp.unwrap_or(cfg.udp.addr.clone());
    let http_addr_s = args.http.unwrap_or(cfg.viz.http_addr.clone());
    let recordings_dir = args.recordings.unwrap_or(cfg.recording.dir.clone());

    let http_addr: SocketAddr = http_addr_s
        .parse()
        .with_context(|| format!("invalid --http address {http_addr_s:?}"))?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        features = %FEATURES.join(","),
        config = %config_path.display(),
        config_found,
        log_file = log_file.as_ref().map(|p| p.display().to_string()),
        http_addr = %http_addr_s,
        udp_addr = %udp_addr_s,
        recordings = %recordings_dir.display(),
        overrides = ?overrides,
        "telemouse-viz starting"
    );
    if !http_addr.ip().is_loopback() {
        // There is no authentication. Peers that are not this machine are
        // limited to the overlay routes (see `server::LAN_ROUTES`), but the
        // live stream itself is visible to anyone who can reach the port.
        warn!(
            %http_addr,
            served_to_network = ?server::LAN_ROUTES,
            "viz is listening on a non-loopback address; anyone who can reach it can watch the live overlay (the dashboard and recordings stay on this machine)"
        );
    }

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

    #[cfg(feature = "observability")]
    tokio::spawn(stats_reporter(hub.clone()));

    let state = AppState {
        hub: hub.clone(),
        recordings_dir: recordings_dir.clone(),
        pages: Arc::new(server::Pages::render(&cfg.viz.obs, &udp_addr_s)),
        sessions: Arc::new(server::SessionsCache::default()),
        addrs: Arc::new(server::Addrs {
            udp: udp_addr_s.clone(),
            http: http_addr_s.clone(),
        }),
        // Every connected page holds a task of its own; each one watches this
        // and sends a `Close` frame rather than being cut off mid-stop.
        shutdown: stop.clone(),
    };
    // With the peer address attached, so the network gate can tell a
    // second PC from a browser on this one.
    let app = server::router(state).into_make_service_with_connect_info::<server::Peer>();

    let listener = NoDelayListener::new(
        tokio::net::TcpListener::bind(http_addr)
            .await
            .with_context(|| format!("failed to bind http {http_addr}"))?,
    );

    // A wildcard bind is where the server listens, not a URL anyone can open:
    // print the loopback form, and say the LAN form is this machine's IP.
    let browse = telemouse_core::localhost::browse_addr(http_addr);
    let lan_hint = if browse == http_addr {
        String::new()
    } else {
        format!(
            " (from another PC: http://<this machine's IP>:{}/obs)",
            http_addr.port()
        )
    };
    info!(
        http = %http_addr,
        udp = %udp_addr_s,
        recordings = %recordings_dir.display(),
        obs_layout = %cfg.viz.obs.layout,
        "telemouse-viz serving; dashboard http://{browse}/ — OBS browser source http://{browse}/obs{lan_hint}"
    );

    serve_until_stopped(listener, app, hub, stop).await
}

/// The service half of the router, with the peer address attached.
type MakeService =
    axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, server::Peer>;

/// Serve until `stop` fires, then close.
///
/// Two futures race. The first is the server's own graceful shutdown: the
/// listener stops accepting, in-flight requests finish, idle keep-alive
/// connections are closed, and it resolves when the last connection is gone —
/// which is the right answer, and not one that can be waited on
/// unconditionally, because "the last connection" includes a peer that has no
/// reason to ever go away. The second is [`SHUTDOWN_DEADLINE`] measured from
/// the signal: when it wins, the remaining connections are dropped with the
/// server and the process leaves anyway, on its own terms, rather than
/// waiting to be terminated.
///
/// Either way `main` returns `Ok`, so a stop is exit code 0 — a clean stop,
/// as the control panel and anything else watching this process reads it.
async fn serve_until_stopped(
    listener: NoDelayListener,
    app: MakeService,
    hub: Arc<Hub>,
    stop: Shutdown,
) -> Result<()> {
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let stop = stop.clone();
        async move { stop.wait().await }
    });
    let deadline = async {
        stop.wait().await;
        tokio::time::sleep(SHUTDOWN_DEADLINE).await;
    };

    tokio::select! {
        result = server => {
            result.context("http server failed")?;
            info!("stopped; every connection closed");
        }
        () = deadline => {
            // The one line that says *what* a stop was waiting on. Without it
            // the evidence is a process that took a second longer than it
            // should have and nothing to say why.
            warn!(
                ws_clients = hub.stats.snapshot().clients,
                deadline_ms = SHUTDOWN_DEADLINE.as_millis() as u64,
                "stopped; connections were still open at the deadline (a held keep-alive socket or a recording still streaming) and were dropped"
            );
        }
    }
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
        Self {
            inner,
            warned: false,
        }
    }
}

/// How a request learns who connected: the accepted socket's peer address,
/// as reported by the wrapped listener.
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, NoDelayListener>>
    for server::Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, NoDelayListener>) -> Self {
        server::Peer(*stream.remote_addr())
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
#[cfg(feature = "observability")]
async fn stats_reporter(hub: Arc<Hub>) {
    let mut prev = hub.stats.snapshot();
    let mut last = Instant::now();
    let mut log_prev = prev;
    let mut log_last = last;
    let mut prev_bytes = hub.stats.bytes_forwarded();
    let mut prev_gaps = hub.stats.seq_gaps();
    let mut pushes: u32 = 0;
    let mut quiet_intervals: u32 = 0;
    let mut feed = "never";

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
        let gaps = hub.stats.roll_gaps();

        let bytes = hub.stats.bytes_forwarded();
        let window_bytes = bytes.saturating_sub(prev_bytes);
        prev_bytes = bytes;
        let kb_per_s = if log_secs > 0.0 {
            window_bytes as f64 / 1024.0 / log_secs
        } else {
            0.0
        };
        hub.stats.set_kb_per_s(kb_per_s);
        let seq_gaps_total = hub.stats.seq_gaps();
        let seq_gaps = seq_gaps_total.saturating_sub(prev_gaps);
        prev_gaps = seq_gaps_total;
        let queue_depth = hub.queued();
        let queue_depth_max = hub.stats.take_queue_depth_max();

        // A feed that stops (or comes back) is the single most useful thing
        // in this log, and it is invisible in the counters: they simply stop
        // moving. Say it once per transition.
        let now_feed = crate::stats::feed_state(
            hub.stats.udp_bound(),
            hub.stats.last_datagram_age_s(crate::stats::now_utc_us()),
        );
        if now_feed != feed {
            match now_feed {
                "stalled" => warn!(
                    previous = feed,
                    after_s = crate::stats::FEED_STALL_AFTER_S,
                    "no datagrams from the capture agent; live mode is idle"
                ),
                _ => info!(previous = feed, feed = now_feed, "feed state changed"),
            }
            feed = now_feed;
        }

        let idle = ld.datagrams == 0 && ld.parse_errors == 0 && cur.clients == 0;
        if idle && quiet_intervals + 1 < HEARTBEAT_EVERY {
            // Idle and unobserved: stay quiet rather than spamming the log —
            // but not forever (see `HEARTBEAT_EVERY`).
            quiet_intervals += 1;
            continue;
        }
        quiet_intervals = 0;
        info!(
            datagrams_per_s = format_args!("{:.1}", ld.datagrams_per_s),
            kb_per_s = format_args!("{kb_per_s:.1}"),
            forwarded = ld.forwarded,
            bytes_forwarded = bytes,
            parse_errors = ld.parse_errors,
            parse_errors_total = cur.parse_errors,
            seq_gaps,
            seq_gaps_total,
            ws_clients = cur.clients,
            session_cached = hub.cached_session().is_some(),
            queue_depth,
            queue_depth_max,
            lag_drops = ld.lag_drops,
            lag_disconnects = ld.lag_disconnects,
            lat_p50_ms = format_args!("{:.1}", lat.p50_us as f64 / 1000.0),
            lat_p99_ms = format_args!("{:.1}", lat.p99_us as f64 / 1000.0),
            lat_max_ms = format_args!("{:.1}", lat.max_us as f64 / 1000.0),
            lat_samples = lat.samples,
            lat_negative = lat.negative,
            gap_p99_ms = format_args!("{:.1}", gaps.p99_ms()),
            gap_max_ms = format_args!("{:.1}", gaps.max_ms()),
            feed,
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
            "--log-dir",
            "logs",
        ]);
        let Some(Command::Serve(a)) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(a.config, PathBuf::from("custom.toml"));
        assert_eq!(a.udp.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(a.http.as_deref(), Some("127.0.0.1:9001"));
        assert_eq!(a.log_dir, Some(PathBuf::from("logs")));
    }

    /// No `--log-dir` means stderr only: a default would put a log file
    /// somewhere the user never asked for.
    #[test]
    fn the_log_directory_has_no_default() {
        let cli = Cli::parse_from(["telemouse-viz", "serve"]);
        let Some(Command::Serve(a)) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(a.log_dir, None);
    }

    #[test]
    fn the_feature_banner_names_every_feature() {
        let line = FEATURES.join(",");
        for name in ["logging", "observability", "quiet"] {
            assert!(line.contains(name), "{line} should mention {name}");
        }
        // Enabled and disabled are distinguishable at a glance.
        assert_eq!(
            FEATURES.contains(&"observability"),
            cfg!(feature = "observability"),
            "{line}"
        );
    }

    /// A config file that exists but does not parse must stop the process,
    /// not start it on defaults: an operator who edited the file and got the
    /// old settings anyway has no way to tell.
    #[tokio::test]
    async fn a_broken_config_refuses_to_start() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("telemouse.toml");
        std::fs::write(&path, "this is not toml = = =").unwrap();
        let err = serve(
            ServeArgs {
                config: path.clone(),
                ..Default::default()
            },
            None,
            Shutdown::new(),
        )
        .await
        .expect_err("a broken config must not start the server");
        let text = format!("{err:#}");
        assert!(text.contains("refusing to start"), "{text}");

        // A value the parser accepts but the pipeline cannot use, too.
        std::fs::write(&path, "[viz.obs]\nstale_secs = 900.0\n").unwrap();
        assert!(
            serve(
                ServeArgs {
                    config: path,
                    ..Default::default()
                },
                None,
                Shutdown::new(),
            )
            .await
            .is_err()
        );
    }

    /// A running server, an ephemeral port, and the signal the console
    /// handler fires. Returns the address, the state the handlers see, and
    /// the task the server is running on.
    async fn served(
        dir: &std::path::Path,
        stop: Shutdown,
    ) -> (SocketAddr, Arc<Hub>, tokio::task::JoinHandle<Result<()>>) {
        let hub = Arc::new(Hub::new());
        let state = AppState {
            hub: hub.clone(),
            recordings_dir: dir.to_path_buf(),
            pages: Arc::new(server::Pages::render(
                &telemouse_core::config::ObsConfig::default(),
                "127.0.0.1:7878",
            )),
            sessions: Arc::new(server::SessionsCache::default()),
            addrs: Arc::new(server::Addrs::default()),
            shutdown: stop.clone(),
        };
        let app = server::router(state).into_make_service_with_connect_info::<server::Peer>();
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let task = tokio::spawn(serve_until_stopped(
            NoDelayListener::new(tcp),
            app,
            hub.clone(),
            stop,
        ));
        (addr, hub, task)
    }

    /// Read a response head (everything up to the blank line) off a raw
    /// socket. Enough to see the `101` a WebSocket upgrade answers with.
    async fn read_head(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt as _;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = sock.read(&mut byte).await.unwrap();
            assert!(n == 1, "the server closed before answering the handshake");
            head.push(byte[0]);
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    /// A dashboard tab: a real upgraded `/ws`, held open and never read from.
    /// Hand-rolled rather than pulled in as a dependency — the server does
    /// the interesting half, and the client only has to exist.
    async fn connect_ws(addr: SocketAddr, hub: &Hub) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt as _;
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        sock.write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Origin: http://{addr}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let head = read_head(&mut sock).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        // The upgrade is answered before the client task has registered, so
        // wait for the socket the stop has to let go of to exist.
        for _ in 0..200 {
            if hub.stats.snapshot().clients == 1 {
                return sock;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the ws client never registered");
    }

    /// The bug this guards against: viz stayed up through a graceful stop, so
    /// `telemouse-ctl` sat out its whole grace period and terminated it. A
    /// live WebSocket is the case that made it visible — the socket outlives
    /// the HTTP connection it was upgraded from, so nothing but the signal
    /// itself will ever close it.
    #[tokio::test]
    async fn a_stop_finishes_with_a_websocket_client_connected() {
        use tokio::io::AsyncReadExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let stop = Shutdown::new();
        let (addr, hub, task) = served(tmp.path(), stop.clone()).await;
        let mut ws = connect_ws(addr, &hub).await;

        let started = std::time::Instant::now();
        stop.fire();
        tokio::time::timeout(SHUTDOWN_DEADLINE + Duration::from_secs(2), task)
            .await
            .expect("the server must stop, connected client or not")
            .unwrap()
            .unwrap();
        let took = started.elapsed();
        assert!(
            took < SHUTDOWN_DEADLINE,
            "a client that is told to go should not cost the deadline: {took:?}"
        );

        // And the page was told, so it shows "disconnected" and reconnects
        // instead of waiting on a socket nobody will write to again.
        let mut frame = [0u8; 2];
        ws.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[0], 0x88, "a Close frame, not a dropped socket");
    }

    /// The deadline is the other half: a connection that will not finish on
    /// its own (here a request whose headers never end — a browser holding a
    /// keep-alive socket, or a 400 MB replay still streaming, do the same)
    /// must not turn a stop into a termination.
    #[tokio::test]
    async fn a_stop_is_bounded_by_the_deadline() {
        use tokio::io::AsyncWriteExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let stop = Shutdown::new();
        let (addr, _hub, task) = served(tmp.path(), stop.clone()).await;

        let mut half = tokio::net::TcpStream::connect(addr).await.unwrap();
        half.write_all(format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\n").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        stop.fire();
        tokio::time::timeout(SHUTDOWN_DEADLINE + Duration::from_secs(2), task)
            .await
            .expect("the deadline must end the wait")
            .unwrap()
            .unwrap();
        let took = started.elapsed();
        // Bounded, and bounded well below `ctl.stop_grace_secs`.
        assert!(
            took < SHUTDOWN_DEADLINE + Duration::from_secs(1),
            "a stop must not outlast the deadline by much: {took:?}"
        );
        drop(half);
    }

    /// The numbers the comments above rest on: the process has to be gone
    /// long before the panel gives up on it, including the clamped grace a
    /// Windows shutdown gets.
    #[test]
    fn the_deadline_fits_inside_the_panels_grace_period() {
        assert!(SHUTDOWN_DEADLINE < Duration::from_secs(3));
        assert!(
            SHUTDOWN_GRACE > SHUTDOWN_DEADLINE,
            "a terminal console event must be held until teardown is done"
        );
    }

    /// A config that is simply not there is the zero-configuration case, and
    /// must still serve. (It gets as far as binding a port, so the test asks
    /// for one the OS picks and then stops it the way a Ctrl-Break would.)
    #[tokio::test]
    async fn a_missing_config_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let args = ServeArgs {
            config: tmp.path().join("nope.toml"),
            http: Some("127.0.0.1:0".into()),
            udp: Some("127.0.0.1:0".into()),
            recordings: Some(tmp.path().to_path_buf()),
            log_dir: None,
        };
        // `serve` only returns when the listener stops, so give it a moment
        // to get past config loading and the bind, then ask it to stop.
        let stop = Shutdown::new();
        let task = tokio::spawn(serve(args, None, stop.clone()));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!task.is_finished(), "the server should still be serving");
        stop.fire();
        tokio::time::timeout(SHUTDOWN_DEADLINE + Duration::from_secs(2), task)
            .await
            .expect("an idle server must stop on the signal")
            .unwrap()
            .unwrap();
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
        assert!(
            stream.nodelay().unwrap(),
            "accepted stream must have TCP_NODELAY"
        );
    }
}
