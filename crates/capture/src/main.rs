//! `telemouse` — the Phase 1/2 capture agent.
//!
//! Three threads (see `mouse-telemetry-plan.md`):
//!
//! * **T1** [`raw_input`] — message-only window, `WM_INPUT` → QPC timestamp →
//!   SPSC ring. Never blocks, never allocates on the per-report path (rare
//!   exceptions: a hotkey marker allocates its label and mpsc node, and a
//!   never-before-seen device allocates its device-table entry).
//! * **T2** [`shipping`] — drains the ring, batches on the configured window,
//!   fans out to UDP / Kafka / JSONL. Each sink fails independently.
//! * **T3** [`context_thread`] — 250ms poll of foreground process, screen and
//!   cursor, plus the pointer-lock heuristic, the 5s stats report and the
//!   `<session>.meta.json` refresh.

mod context;
mod context_thread;
mod devices;
#[cfg(feature = "observability")]
mod meta;
mod platform;
mod pointer_lock;
mod raw_input;
mod session_setup;
mod shipping;
mod shutdown;
mod sinks;
mod stats;
mod stdin_markers;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::{Args, Parser, Subcommand};
use telemouse_core::config::AppConfig;

const DEFAULT_CONFIG: &str = "telemouse.toml";

/// Which build this is. A debug build drops events at rates a release build
/// never would, so it is the first thing a surprising drop count should be
/// checked against — it goes in the startup banner and in the sidecar.
pub const PROFILE: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

/// What the log filter defaults to when `RUST_LOG` says nothing. `rskafka`
/// narrates every connection attempt at `info`, which on a desk with no
/// broker is the only thing in the log.
const DEFAULT_LOG_FILTER: &str = "info,rskafka=warn";

#[derive(Debug, Parser)]
#[command(
    name = "telemouse",
    version,
    about = "Raw mouse telemetry capture agent",
    arg_required_else_help = true
)]
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
    /// Also write `<DIR>/capture.log`. Off by default: started from the
    /// control panel, this process's stderr is already teed into one.
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
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
    #[arg(long, conflicts_with = "record")]
    no_record: bool,
    /// Enable the local JSONL recording regardless of config
    /// (`recording.enabled = false` in telemouse.toml).
    #[arg(long)]
    record: bool,
    /// Stop automatically after N seconds (smoke testing).
    #[arg(long)]
    duration_secs: Option<u64>,
}

#[derive(Debug, Args)]
struct DoctorArgs {
    #[arg(long, default_value = DEFAULT_CONFIG)]
    config: PathBuf,
}

/// The optional features this build was compiled with, comma-separated in a
/// fixed order — the banner's answer to "which telemouse is this?", which a
/// version number alone cannot give once the same version ships in three
/// shapes.
fn enabled_features() -> String {
    let mut on: Vec<&str> = Vec::new();
    if cfg!(feature = "logging") {
        on.push("logging");
    }
    if cfg!(feature = "observability") {
        on.push("observability");
    }
    if cfg!(feature = "kafka") {
        on.push("kafka");
    }
    if cfg!(feature = "quiet") {
        on.push("quiet");
    }
    on.join(",")
}

/// What logging init ended up doing, in the shape the startup lines need.
/// Empty when this build has no subscriber at all.
#[derive(Debug, Default)]
struct LogSetup {
    file: Option<PathBuf>,
    file_error: Option<String>,
}

#[cfg(feature = "logging")]
fn init_logging(log_dir: Option<&Path>) -> LogSetup {
    let init = telemouse_core::logging::init(telemouse_core::logging::LogOptions {
        component: "capture",
        log_dir,
        default_filter: DEFAULT_LOG_FILTER,
    });
    LogSetup {
        file: init.file,
        file_error: init.file_error,
    }
}

/// No subscriber in this build: the `tracing` macros stay, and cost a relaxed
/// load and a branch each with nobody listening.
#[cfg(not(feature = "logging"))]
fn init_logging(_log_dir: Option<&Path>) -> LogSetup {
    let _ = DEFAULT_LOG_FILTER;
    LogSetup::default()
}

