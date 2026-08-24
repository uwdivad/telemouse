//! `telemouse` — the Phase 1/2 capture agent.
//!
//! Three threads (see `mouse-telemetry-plan.md`):
//!
//! * **T1** [`raw_input`] — message-only window, `WM_INPUT` → QPC timestamp →
//!   SPSC ring. Never blocks, never allocates after startup.
//! * **T2** [`shipping`] — drains the ring, batches on a 25ms window, fans out
//!   to UDP / Kafka / JSONL. Each sink fails independently.
//! * **T3** [`context_thread`] — 250ms poll of foreground process, screen and
//!   cursor, plus the pointer-lock heuristic and the 5s stats report.

mod context;
mod context_thread;
mod devices;
mod platform;
mod pointer_lock;
mod raw_input;
mod session_setup;
mod shipping;
mod shutdown;
mod sinks;
mod stats;

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::{Args, Parser, Subcommand};
use telemouse_core::config::AppConfig;
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "telemouse.toml";

#[derive(Debug, Parser)]
#[command(name = "telemouse", version, about = "Raw mouse telemetry capture agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Capture raw mouse input and ship it to the configured sinks.
    Run(RunArgs),
    /// Report the environment as the agent sees it, then exit.
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Path to telemouse.toml (defaults are used if it does not exist).
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
    /// Log a one-line summary for every batch.
    #[arg(long)]
    print: bool,
    /// Disable the Kafka sink regardless of config.
    #[arg(long)]
    no_kafka: bool,
    /// Disable the localhost UDP sink regardless of config.
    #[arg(long)]
    no_udp: bool,
    /// Disable the local JSONL recording regardless of config.
    #[arg(long)]
    no_record: bool,
    /// Stop automatically after N seconds (smoke testing).
    #[arg(long)]
    duration_secs: Option<u64>,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Run(args) => cmd_run(args),
        Command::Doctor(args) => cmd_doctor(args),
    }
}

/// Apply the CLI's disable switches to a loaded config.
fn apply_cli(cfg: &mut AppConfig, args: &RunArgs) {
    if args.no_kafka {
        cfg.kafka.enabled = false;
    }
    if args.no_udp {
        cfg.udp.enabled = false;
    }
    if args.no_record {
        cfg.recording.enabled = false;
    }
}

/// Load config from disk (defaults if absent).
fn load_config(path: &std::path::Path) -> Result<AppConfig> {
    AppConfig::load_or_default(path).with_context(|| format!("load config {}", path.display()))
}

/// Load config and apply the CLI's disable switches.
fn resolve_config(path: &std::path::Path, args: Option<&RunArgs>) -> Result<AppConfig> {
    let mut cfg = load_config(path)?;
    if let Some(args) = args {
        apply_cli(&mut cfg, args);
    }
    Ok(cfg)
}

#[cfg(not(windows))]
fn cmd_run(_args: RunArgs) -> Result<()> {
    anyhow::bail!("raw input capture requires Windows; `telemouse doctor` still works here")
}

