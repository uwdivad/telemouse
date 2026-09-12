//! `telemouse-ctl` — the control panel.
//!
//! One small HTTP server and one page: start and stop the capture agent and
//! the viz server, run the one-shot tools (doctor, analysis), watch what they
//! print, and see — and terminate — every telemouse process on the machine,
//! whoever started it.
//!
//! Children are launched from the binaries next to this one (or `[ctl]
//! bin_dir`), so a `cargo build --workspace` is all the setup there is.
//! `telemouse.toml` is looked for in the working directory first and next to
//! this executable second; when there is none the shipped sample is written
//! there, so an unzipped release runs without editing anything. Everything
//! the panel writes or launches is anchored to that file's directory, not to
//! whatever working directory a shortcut happened to have.

mod gui;
mod manager;
mod places;
mod procs;
mod server;
#[cfg(feature = "observability")]
mod stats;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use telemouse_core::config::AppConfig;
use tracing::{error, info, warn};

use crate::manager::{ConfigStatus, Manager, ManagerConfig};
use crate::places::Places;
use crate::procs::Scanner;
use crate::server::{AppState, PageConfig};

/// How often exited children are reaped while something is running. When
/// nothing is, the reaper sleeps until the manager reports a start.
const REAP_INTERVAL: Duration = Duration::from_millis(500);

/// How long a console close, logoff or shutdown may be held while the
/// children are stopped. Windows gives a console process about five seconds
/// before it is killed regardless; the fast stop is clamped below this.
const SIGNAL_MAX_WAIT: Duration = Duration::from_secs(4);

/// The sample config shipped with every release: `telemouse.example.toml`
/// at the workspace root (loopback everywhere, Kafka off). Written as
/// `telemouse.toml` on first start when none exists. The path reaches
/// outside this crate, so the crate builds from the workspace only.
const SAMPLE_CONFIG: &str = include_str!("../../../telemouse.example.toml");

/// What [`seed_config`] did; reported once the subscriber exists.
enum Seed {
    /// A file was already there (or something else is; `load` will say).
    Existing,
    /// The sample was written.
    Written,
    /// The sample could not be written (a directory in the way, a read-only
    /// location, ...); `load` decides what that means.
    Failed(std::io::Error),
}

/// Write the shipped sample to `path` unless something is already there.
/// `create_new` means an existing file is never touched, even one that
/// appears between a check and the write; a half-written file is removed
/// rather than left to fail parsing on the next start.
fn seed_config(path: &Path) -> Seed {
    use std::io::Write;
    match std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => match f
            .write_all(SAMPLE_CONFIG.as_bytes())
            .and_then(|()| f.flush())
        {
            Ok(()) => Seed::Written,
            Err(e) => {
                drop(f);
                let _ = std::fs::remove_file(path);
                Seed::Failed(e)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Seed::Existing,
        Err(e) => Seed::Failed(e),
    }
}

/// The features this binary was built with, for the startup line and the
/// page: the one fact a bug report needs and nobody thinks to include.
fn features() -> &'static str {
    match (cfg!(feature = "logging"), cfg!(feature = "observability")) {
        (true, true) => "logging,observability",
        (true, false) => "logging",
        (false, true) => "observability",
        (false, false) => "minimal",
    }
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

/// stderr plus `<log_dir>/ctl.log`. Started from Explorer the panel hides
/// its console, so without the file its own warnings — a child that died,
/// a request refused — would be written into a window nobody can see.
/// Colour only reaches a real terminal; the file never sees escape codes.
#[cfg(feature = "logging")]
fn init_logging(log_dir: Option<&Path>) -> Option<PathBuf> {
    let init = telemouse_core::logging::init(telemouse_core::logging::LogOptions {
        component: "ctl",
        log_dir,
        default_filter: "info",
    });
    if let Some(e) = &init.file_error {
        warn!(
            dir = %log_dir.map(|d| d.display().to_string()).unwrap_or_default(),
            error = %e,
            "cannot open ctl.log; logging to stderr only"
        );
    }
    init.file
}

/// Without the `logging` feature nothing subscribes to `tracing`: the panel
/// is silent by design, and the page is where its state shows.
#[cfg(not(feature = "logging"))]
fn init_logging(_log_dir: Option<&Path>) -> Option<PathBuf> {
    None
}

/// Real pixels for the status window and any geometry query, on a scaled
/// display. Must run before the first window is created; failure only means
/// Windows keeps virtualising, which is what happened before.
#[cfg(windows)]
fn dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // SAFETY: a plain process-wide setting with no pointers involved.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(windows))]
fn dpi_aware() {}

#[derive(Parser, Debug)]
#[command(
    name = "telemouse-ctl",
    version,
    about = "Control panel: start, stop and inspect telemouse processes"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve the control panel.
    Serve(ServeArgs),
}