/// Double-clicked, or started from a shortcut, with nothing to do: say where
/// the controls are and hold the window open long enough to be read.
fn explain_and_wait() -> Result<()> {
    println!("Run telemouse-ctl.exe for the control panel, or `telemouse run`.");
    println!();
    print!("Press Enter to close this window. ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    Ok(())
}

fn main() -> Result<()> {
    // Before anything reads a screen size, a monitor rectangle or the cursor:
    // on a scaled desktop those are virtualised until this is called, and
    // every one of them goes into the session record.
    platform::set_dpi_awareness();

    // A console this process owns closes the instant it returns, taking
    // clap's "which subcommand?" help with it.
    if std::env::args_os().count() == 1 && platform::owns_console() {
        return explain_and_wait();
    }

    let cli = Cli::parse();
    let log_dir = match &cli.command {
        Command::Run(args) => args.log_dir.clone(),
        Command::Doctor(_) => None,
    };
    let log = init_logging(log_dir.as_deref());
    // A panic on any thread reaches the same log as everything else — the
    // control panel's capture.log when launched from there — not just a
    // console nobody may be watching.
    telemouse_core::panic_hook::install("capture");
    if let Some(path) = &log.file {
        tracing::info!(path = %path.display(), "logging to file");
    }
    if let Some(error) = &log.file_error {
        tracing::warn!(error, "could not open the log file; stderr only");
    }

    match cli.command {
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
    if args.record {
        cfg.recording.enabled = true;
    }
}

/// Load config from disk (defaults if absent), with every relative path in it
/// made absolute against the config file's own directory — so `recordings`
/// means the folder beside the config, not beside whatever started us.
fn load_config(path: &Path) -> Result<AppConfig> {
    let mut cfg = AppConfig::load_or_default(path)
        .with_context(|| format!("load config {}", path.display()))?;
    cfg.resolve_paths(&telemouse_core::paths::config_base(path));
    Ok(cfg)
}

/// Load config and apply the CLI's disable switches.
fn resolve_config(path: &Path, args: Option<&RunArgs>) -> Result<AppConfig> {
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

/// Clears an alive flag when its thread exits *for any reason* — a panic
/// unwinds through the guard, so the main loop's watchdog sees honest state
/// instead of a zombie thread.
#[cfg(windows)]
struct AliveGuard {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    shutdown: std::sync::Arc<shutdown::Shutdown>,
}

#[cfg(windows)]
impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::Release);
        // Wake the main thread so it notices immediately.
        self.shutdown.notify();
    }
}

/// Join a worker thread, logging a panic payload instead of discarding it.
/// Returns false on a panic so the process can exit nonzero.
#[cfg(windows)]
fn join_loudly(name: &'static str, handle: std::thread::JoinHandle<()>) -> bool {
    match handle.join() {
        Ok(()) => true,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            tracing::error!(thread = name, panic = %msg, "worker thread panicked");
            false
        }
    }
}

/// How long a console-close handler is held while teardown finishes. Windows
/// grants about five seconds before it kills the process anyway.
#[cfg(windows)]
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

