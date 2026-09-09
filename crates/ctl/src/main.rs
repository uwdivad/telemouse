//! `telemouse-ctl` — the control panel.
//!
//! One small HTTP server and one page: start and stop the capture agent and
//! the viz server, run the one-shot tools (doctor, analysis), watch what they
//! print, and see — and terminate — every telemouse process on the machine,
//! whoever started it.
//!
//! Children are launched from the binaries next to this one (or `[ctl]
//! bin_dir`), so a `cargo build --workspace` is all the setup there is.

mod gui;
mod manager;
mod procs;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use telemouse_core::config::AppConfig;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::manager::{Manager, ManagerConfig};
use crate::procs::Scanner;
use crate::server::{AppState, PageConfig};

/// How often exited children are reaped when nobody is looking at the page.
const REAP_INTERVAL: Duration = Duration::from_millis(500);

/// stderr plus `<log_dir>/ctl.log`. Started from Explorer the panel hides
/// its console, so without the file its own warnings — a child that died,
/// a request refused — would be written into a window nobody can see.
fn init_tracing(log_dir: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file = manager::open_log(log_dir, "ctl");
    let file_layer = file.as_ref().ok().map(|f| {
        f.try_clone().ok().map(|f| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(f))
        })
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer.flatten())
        .init();
    match file {
        Ok(_) => info!(path = %log_dir.join("ctl.log").display(), "logging to file"),
        Err(e) => {
            warn!(dir = %log_dir.display(), error = %e, "cannot open log file; logging to stderr only")
        }
    }
}

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
    /// Path to telemouse.toml (defaults are used if it does not exist).
    /// Also handed to every component the panel launches.
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
    /// launched component are written.
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let args = match cli.command {
        Some(Command::Serve(a)) => a,
        None => ServeArgs {
            config: PathBuf::from("telemouse.toml"),
            ..Default::default()
        },
    };
    // The config decides where logs go, so it is read before the subscriber
    // exists; its own failure is reported right after.
    let cfg = AppConfig::load_or_default(&args.config);
    let log_dir = args.log_dir.clone().unwrap_or_else(|| {
        cfg.as_ref()
            .map(|c| c.ctl.log_dir.clone())
            .unwrap_or_else(|_| PathBuf::from("logs"))
    });
    init_tracing(&log_dir);
    // A panic on the GUI thread or a pump task lands in ctl.log, which is
    // the only place a tray-launched panel can be seen.
    telemouse_core::panic_hook::install("ctl");
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, path = %args.config.display(), "config unreadable; using defaults");
            AppConfig::default()
        }
    };
    serve(args, cfg, log_dir).await
}

async fn serve(args: ServeArgs, cfg: AppConfig, log_dir: PathBuf) -> Result<()> {
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
            config_path: args.config.clone(),
            recordings_dir: cfg.recording.dir.clone(),
            recording_enabled: cfg.recording.enabled,
            grace: Duration::from_secs(cfg.ctl.stop_grace_secs),
            log_dir: Some(log_dir.clone()),
        },
    ));
    let state = AppState {
        manager: manager.clone(),
        scanner: Arc::new(Scanner::new()),
        page: Arc::new(server::render_page(&PageConfig {
            // The page links to the viz; a wildcard bind is not a URL a
            // browser will open, so link to the loopback of the same family.
            viz_http: telemouse_core::localhost::browse_addr_str(&cfg.viz.http_addr),
            stop_grace_secs: cfg.ctl.stop_grace_secs,
        })),
    };

    {
        let m = manager.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(REAP_INTERVAL);
            loop {
                t.tick().await;
                m.reap().await;
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("failed to bind http {http_addr}"))?;

    for c in manager::COMPONENTS {
        let (p, found) = manager.resolve_bin(c.bin);
        if !found {
            warn!(component = c.id, path = %p.display(), "binary not found — build it (cargo build --workspace) or set [ctl] bin_dir");
        }
    }
    info!(
        http = %http_addr,
        config = %args.config.display(),
        bin_dir = %bin_dir.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(next to telemouse-ctl, then PATH)".into()),
        logs = %log_dir.display(),
        "telemouse-ctl serving; control panel http://{http_addr}/"
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
    let gui = if cfg!(windows) && !args.no_gui {
        gui::spawn(gui::GuiDeps {
            handle: tokio::runtime::Handle::current(),
            manager: manager.clone(),
            scanner: state.scanner.clone(),
            http_addr: listener.local_addr().unwrap_or(http_addr),
            quit: quit.clone(),
            hotkey,
        })
    } else {
        None
    };

    // Children live in their own process groups, so Ctrl-C here does not
    // reach them: stop them ourselves before leaving.
    let shutdown = {
        let m = manager.clone();
        async move {
            tokio::select! {
                r = tokio::signal::ctrl_c() => {
                    if let Err(e) = r {
                        warn!(error = %e, "ctrl-c handler unavailable; use the tray's Exit (or kill) to stop");
                        std::future::pending::<()>().await;
                    }
                }
                _ = quit.notified() => info!("exit requested from the tray"),
            }
            info!("shutting down; stopping managed components");
            m.stop_all().await;
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
}