#[derive(Parser, Debug, Default)]
struct ServeArgs {
    /// Path to telemouse.toml. A relative path is tried in the working
    /// directory, then next to this executable; if nothing is there the
    /// shipped sample (loopback only, Kafka off) is written first. Also
    /// handed to every component the panel launches.
    #[arg(long, default_value = "telemouse.toml")]
    config: PathBuf,
    /// Override `ctl.http_addr`: where the panel listens.
    #[arg(long, value_name = "ADDR")]
    http: Option<String>,
    /// Override `ctl.bin_dir`: where the telemouse binaries are.
    #[arg(long, value_name = "DIR")]
    bin_dir: Option<PathBuf>,
    /// Do not show the tray icon and status window (Windows only; the panel
    /// is always headless elsewhere).
    #[arg(long)]
    no_gui: bool,
    /// Override `ctl.log_dir`: where `ctl.log` and one `<component>.log` per
    /// launched component are written (builds with the `logging` feature).
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    dpi_aware();
    let cli = Cli::parse();
    let args = match cli.command {
        Some(Command::Serve(a)) => a,
        None => ServeArgs {
            config: PathBuf::from("telemouse.toml"),
            ..Default::default()
        },
    };
    let gui_mode = cfg!(windows) && !args.no_gui;

    // Where the config is decides where everything else is, so it is
    // located, seeded and read before the subscriber exists; what happened
    // is reported right after.
    let config = telemouse_core::paths::locate_config(&args.config);
    let base = telemouse_core::paths::config_base(&config);
    let seed = seed_config(&config);
    // Overrides are reported as the file spells them, before relative paths
    // are made absolute (an absolute `recordings` is not an override).
    let loaded = AppConfig::load_or_default(&config).map(|mut c| {
        let overrides = c.non_default_fields();
        c.resolve_paths(&base);
        (c, overrides)
    });
    let log_dir: Option<PathBuf> = cfg!(feature = "logging").then(|| {
        args.log_dir.clone().unwrap_or_else(|| {
            loaded
                .as_ref()
                .map(|(c, _)| c.ctl.log_dir.clone())
                .unwrap_or_else(|_| base.join("logs"))
        })
    });
    let log_file = init_logging(log_dir.as_deref());
    // A panic on the GUI thread or a pump task lands in ctl.log, which is
    // the only place a tray-launched panel can be seen.
    telemouse_core::panic_hook::install("ctl");