/// How often the main thread checks that T2 and T3 are still looping.
#[cfg(windows)]
const STALL_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[cfg(windows)]
fn cmd_run(args: RunArgs) -> Result<()> {
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use telemouse_core::RawEvent;
    use telemouse_core::recordings::ExitReason;
    use telemouse_core::shutdown::Signal;

    use crate::context::{ContextSnapshot, SharedContext};
    use crate::context_thread::ContextArgs;
    use crate::raw_input::{CaptureDeps, CaptureHandles, RingWaker};
    use crate::session_setup::{SessionEnv, build_session_config, measure_anchor, new_session_id};
    use crate::shipping::{MarkerSignal, ShippingArgs};
    use crate::shutdown::Shutdown;
    use crate::sinks::{JsonlSink, Sink, UdpSink};
    use crate::stats::{StallWatch, Stalled, Stats};

    let config_path = telemouse_core::paths::locate_config(&args.config);
    let config_found = config_path.exists();
    // Loaded once, raw: T3 diffs later reloads against this, and a reload
    // reads the file the same way, so both sides speak in the file's own
    // relative paths rather than reporting a spurious `recording.dir` change.
    let file_cfg = AppConfig::load_or_default(&config_path)
        .with_context(|| format!("load config {}", config_path.display()))?;
    // Reported as the file spells them, so a relative `recordings` is not
    // listed as an override just because it was made absolute.
    let overrides = file_cfg.non_default_fields();
    let mut cfg = file_cfg.clone();
    cfg.resolve_paths(&telemouse_core::paths::config_base(&config_path));
    apply_cli(&mut cfg, &args);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        profile = PROFILE,
        features = %enabled_features(),
        config = %config_path.display(),
        config_found,
        window_ms = cfg.batch.window_ms,
        coalesce_ms = cfg.batch.coalesce_ms,
        mouse_cpi = cfg.mouse_cpi,
        marker_hotkey = %cfg.marker_hotkey,
        overrides = ?overrides,
        "telemouse starting"
    );

    // The marker chord, validated when the config loaded; `""` means none.
    let marker_hotkey = match telemouse_core::hotkey::Hotkey::parse(&cfg.marker_hotkey) {
        Ok(hk) => hk,
        Err(e) => {
            tracing::warn!(error = %e, "marker_hotkey is unusable; no marker hotkey");
            None
        }
    };

    // Keep Windows from parking us on a throttled E-core while a game owns the
    // P-cores. Best effort: a failure is logged, never fatal.
    platform::disable_power_throttling();

    // One QPC↔UTC anchor for the whole session, taken as a QPC/UTC/QPC sandwich
    // so its pairing error is measured rather than assumed.
    let qpc_freq = platform::qpc_freq();
    let (anchor, anchor_uncertainty_us) = measure_anchor(qpc_freq);
    let session_id = new_session_id(anchor.utc_us);
    // Everything from here carries the session id, on every thread.
    let session_span = tracing::info_span!("session", id = %session_id);
    let _session_entered = session_span.enter();

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
            coalesce_ms: cfg.batch.coalesce_ms,
            window_ms: cfg.batch.window_ms,
            max_events: cfg.batch.max_events,
            os: platform::os_version(),
        },
    );
    tracing::info!(
        qpc_freq,
        anchor_uncertainty_us,
        devices = device_table.len() - 1,
        monitors = session.monitors.len(),
        os = session.os.as_deref().unwrap_or("unknown"),
        coalesce_ms = cfg.batch.coalesce_ms,
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
    // Only set once the writer actually opened: a sidecar next to a recording
    // that does not exist would be a document about nothing.
    let mut recording_dir: Option<PathBuf> = None;
    if cfg.recording.enabled {
        match JsonlSink::create(&cfg.recording.dir, &session_id, Arc::clone(&stats)) {
            Ok(s) => {
                tracing::info!(path = %s.path().display(), "recording to jsonl");
                recording_dir = Some(cfg.recording.dir.clone());
                sinks.push(Box::new(s));
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "recording unavailable"),
        }
    }
    if cfg.kafka.enabled {
        #[cfg(feature = "kafka")]
        match crate::sinks::KafkaSink::connect(&cfg.kafka.brokers, Arc::clone(&stats)) {
            Ok(s) => {
                tracing::info!(brokers = ?cfg.kafka.brokers, "kafka sink starting");
                sinks.push(Box::new(s));
            }
            // Invalid setup degrades the pipeline; broker reachability is
            // resolved asynchronously by the worker after capture starts.
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "kafka unavailable; continuing without it"
            ),
        }
        #[cfg(not(feature = "kafka"))]
        tracing::warn!("this build has no Kafka support; [kafka] enabled ignored");
    }
    if sinks.is_empty() {
        tracing::warn!("no sinks enabled; capture will only update counters");
    }
    // Remembered for the metadata sidecar: only the sinks that ran get a row.
    let sink_names: Vec<&'static str> = sinks.iter().map(|s| s.name()).collect();

    // The live `<session>.meta.json`. Written from the first context tick
    // onwards, so a run that is killed outright still leaves counters within
    // one report interval of the truth, flagged as unfinished.
    #[cfg(feature = "observability")]
    let sidecar = recording_dir.clone().map(|dir| {
        Arc::new(meta::Sidecar::new(
            dir,
            session_id.clone(),
            env!("CARGO_PKG_VERSION"),
            anchor.utc_us,
            qpc_freq,
            anchor_uncertainty_us,
            cfg.batch.window_ms,
            cfg.batch.coalesce_ms,
            sink_names.clone(),
            Arc::clone(&stats),
        ))
    });
    #[cfg(not(feature = "observability"))]
    let _ = (&sink_names, &recording_dir, anchor_uncertainty_us);

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
    // Same watchdog for T2: a dead shipping thread would otherwise leave a
    // zombie agent capturing into a ring nobody drains.
    let shipping_alive = Arc::new(AtomicBool::new(true));
    // And for T3: without it a context-thread panic would freeze the game
    // name and pointer-lock state for the rest of the session, silently
    // mislabelling every later batch.
    let context_alive = Arc::new(AtomicBool::new(true));
    let handles = Arc::new(CaptureHandles::default());
    let waker = Arc::new(RingWaker::default());

    // Console control events. Ctrl-C and Ctrl-Break return from the handler
    // and let teardown happen here; a window close, a logoff or a shutdown
    // *block* in the handler until `guard.finished()` below, because Windows
    // kills the process the moment such a handler returns — and the tail of
    // the recording, the queued Kafka batches and the sidecar all live on the
    // other side of that moment.
    let signal_seen: Arc<OnceLock<Signal>> = Arc::new(OnceLock::new());
    let guard = {
        let shutdown = Arc::clone(&shutdown);
        let seen = Arc::clone(&signal_seen);
        telemouse_core::shutdown::install(
            move |signal| {
                let _ = seen.set(signal);
                tracing::info!(
                    signal = signal.as_str(),
                    blocking = signal.is_terminal(),
                    "console control event; stopping"
                );
                shutdown.set();
            },
            SHUTDOWN_GRACE,
        )
        .context("install console control handler")?
    };

    let t1 = {
        let deps = CaptureDeps {
            producer,
            marker_tx: marker_tx.clone(),
            stats: Arc::clone(&stats),
            handles: Arc::clone(&handles),
            waker: Arc::clone(&waker),
            ctx: Arc::clone(&ctx),
            devices: device_table,
            hotkey: raw_input::Hotkey::from_config(marker_hotkey),
            coalesce: Duration::from_millis(cfg.batch.coalesce_ms),
            qpc_freq,
        };
        let (alive, shutdown) = (Arc::clone(&capture_alive), Arc::clone(&shutdown));
        let span = session_span.clone();
        std::thread::Builder::new()
            .name("telemouse-capture".into())
            .spawn(move || {
                let _entered = span.enter();
                let _alive = AliveGuard {
                    flag: alive,
                    shutdown,
                };
                if let Err(e) = raw_input::run(deps) {
                    tracing::error!(error = %format!("{e:#}"), "capture thread failed");
                }
            })
            .context("spawn capture thread")?
    };

    // A piped stdin (the control panel, a script) is a marker source; a
    // console is not. Not a thread we join: it ends when the pipe does.
    if stdin_markers::spawn(marker_tx.clone(), Arc::clone(&waker)) {
        tracing::info!("stdin is a pipe; each line written to it becomes a marker");
    }

    let t3 = {
        let (ctx, stats, shutdown) = (Arc::clone(&ctx), Arc::clone(&stats), Arc::clone(&shutdown));
        let ctx_args = ContextArgs {
            session_id: session_id.clone(),
            anchor,
            config_path: config_path.clone(),
            config: file_cfg,
            marker_tx,
            waker: Arc::clone(&waker),
            #[cfg(feature = "observability")]
            sidecar: sidecar.clone(),
        };
        let (alive, guard_shutdown) = (Arc::clone(&context_alive), Arc::clone(&shutdown));
        let span = session_span.clone();
        std::thread::Builder::new()
            .name("telemouse-context".into())
            .spawn(move || {
                let _entered = span.enter();
                let _alive = AliveGuard {
                    flag: alive,
                    shutdown: guard_shutdown,
                };
                context_thread::run(ctx, stats, shutdown, ctx_args)
            })
            .context("spawn context thread")?
    };

    let t2 = {
        let (ctx, stats) = (Arc::clone(&ctx), Arc::clone(&stats));
        let (alive, shutdown) = (Arc::clone(&shipping_alive), Arc::clone(&shutdown));
        // Runs after the recording's final flush and before Kafka's bounded
        // drain, so the sidecar on disk is accurate about the JSONL even if
        // the drain then times out.
        #[cfg(feature = "observability")]
        let on_recording_closed: Option<Box<dyn FnOnce() + Send>> =
            sidecar.clone().map(|s| -> Box<dyn FnOnce() + Send> {
                Box::new(move || {
                    s.refresh();
                })
            });
        #[cfg(not(feature = "observability"))]
        let on_recording_closed: Option<Box<dyn FnOnce() + Send>> = None;
        let ship_args = ShippingArgs {
            session,
            window_ms: cfg.batch.window_ms,
            max_events: cfg.batch.max_events,
            print: args.print,
            capture_stopped: Arc::clone(&capture_stopped),
            waker: Arc::clone(&waker),
            on_recording_closed,
        };
        let span = session_span.clone();
        std::thread::Builder::new()
            .name("telemouse-shipping".into())
            .spawn(move || {
                let _entered = span.enter();
                let _alive = AliveGuard {
                    flag: alive,
                    shutdown,
                };
                shipping::run(consumer, marker_rx, sinks, ctx, stats, ship_args)
            })
            .context("spawn shipping thread")?
    };

    let deadline = args
        .duration_secs
        .map(|s| Instant::now() + Duration::from_secs(s));
    if let Some(secs) = args.duration_secs {
        tracing::info!(secs, "will stop automatically");
    }
    let mut stall_watch = StallWatch::new();
    let mut last_stall_check = Instant::now();
    // Idle here costs one wakeup per minute (or one at the deadline): the
    // condvar is signalled by a console event and by any worker thread dying.
    let exit_reason = loop {
        if shutdown.is_set() {
            break ExitReason::Interrupt;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break ExitReason::Duration;
        }
        if !capture_alive.load(Ordering::Acquire) {
            tracing::error!("capture thread exited; shutting down so the gap is not silent");
            break ExitReason::CaptureThreadExited;
        }
        if !shipping_alive.load(Ordering::Acquire) {
            tracing::error!(
                "shipping thread exited; shutting down so capture does not feed a ring nobody drains"
            );
            break ExitReason::ShippingThreadExited;
        }
        if !context_alive.load(Ordering::Acquire) {
            // Capture itself is intact, but every batch from here on would
            // carry a frozen game name and lock state. Stop rather than
            // mislabel the rest of the session.
            tracing::error!(
                "context thread exited; shutting down rather than record stale game/lock state"
            );
            break ExitReason::ContextThreadExited;
        }
        // A thread can be alive and still have stopped working — parked on a
        // handle that never signals, or spinning inside a sink. The alive
        // flags say nothing about that; the loop counters do.
        if last_stall_check.elapsed() >= STALL_CHECK_INTERVAL {
            last_stall_check = Instant::now();
            let t2_iters = stats.t2_iters.load(Ordering::Relaxed);
            let t3_ticks = stats.t3_ticks.load(Ordering::Relaxed);
            match stall_watch.observe(t2_iters, t3_ticks) {
                Stalled::Shipping => {
                    tracing::error!(
                        t2_iters,
                        "shipping thread stopped looping; stopping the run"
                    );
                    break ExitReason::ShippingThreadStalled;
                }
                Stalled::Context => {
                    tracing::error!(t3_ticks, "context thread stopped ticking; stopping the run");
                    break ExitReason::ContextThreadStalled;
                }
                Stalled::None => {}
            }
        }
        match deadline {
            Some(d) => shutdown.wait_until(d),
            None => shutdown.wait_timeout(STALL_CHECK_INTERVAL),
        };
    };
    tracing::info!(
        reason = exit_reason.as_str(),
        signal = signal_seen.get().map(|s| s.as_str()).unwrap_or("-"),
        "shutting down"
    );
    // Settle the reason before the workers tear down: the refresh T2 triggers
    // between the recording's last flush and the Kafka drain then already
    // carries it, instead of still saying "running".
    #[cfg(feature = "observability")]
    if let Some(s) = &sidecar {
        s.set_exit(exit_reason, false);
    }

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
    let mut joined_clean = true;
    if hwnd == 0 && thread_id == 0 && capture_alive.load(Ordering::Acquire) {
        // Nothing to post to and the thread claims to be running: detach rather
        // than block forever on a join that can never complete.
        tracing::error!("capture thread never published a handle; detaching it");
        drop(t1);
    } else {
        joined_clean &= join_loudly("capture", t1);
    }
    capture_stopped.store(true, Ordering::Release);
    // T2 may be in its idle park; do not make shutdown wait it out.
    waker.wake();
    joined_clean &= join_loudly("shipping", t2);
    joined_clean &= join_loudly("context", t3);

    let final_stats = stats.snapshot();
    tracing::info!(
        events = final_stats.events,
        batches = final_stats.batches,
        markers = final_stats.markers,
        ring_drops = final_stats.ring_drops,
        ring_high_water = final_stats.ring_high_water,
        abs_frames = final_stats.abs_frames,
        raw_read_errors = final_stats.raw_read_errors,
        udp_errors = final_stats.udp_errors,
        udp_unreachable = final_stats.udp_unreachable,
        udp_oversized = final_stats.udp_oversized,
        udp_would_block = final_stats.udp_would_block,
        jsonl_errors = final_stats.jsonl_errors,
        jsonl_flush_max_us = final_stats.jsonl_flush_max_us,
        jsonl_queued = final_stats.jsonl_queued,
        jsonl_dropped = final_stats.jsonl_dropped,
        jsonl_abandoned = final_stats.jsonl_abandoned,
        kafka_errors = final_stats.kafka_errors,
        kafka_queued = final_stats.kafka_queued,
        kafka_dropped = final_stats.kafka_dropped,
        kafka_abandoned = final_stats.kafka_abandoned,
        capture_to_ship_us_p50 = %stats::ReportPercentile::of(&final_stats.ship_latency_first, 0.50),
        capture_to_ship_us_p99 = %stats::ReportPercentile::of(&final_stats.ship_latency_first, 0.99),
        ship_tail_us_p99 = %stats::ReportPercentile::of(&final_stats.ship_latency_last, 0.99),
        "session finished"
    );

    // The same numbers, next to the recording, so a sink that lost batches
    // is visible in `telemouse-analyze list` and the control panel later —
    // not only in a log line that scrolled past. This is the rewrite after
    // the Kafka drain; T2 already wrote one before it.
    #[cfg(feature = "observability")]
    if let Some(s) = &sidecar {
        s.set_exit(exit_reason, joined_clean);
        if let Some(path) = s.refresh() {
            let losses = s.document().losses();
            if losses.is_empty() {
                tracing::info!(path = %path.display(), "session metadata written");
            } else {
                tracing::warn!(
                    path = %path.display(),
                    losses = ?losses,
                    "session metadata written; some sinks lost envelopes"
                );
            }
        }
    }

    // The recording is closed and the sidecar is on disk: a console handler
    // blocked on a close/logoff/shutdown may let Windows proceed.
    guard.finished();

    if !joined_clean {
        anyhow::bail!("a worker thread panicked; see the errors above");
    }
    Ok(())
}