#[cfg(windows)]
fn cmd_run(args: RunArgs) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use telemouse_core::RawEvent;

    use crate::context::{ContextSnapshot, SharedContext};
    use crate::context_thread::ContextArgs;
    use crate::raw_input::{CaptureDeps, CaptureHandles, RingWaker};
    use crate::session_setup::{SessionEnv, build_session_config, measure_anchor, new_session_id};
    use crate::shipping::{MarkerSignal, ShippingArgs};
    use crate::shutdown::Shutdown;
    use crate::sinks::{JsonlSink, KafkaSink, Sink, UdpSink};
    use crate::stats::Stats;

    let file_cfg = load_config(&args.config)?;
    let mut cfg = file_cfg.clone();
    apply_cli(&mut cfg, &args);

    // Keep Windows from parking us on a throttled E-core while a game owns the
    // P-cores. Best effort: a failure is logged, never fatal.
    platform::disable_power_throttling();

    // One QPC↔UTC anchor for the whole session, taken as a QPC/UTC/QPC sandwich
    // so its pairing error is measured rather than assumed.
    let qpc_freq = platform::qpc_freq();
    let (anchor, anchor_uncertainty_us) = measure_anchor(qpc_freq);
    let session_id = new_session_id(anchor.utc_us);

    // Enumerate pointing devices once, before capture starts: the names ship in
    // the session record and T1 maps handles onto these very indices.
    let device_table = devices::DeviceTable::new(devices::enumerate_mice());
    let device_names = device_table.names().to_vec();
    for (ix, name) in device_names.iter().enumerate().skip(1) {
        tracing::info!(device_ix = ix, name = %name, "pointing device");
    }
    if device_table.is_empty() {
        tracing::warn!("no pointing devices enumerated; events will report device_ix=0");
    }

    let session = build_session_config(
        session_id.clone(),
        SessionEnv {
            anchor,
            anchor_uncertainty_us: Some(anchor_uncertainty_us),
            mouse_cpi: cfg.mouse_cpi,
            devices: device_names.clone(),
            games: cfg.games.clone(),
            monitors: platform::monitors(),
            capture_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    );
    tracing::info!(
        session = %session_id,
        qpc_freq,
        anchor_uncertainty_us,
        devices = device_table.len() - 1,
        monitors = session.monitors.len(),
        "session starting"
    );

    let stats = Arc::new(Stats::default());

    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    if cfg.udp.enabled {
        match UdpSink::connect(&cfg.udp.addr, Arc::clone(&stats)) {
            Ok(s) => {
                tracing::info!(addr = %cfg.udp.addr, "udp sink ready");
                sinks.push(Box::new(s));
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "udp sink unavailable"),
        }
    }
    if cfg.recording.enabled {
        match JsonlSink::create(&cfg.recording.dir, &session_id, Arc::clone(&stats)) {
            Ok(s) => {
                tracing::info!(path = %s.path().display(), "recording to jsonl");
                sinks.push(Box::new(s));
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "recording unavailable"),
        }
    }
    if cfg.kafka.enabled {
        match KafkaSink::connect(&cfg.kafka.brokers, Arc::clone(&stats)) {
            Ok(s) => {
                tracing::info!(brokers = ?cfg.kafka.brokers, "kafka sink ready");
                sinks.push(Box::new(s));
            }
            // A missing broker degrades the pipeline; it never stops it.
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "kafka unavailable; continuing without it"
            ),
        }
    }
    if sinks.is_empty() {
        tracing::warn!("no sinks enabled; capture will only update counters");
    }

    let (producer, consumer) = rtrb::RingBuffer::<RawEvent>::new(cfg.batch.ring_capacity);
    let (marker_tx, marker_rx) = std::sync::mpsc::channel::<MarkerSignal>();

    let (screen_w, screen_h) = platform::primary_screen();
    let ctx = Arc::new(SharedContext::new(ContextSnapshot {
        screen_w,
        screen_h,
        ..Default::default()
    }));

    let shutdown = Arc::new(Shutdown::new());
    let capture_stopped = Arc::new(AtomicBool::new(false));
    // Cleared by T1 when its message loop returns, for any reason. The main
    // loop watches it so a dead capture thread is an error, not silence.
    let capture_alive = Arc::new(AtomicBool::new(true));
    let handles = Arc::new(CaptureHandles::default());
    let waker = Arc::new(RingWaker::default());

    {
        let shutdown = Arc::clone(&shutdown);
        ctrlc::set_handler(move || shutdown.set()).context("install Ctrl-C handler")?;
    }

    let t1 = {
        let deps = CaptureDeps {
            producer,
            marker_tx: marker_tx.clone(),
            stats: Arc::clone(&stats),
            handles: Arc::clone(&handles),
            waker: Arc::clone(&waker),
            ctx: Arc::clone(&ctx),
            devices: device_table,
            hotkey: raw_input::Hotkey::default(),
        };
        let (alive, shutdown) = (Arc::clone(&capture_alive), Arc::clone(&shutdown));
        std::thread::Builder::new()
            .name("telemouse-capture".into())
            .spawn(move || {
                if let Err(e) = raw_input::run(deps) {
                    tracing::error!(error = %format!("{e:#}"), "capture thread failed");
                }
                alive.store(false, Ordering::Release);
                // Wake the main thread so it notices immediately.
                shutdown.notify();
            })
            .context("spawn capture thread")?
    };

    let t3 = {
        let (ctx, stats, shutdown) = (
            Arc::clone(&ctx),
            Arc::clone(&stats),
            Arc::clone(&shutdown),
        );
        let ctx_args = ContextArgs {
            session_id: session_id.clone(),
            anchor,
            config_path: args.config.clone(),
            config: file_cfg,
            marker_tx,
        };
        std::thread::Builder::new()
            .name("telemouse-context".into())
            .spawn(move || context_thread::run(ctx, stats, shutdown, ctx_args))
            .context("spawn context thread")?
    };

    let t2 = {
        let (ctx, stats) = (Arc::clone(&ctx), Arc::clone(&stats));
        let ship_args = ShippingArgs {
            session,
            window_ms: cfg.batch.window_ms,
            max_events: cfg.batch.max_events,
            print: args.print,
            capture_stopped: Arc::clone(&capture_stopped),
            waker: Arc::clone(&waker),
        };
        std::thread::Builder::new()
            .name("telemouse-shipping".into())
            .spawn(move || shipping::run(consumer, marker_rx, sinks, ctx, stats, ship_args))
            .context("spawn shipping thread")?
    };

    let deadline = args
        .duration_secs
        .map(|s| Instant::now() + Duration::from_secs(s));
    if let Some(secs) = args.duration_secs {
        tracing::info!(secs, "will stop automatically");
    }
    // Idle here costs one wakeup per minute (or one at the deadline): the
    // condvar is signalled by Ctrl-C and by T1 dying.
    loop {
        if shutdown.is_set() {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if !capture_alive.load(Ordering::Acquire) {
            tracing::error!("capture thread exited; shutting down so the gap is not silent");
            break;
        }
        match deadline {
            Some(d) => shutdown.wait_until(d),
            None => shutdown.wait_timeout(Duration::from_secs(60)),
        };
    }
    tracing::info!("shutting down");

    // Ordered teardown: stop capture, let shipping drain what is left, then the
    // context thread. Every partial batch is flushed before we exit.
    shutdown.set();
    let hwnd = handles.hwnd();
    let thread_id = handles.thread_id();
    if hwnd != 0 {
        raw_input::post_quit(hwnd);
    } else if thread_id != 0 {
        // The window never came up; WM_QUIT straight to the thread queue still
        // breaks GetMessageW, so the join below cannot hang.
        tracing::warn!("capture window never came up; posting WM_QUIT to the thread");
        raw_input::post_thread_quit(thread_id);
    }
    if hwnd == 0 && thread_id == 0 && capture_alive.load(Ordering::Acquire) {
        // Nothing to post to and the thread claims to be running: detach rather
        // than block forever on a join that can never complete.
        tracing::error!("capture thread never published a handle; detaching it");
        drop(t1);
    } else {
        let _ = t1.join();
    }
    capture_stopped.store(true, Ordering::Release);
    let _ = t2.join();
    let _ = t3.join();

    let final_stats = stats.snapshot();
    tracing::info!(
        session = %session_id,
        events = final_stats.events,
        batches = final_stats.batches,
        markers = final_stats.markers,
        ring_drops = final_stats.ring_drops,
        ring_high_water = final_stats.ring_high_water,
        abs_frames = final_stats.abs_frames,
        udp_errors = final_stats.udp_errors,
        udp_unreachable = final_stats.udp_unreachable,
        udp_oversized = final_stats.udp_oversized,
        jsonl_errors = final_stats.jsonl_errors,
        jsonl_flush_max_us = final_stats.jsonl_flush_max_us,
        kafka_errors = final_stats.kafka_errors,
        kafka_dropped = final_stats.kafka_dropped,
        kafka_abandoned = final_stats.kafka_abandoned,
        capture_to_ship_us_p50 = final_stats.ship_latency_first.percentile_us(0.50),
        capture_to_ship_us_p99 = final_stats.ship_latency_first.percentile_us(0.99),
        "session finished"
    );
    Ok(())
}