    match &seed {
        Seed::Existing => {}
        Seed::Written => info!(
            path = %config.display(),
            "no config found; wrote the shipped sample there (loopback only, Kafka off) — set mouse_cpi and your games in it"
        ),
        Seed::Failed(e) => warn!(
            error = %e,
            path = %config.display(),
            "could not write the sample config; using defaults if nothing is there"
        ),
    }
    let (cfg, overrides) = match loaded {
        Ok(c) => c,
        Err(e) => {
            // A file that exists but cannot be read is refused: running on
            // defaults would silently move the viz back to loopback, forget
            // the recordings directory and drop the hotkey.
            error!(error = %e, path = %config.display(), "telemouse.toml is unreadable; fix it or delete it (the sample is written back on the next start)");
            if gui_mode {
                gui::alert(
                    "telemouse-ctl: telemouse.toml is unreadable",
                    &format!(
                        "{e}\n\nFix the file or delete it; the shipped sample is written back on the next start."
                    ),
                );
            }
            return Err(e).context("load telemouse.toml");
        }
    };
    let config_status = match seed {
        Seed::Written => ConfigStatus::Seeded,
        _ if config.is_file() => ConfigStatus::Loaded,
        _ => ConfigStatus::Defaults,
    };
    info!(
        version = places::VERSION,
        profile = profile(),
        features = features(),
        config = %config.display(),
        config_found = config.is_file(),
        config_status = ?config_status,
        log_file = %log_file.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into()),
        overrides = ?overrides,
        "telemouse-ctl starting"
    );
    serve(args, cfg, config, config_status, log_dir, gui_mode).await
}