fn cmd_doctor(args: DoctorArgs) -> Result<()> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let config_path = telemouse_core::paths::locate_config(&args.config);
    let cfg = resolve_config(&config_path, None)?;

    println!("telemouse {} — doctor", env!("CARGO_PKG_VERSION"));
    println!(
        "build             : {PROFILE}, features [{}]",
        enabled_features()
    );
    println!("os                : {}", std::env::consts::OS);
    println!(
        "os version        : {}",
        platform::os_version().unwrap_or_else(|| "unknown".into())
    );
    println!(
        "config            : {} ({})",
        config_path.display(),
        if config_path.exists() {
            "loaded"
        } else {
            "missing, using defaults"
        }
    );
    let overrides = cfg.non_default_fields();
    if overrides.is_empty() {
        println!("non-default       : (none — everything is at its default)");
    } else {
        println!("non-default       : {} setting(s)", overrides.len());
        for (field, value) in &overrides {
            println!("  {field:<18}: {value}");
        }
    }

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
            m.refresh_hz.map(|r| format!(" @{r}Hz")).unwrap_or_default(),
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
        if !cfg!(feature = "kafka") {
            "NOT BUILT IN (probing brokers anyway)"
        } else if cfg.kafka.enabled {
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
        assert_eq!(args.log_dir, None, "a log file is opt-in");
    }

    /// `--no-kafka` is accepted by every build, so the same command line
    /// works whether or not this one can talk to a broker.
    #[test]
    fn the_kafka_switch_is_accepted_whatever_the_build() {
        let Command::Run(args) = Cli::parse_from(["telemouse", "run", "--no-kafka"]).command else {
            panic!("expected run");
        };
        assert!(args.no_kafka);
        let mut cfg = AppConfig::default();
        cfg.kafka.enabled = true;
        apply_cli(&mut cfg, &args);
        assert!(!cfg.kafka.enabled);
    }

    #[test]
    fn a_log_directory_can_be_asked_for() {
        let Command::Run(args) = Cli::parse_from(["telemouse", "run", "--log-dir", "logs"]).command
        else {
            panic!("expected run");
        };
        assert_eq!(args.log_dir, Some(PathBuf::from("logs")));
    }

    /// Bare `telemouse` must not silently do nothing: with a console of its
    /// own it explains itself, and otherwise clap prints the help.
    #[test]
    fn no_subcommand_is_an_error_rather_than_a_silent_success() {
        let err = Cli::try_parse_from(["telemouse"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
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

    #[test]
    fn record_flag_forces_recording_on_and_excludes_no_record() {
        let missing = PathBuf::from("definitely-not-a-config-file.toml");
        let mut off = resolve_config(&missing, None).unwrap();
        off.recording.enabled = false;
        let Command::Run(args) = Cli::parse_from(["telemouse", "run", "--record"]).command else {
            panic!("expected run");
        };
        assert!(args.record && !args.no_record);
        apply_cli(&mut off, &args);
        assert!(
            off.recording.enabled,
            "--record overrides recording.enabled = false"
        );
        assert!(Cli::try_parse_from(["telemouse", "run", "--record", "--no-record"]).is_err());
    }

    /// A relative `recordings` in the config means "beside the config", not
    /// "beside whatever process started us" — which for a tray-launched agent
    /// is `C:\Windows\system32`.
    #[test]
    fn config_paths_resolve_against_the_config_file() {
        let dir = std::env::temp_dir().join(format!("telemouse-cfgpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("telemouse.toml");
        std::fs::write(&path, "[recording]\ndir = \"recordings\"\n").unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.recording.dir, dir.join("recordings"));
        assert!(cfg.recording.dir.is_absolute());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A config named but nowhere to be found still yields an absolute path,
    /// so every later message names one real location.
    #[test]
    fn a_missing_config_is_still_located_absolutely() {
        let located =
            telemouse_core::paths::locate_config(Path::new("definitely-not-a-config-file.toml"));
        assert!(located.is_absolute(), "{}", located.display());
        assert!(!located.exists());
        // And loading it is not an error: the defaults are a working agent.
        assert_eq!(load_config(&located).unwrap().mouse_cpi, 1600.0);
    }

    #[test]
    fn a_broken_config_refuses_to_start() {
        let dir = std::env::temp_dir().join(format!("telemouse-badcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("telemouse.toml");
        std::fs::write(&path, "mouse_cpi = -5.0\n").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("mouse_cpi"),
            "the error should name the field: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_banner_names_the_features_this_build_has() {
        let features = enabled_features();
        assert_eq!(cfg!(feature = "logging"), features.contains("logging"));
        assert_eq!(
            cfg!(feature = "observability"),
            features.contains("observability")
        );
        assert_eq!(cfg!(feature = "kafka"), features.contains("kafka"));
        // A default build lists all three, in a stable order.
        #[cfg(all(
            feature = "logging",
            feature = "observability",
            feature = "kafka",
            not(feature = "quiet")
        ))]
        assert_eq!(features, "logging,observability,kafka");
        #[cfg(not(any(
            feature = "logging",
            feature = "observability",
            feature = "kafka",
            feature = "quiet"
        )))]
        assert_eq!(features, "");
    }

    #[test]
    fn the_profile_is_the_one_this_build_was_compiled_with() {
        assert_eq!(
            PROFILE,
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
    }
}