fn cmd_doctor(args: DoctorArgs) -> Result<()> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let cfg = resolve_config(&args.config, None)?;

    println!("telemouse {} — doctor", env!("CARGO_PKG_VERSION"));
    println!("os                : {}", std::env::consts::OS);
    println!(
        "config            : {} ({})",
        args.config.display(),
        if args.config.exists() {
            "loaded"
        } else {
            "missing, using defaults"
        }
    );

    let freq = platform::qpc_freq();
    let a = platform::qpc();
    let b = platform::qpc();
    println!(
        "qpc frequency     : {freq} ticks/s ({:.3} MHz), resolution {} ticks between reads",
        freq as f64 / 1e6,
        b.saturating_sub(a)
    );

    let (w, h) = platform::primary_screen();
    println!("primary screen    : {w}x{h}");
    for (i, m) in platform::monitors().iter().enumerate() {
        println!(
            "  monitor[{i}]     : {}x{}{}{}",
            m.width,
            m.height,
            m.refresh_hz
                .map(|r| format!(" @{r}Hz"))
                .unwrap_or_default(),
            if m.primary { " (primary)" } else { "" }
        );
    }
    println!(
        "cursor            : {}",
        platform::cursor_pos()
            .map(|(x, y)| format!("{x},{y}"))
            .unwrap_or_else(|| "unavailable".into())
    );
    println!(
        "foreground process: {}",
        platform::foreground_process_name().unwrap_or_else(|| "unknown".into())
    );

    let mice = devices::enumerate_mice();
    println!("pointing devices  : {}", mice.len());
    for (ix, (_, name)) in mice.iter().enumerate() {
        // device_ix 0 is reserved for "unknown", so real devices start at 1.
        println!("  device[{}]      : {name}", ix + 1);
    }

    // UDP: we can only prove our own socket works — nothing listens on the far
    // end of an unconnected datagram socket.
    print!("udp sink          : ");
    if cfg.udp.enabled {
        match sinks::UdpSink::connect(&cfg.udp.addr, std::sync::Arc::new(stats::Stats::default())) {
            Ok(s) => println!("ready -> {}", s.addr()),
            Err(e) => println!("UNAVAILABLE ({e:#})"),
        }
    } else {
        println!("disabled");
    }

    print!("recording         : ");
    if cfg.recording.enabled {
        match std::fs::create_dir_all(&cfg.recording.dir) {
            Ok(()) => println!("ready -> {}", cfg.recording.dir.display()),
            Err(e) => println!("UNAVAILABLE ({e})"),
        }
    } else {
        println!("disabled");
    }

    println!(
        "kafka             : {}",
        if cfg.kafka.enabled {
            "enabled"
        } else {
            "disabled (probing brokers anyway)"
        }
    );
    for broker in &cfg.kafka.brokers {
        let reachable = broker
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok())
            .unwrap_or(false);
        println!(
            "  broker {broker:<22}: {}",
            if reachable {
                "reachable"
            } else {
                "UNREACHABLE"
            }
        );
    }

    println!("--- resolved config ---");
    match toml::to_string_pretty(&cfg) {
        Ok(text) => print!("{text}"),
        Err(e) => println!("(could not render config: {e})"),
    }
    Ok(())
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
    fn run_flags_parse() {
        let cli = Cli::parse_from([
            "telemouse",
            "run",
            "--print",
            "--no-kafka",
            "--no-udp",
            "--no-record",
            "--duration-secs",
            "3",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(args.print && args.no_kafka && args.no_udp && args.no_record);
        assert_eq!(args.duration_secs, Some(3));
        assert_eq!(args.config, PathBuf::from(DEFAULT_CONFIG));
    }

    #[test]
    fn doctor_defaults_to_the_repo_config() {
        let cli = Cli::parse_from(["telemouse", "doctor"]);
        let Command::Doctor(args) = cli.command else {
            panic!("expected doctor");
        };
        assert_eq!(args.config, PathBuf::from(DEFAULT_CONFIG));
    }

    #[test]
    fn cli_switches_disable_sinks_in_the_resolved_config() {
        let missing = PathBuf::from("definitely-not-a-config-file.toml");
        let defaults = resolve_config(&missing, None).unwrap();
        assert!(defaults.udp.enabled);
        assert!(defaults.recording.enabled);

        let Command::Run(args) =
            Cli::parse_from(["telemouse", "run", "--no-udp", "--no-record", "--no-kafka"]).command
        else {
            panic!("expected run");
        };
        let cfg = resolve_config(&missing, Some(&args)).unwrap();
        assert!(!cfg.udp.enabled);
        assert!(!cfg.recording.enabled);
        assert!(!cfg.kafka.enabled);
    }
}