async fn serve(
    args: ServeArgs,
    cfg: AppConfig,
    config: PathBuf,
    config_status: ConfigStatus,
    log_dir: Option<PathBuf>,
    gui_mode: bool,
) -> Result<()> {
    let http_addr_s = args.http.unwrap_or(cfg.ctl.http_addr);
    let http_addr: SocketAddr = http_addr_s
        .parse()
        .with_context(|| format!("invalid --http address {http_addr_s:?}"))?;
    if !http_addr.ip().is_loopback() {
        // The panel can terminate processes: it is a local tool by design.
        warn!(%http_addr, "control panel is listening on a non-loopback address; anyone who can reach it can stop capture and kill telemouse processes");
    }
    let bin_dir = args.bin_dir.or(cfg.ctl.bin_dir);

    let manager = Arc::new(Manager::new(
        manager::COMPONENTS,
        ManagerConfig {
            bin_dir: bin_dir.clone(),
            config_path: config.clone(),
            recordings_dir: cfg.recording.dir.clone(),
            recording_enabled: cfg.recording.enabled,
            config_status,
            grace: Duration::from_secs(cfg.ctl.stop_grace_secs),
            log_dir: log_dir.clone(),
        },
    ));

    // Every place the panel talks about, decided once so the header, the
    // tray menu and the page cannot disagree.
    let exe_dir = telemouse_core::paths::exe_dir();
    let places = Places {
        version: places::VERSION.into(),
        panel_url: gui::model::panel_url(http_addr),
        config: config.display().to_string(),
        logs: log_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        bin_dir: bin_dir
            .as_ref()
            .or(exe_dir.as_ref())
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        docs: Places::docs_target(exe_dir.as_deref()),
        releases: places::RELEASES_URL.into(),
    };

    let state = AppState {
        manager: manager.clone(),
        scanner: Arc::new(Scanner::new()),
        page: Arc::new(server::render_page(&PageConfig {
            // The page links to the viz; a wildcard bind is not a URL a
            // browser will open, so link to the loopback of the same family.
            viz_http: telemouse_core::localhost::browse_addr_str(&cfg.viz.http_addr),
            stop_grace_secs: cfg.ctl.stop_grace_secs,
            features: features().into(),
            places: places.clone(),
        })),
        places: Arc::new(places.clone()),
    };

    // Reap exited children while anything runs; when nothing does, sleep
    // until the manager reports a start instead of ticking at an idle panel.
    {
        let m = manager.clone();
        tokio::spawn(async move {
            loop {
                if !m.any_running().await {
                    m.work().notified().await;
                }
                tokio::time::sleep(REAP_INTERVAL).await;
                m.reap().await;
            }
        });
    }

    let listener = match tokio::net::TcpListener::bind(http_addr).await {
        Ok(l) => l,
        Err(e) => {
            // The most likely first-run failure: a panel is already running.
            // Explorer closes the console with the process, so say it where
            // it can be seen, and take the user to the panel that answers.
            error!(error = %e, %http_addr, "failed to bind the control panel port");
            if gui_mode && e.kind() == std::io::ErrorKind::AddrInUse {
                let url = &places.panel_url;
                let opened = gui::open_url(url);
                gui::alert(
                    "telemouse-ctl is already running",
                    &format!(
                        "Port {} is in use, most likely by another telemouse-ctl.\n\n{}\n\nTo run a second panel start it with --http 127.0.0.1:<other port>.",
                        http_addr.port(),
                        if opened {
                            format!("Opened {url} in your browser.")
                        } else {
                            format!("Open {url} in your browser.")
                        }
                    ),
                );
                return Ok(());
            }
            if gui_mode {
                gui::alert(
                    "telemouse-ctl could not start",
                    &format!(
                        "Could not listen on {http_addr}:\n{e}\n\nChange [ctl] http_addr in telemouse.toml or start with --http."
                    ),
                );
            }
            return Err(e).with_context(|| format!("failed to bind http {http_addr}"));
        }
    };

    for c in manager::COMPONENTS {
        let (p, found) = manager.resolve_bin(c.bin);
        if !found {
            warn!(component = c.id, path = %p.display(), "binary not found — build it (cargo build --workspace) or set [ctl] bin_dir");
        }
    }
    info!(
        http = %http_addr,
        config = %config.display(),
        bin_dir = %bin_dir.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(next to telemouse-ctl, then PATH)".into()),
        logs = %places.logs,
        "telemouse-ctl serving; control panel {}", places.panel_url
    );

    // The tray's Exit item ends up here too.
    let quit = Arc::new(tokio::sync::Notify::new());
    // `validate` already rejected a bad chord at load; this only guards the
    // defaults path.
    let hotkey = match telemouse_core::hotkey::Hotkey::parse(&cfg.ctl.hotkey) {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "ctl.hotkey ignored");
            None
        }
    };
    let gui = if gui_mode {
        gui::spawn(gui::GuiDeps {
            handle: tokio::runtime::Handle::current(),
            manager: manager.clone(),
            scanner: state.scanner.clone(),
            quit: quit.clone(),
            hotkey,
            places: places.clone(),
        })
    } else {
        None
    };

    // Console signals. Ctrl-C and Ctrl-Break are the ordinary stop; a closed
    // console, a logoff or a shutdown are "terminal": Windows kills the
    // process the moment the handler returns, so the handler blocks until
    // the children have been stopped (`finished`) or `SIGNAL_MAX_WAIT`.
    let signalled = Arc::new(tokio::sync::Notify::new());
    let terminal = Arc::new(AtomicBool::new(false));
    let signal_name = Arc::new(std::sync::Mutex::new(""));
    let guard = {
        let (n, t, name) = (signalled.clone(), terminal.clone(), signal_name.clone());
        match telemouse_core::shutdown::install(
            move |sig| {
                if sig.is_terminal() {
                    t.store(true, Ordering::Release);
                }
                if let Ok(mut g) = name.lock() {
                    *g = sig.as_str();
                }
                n.notify_one();
            },
            SIGNAL_MAX_WAIT,
        ) {
            Ok(g) => Some(g),
            Err(e) => {
                warn!(error = %e, "console signal handler unavailable; use the tray's Exit to stop");
                None
            }
        }
    };

    // Children live in their own process groups, so a console signal here
    // does not reach them: stop them ourselves before leaving. Every exit
    // path logs its reason, because "the logs just stop" was how the last
    // one looked.
    let shutdown = {
        let m = manager.clone();
        let guard = guard.clone();
        async move {
            tokio::select! {
                _ = signalled.notified() => {
                    let name = signal_name.lock().map(|g| *g).unwrap_or("signal");
                    info!(signal = name, "shutting down on a console signal");
                }
                _ = quit.notified() => info!("shutting down: exit requested from the tray"),
            }
            if terminal.load(Ordering::Acquire) {
                // The session is ending: stop fast, inside the OS's window.
                info!(
                    "stopping managed components (fast: the console is closing or the session is ending)"
                );
                m.stop_all_fast().await;
            } else {
                info!("stopping managed components");
                m.stop_all().await;
            }
            if let Some(g) = guard {
                g.finished();
            }
        }
    };

    let served = axum::serve(listener, server::router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("http server failed");
    // Only now: the icon should disappear once the children are gone.
    if let Some(g) = gui {
        g.shutdown();
    }
    info!("telemouse-ctl stopped");
    served
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
            "telemouse-ctl",
            "serve",
            "--config",
            "c.toml",
            "--http",
            "127.0.0.1:9001",
            "--bin-dir",
            "target/release",
        ]);
        let Some(Command::Serve(a)) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(a.config, PathBuf::from("c.toml"));
        assert_eq!(a.http.as_deref(), Some("127.0.0.1:9001"));
        assert_eq!(a.bin_dir, Some(PathBuf::from("target/release")));
        assert!(!a.no_gui, "the gui is on by default");
        assert!(
            a.log_dir.is_none(),
            "log dir comes from the config by default"
        );
    }

    #[test]
    fn log_dir_flag_parses() {
        let cli = Cli::parse_from(["telemouse-ctl", "serve", "--log-dir", "D:/tm/logs"]);
        let Some(Command::Serve(a)) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(a.log_dir, Some(PathBuf::from("D:/tm/logs")));
    }

    #[test]
    fn no_gui_flag_parses() {
        let cli = Cli::parse_from(["telemouse-ctl", "serve", "--no-gui"]);
        let Some(Command::Serve(a)) = cli.command else {
            panic!("expected serve");
        };
        assert!(a.no_gui);
    }

    #[test]
    fn no_subcommand_defaults_to_serve() {
        assert!(Cli::parse_from(["telemouse-ctl"]).command.is_none());
    }

    #[test]
    fn features_string_names_this_build() {
        let f = features();
        assert_eq!(f.contains("logging"), cfg!(feature = "logging"));
        assert_eq!(f.contains("observability"), cfg!(feature = "observability"));
    }

    /// The sample is what a fresh install runs on: it must load, it must be
    /// the compiled defaults apart from the two example games, and it must
    /// talk to nobody but this machine.
    #[test]
    fn shipped_sample_is_the_defaults_and_loopback_only() {
        let d = manager::tmpdir("sample");
        let p = d.join("telemouse.toml");
        std::fs::write(&p, SAMPLE_CONFIG).unwrap();
        let c = AppConfig::load(&p).expect("telemouse.example.toml must parse and validate");
        assert_eq!(
            AppConfig {
                games: Default::default(),
                ..c.clone()
            },
            AppConfig::default(),
            "the sample must be the compiled defaults apart from [games]"
        );
        assert_eq!(c.games.len(), 2, "two example games");
        assert!(
            !c.kafka.enabled,
            "a fresh install must not look for a broker"
        );
        for (name, addr) in [
            ("udp.addr", &c.udp.addr),
            ("viz.http_addr", &c.viz.http_addr),
            ("ctl.http_addr", &c.ctl.http_addr),
        ] {
            let sa: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(sa.ip().is_loopback(), "{name} = {addr} is not loopback");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn seed_writes_sample_once_and_never_overwrites() {
        let d = manager::tmpdir("seed");
        let p = d.join("telemouse.toml");
        assert!(matches!(seed_config(&p), Seed::Written));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), SAMPLE_CONFIG);

        let edited = "mouse_cpi = 800.0\n";
        std::fs::write(&p, edited).unwrap();
        assert!(matches!(seed_config(&p), Seed::Existing));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            edited,
            "a user's config is never overwritten"
        );

        let nowhere = d.join("missing").join("telemouse.toml");
        assert!(matches!(seed_config(&nowhere), Seed::Failed(_)));
        assert!(!nowhere.exists(), "seeding never creates directories");

        // A directory in the way is "failed", not "existing": load() will
        // then say what is actually there.
        let dir_in_the_way = d.join("as-dir");
        std::fs::create_dir_all(&dir_in_the_way).unwrap();
        assert!(matches!(seed_config(&dir_in_the_way), Seed::Failed(_)));
        let _ = std::fs::remove_dir_all(&d);
    }
}
