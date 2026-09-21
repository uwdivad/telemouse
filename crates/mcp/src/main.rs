//! `telemouse-mcp` — telemouse as a set of MCP tools.
//!
//! One stdio Model Context Protocol server. Read-only tools answer from the
//! analyzer library and from `GET`s against the control panel and the viz
//! server; control tools are proxies over the panel's HTTP API, so ctl's
//! allow-lists stay the only thing that decides what may start, stop or be
//! terminated. Nothing here opens a handle on a foreign process, hooks
//! anything, or writes outside telemouse's own folders.
//!
//! **stdout is the protocol.** Every JSON-RPC frame goes there and nothing
//! else ever may, so this binary has no `println!` and logging goes to
//! stderr and, in a build with the `logging` feature, to
//! `<log_dir>/mcp.log` — the same two destinations every other binary has.
//!
//! # Adding a second transport
//!
//! The transport is the last two lines of [`main`], and the tool set
//! ([`server::Telemouse`]) knows nothing about it. A streamable-HTTP
//! transport is therefore: enable rmcp's `transport-streamable-http-server`
//! feature, read an `[mcp] http_addr` and an `[mcp] token` from
//! `telemouse.toml`, and serve the same [`server::Telemouse`] over
//! `StreamableHttpService` instead of (or beside) `stdio()`. The design rule
//! that goes with it is in `docs/AGENTIC-2026-09-13.md`, *Remote*: ctl and
//! viz keep their loopback bind, the MCP server is the only thing ever
//! exposed, a token is required, TLS comes from a tunnel, and `kill` is
//! never remote. That is why [`server::Deps::read_only`] exists as a
//! property of the server rather than of the process: a second transport
//! gets its own tool profile without touching a tool body.

mod analysis;
mod args;
mod digest;
mod http;
mod local;
mod server;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use telemouse_core::config::AppConfig;
use tracing::{info, warn};

use crate::analysis::Analysis;
use crate::local::Local;
use crate::server::{Deps, Telemouse};

/// The features this binary was built with, for the startup line — the one
/// fact a bug report needs and nobody thinks to include.
fn features() -> &'static str {
    match (cfg!(feature = "logging"), cfg!(feature = "observability")) {
        (true, true) => "logging,observability",
        (true, false) => "logging",
        (false, true) => "observability",
        (false, false) => "minimal",
    }
}

/// stderr plus `<log_dir>/mcp.log`. Never stdout: that is the MCP channel,
/// and one stray line on it desynchronises the client's JSON-RPC framing.
#[cfg(feature = "logging")]
fn init_logging(log_dir: Option<&Path>) -> Option<PathBuf> {
    let init = telemouse_core::logging::init(telemouse_core::logging::LogOptions {
        component: "mcp",
        log_dir,
        default_filter: "info",
    });
    if let Some(e) = &init.file_error {
        warn!(
            dir = %log_dir.map(|d| d.display().to_string()).unwrap_or_default(),
            error = %e,
            "cannot open mcp.log; logging to stderr only"
        );
    }
    init.file
}

#[cfg(not(feature = "logging"))]
fn init_logging(_log_dir: Option<&Path>) -> Option<PathBuf> {
    None
}

#[derive(Parser, Debug)]
#[command(
    name = "telemouse-mcp",
    version,
    about = "MCP server: telemouse sessions and processes as tools, over stdio"
)]
struct Cli {
    /// Path to telemouse.toml. A relative path is tried in the working
    /// directory, then next to this executable — the same rule every other
    /// binary follows.
    #[arg(long, default_value = "telemouse.toml")]
    config: PathBuf,
    /// Offer only the tools that read. `capture_start`, `capture_stop`,
    /// `marker`, `doctor` and `kill` are then not in `tools/list` at all.
    #[arg(long)]
    read_only: bool,
    /// Override `[recording] dir`: where the recordings are.
    #[arg(long, value_name = "DIR")]
    recordings: Option<PathBuf>,
    /// Override `[ctl] http_addr`: where the control panel listens.
    #[arg(long, value_name = "ADDR")]
    ctl: Option<String>,
    /// Override `[viz] http_addr`: where the viz server listens.
    #[arg(long, value_name = "ADDR")]
    viz: Option<String>,
    /// Override `[ctl] log_dir`: where `logs_tail` looks for log files.
    #[arg(long, value_name = "DIR")]
    log_dir: Option<PathBuf>,
}

/// Where to connect for a server bound to `bind`.
///
/// A wildcard bind is not an address to dial, so it becomes the loopback of
/// the same family — the same translation the panel page's link uses.
fn dial(bind: &str, what: &str) -> Result<SocketAddr> {
    let s = telemouse_core::localhost::browse_addr_str(bind);
    s.parse()
        .with_context(|| format!("{what} is not an address this tool can dial: {bind:?}"))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Where the config is decides where everything else is, so it is found
    // and read before the subscriber exists; what happened is reported
    // right after.
    let config = telemouse_core::paths::locate_config(&cli.config);
    let base = telemouse_core::paths::config_base(&config);
    // A config that exists but does not parse is refused, never silently
    // replaced by defaults: running on defaults would point this server at
    // the wrong recordings folder and the wrong ports and say nothing.
    let mut cfg = AppConfig::load_or_default(&config)
        .with_context(|| format!("load {}", config.display()))?;
    cfg.resolve_paths(&base);

    let log_dir: Option<PathBuf> = cfg!(feature = "logging").then(|| {
        cli.log_dir
            .clone()
            .unwrap_or_else(|| cfg.ctl.log_dir.clone())
    });
    let log_file = init_logging(log_dir.as_deref());
    telemouse_core::panic_hook::install("mcp");

    let recordings = cli.recordings.unwrap_or_else(|| cfg.recording.dir.clone());
    let ctl_addr = dial(
        cli.ctl.as_deref().unwrap_or(&cfg.ctl.http_addr),
        "[ctl] http_addr",
    )?;
    let viz_addr = dial(
        cli.viz.as_deref().unwrap_or(&cfg.viz.http_addr),
        "[viz] http_addr",
    )?;

    let telemouse = Telemouse::new(Deps {
        analysis: Analysis::new(recordings.clone()),
        ctl: Local::ctl(ctl_addr),
        viz: Local::viz(viz_addr),
        log_dir: log_dir.clone(),
        read_only: cli.read_only,
    });

    info!(
        version = env!("CARGO_PKG_VERSION"),
        features = features(),
        config = %config.display(),
        config_found = config.is_file(),
        recordings = %recordings.display(),
        ctl = %ctl_addr,
        viz = %viz_addr,
        log_file = %log_file.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into()),
        read_only = cli.read_only,
        tools = ?telemouse.tool_names(),
        "telemouse-mcp serving over stdio"
    );
    if !recordings.is_dir() {
        // Not fatal: the panel may create it on the next recording, and
        // every other tool still works.
        warn!(dir = %recordings.display(), "the recordings directory is not there yet; sessions_list and trend will be empty");
    }

    // The transport, and the only place in this binary that owns stdout.
    let service = telemouse
        .serve(stdio())
        .await
        .context("start the MCP stdio transport")?;
    let reason = service.waiting().await.context("MCP session failed")?;
    info!(?reason, "telemouse-mcp stopped");
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
    fn the_defaults_are_the_repo_layout() {
        let cli = Cli::parse_from(["telemouse-mcp"]);
        assert_eq!(cli.config, PathBuf::from("telemouse.toml"));
        assert!(!cli.read_only, "the control tools are on by default");
        assert!(cli.ctl.is_none() && cli.viz.is_none() && cli.recordings.is_none());
    }

    #[test]
    fn every_override_parses() {
        let cli = Cli::parse_from([
            "telemouse-mcp",
            "--config",
            "c.toml",
            "--read-only",
            "--recordings",
            "D:/tm/recordings",
            "--ctl",
            "127.0.0.1:9880",
            "--viz",
            "127.0.0.1:9879",
            "--log-dir",
            "D:/tm/logs",
        ]);
        assert_eq!(cli.config, PathBuf::from("c.toml"));
        assert!(cli.read_only);
        assert_eq!(cli.recordings, Some(PathBuf::from("D:/tm/recordings")));
        assert_eq!(cli.ctl.as_deref(), Some("127.0.0.1:9880"));
        assert_eq!(cli.viz.as_deref(), Some("127.0.0.1:9879"));
        assert_eq!(cli.log_dir, Some(PathBuf::from("D:/tm/logs")));
    }

    #[test]
    fn a_wildcard_bind_is_dialled_on_loopback() {
        assert_eq!(
            dial("0.0.0.0:7879", "[viz] http_addr").unwrap(),
            "127.0.0.1:7879".parse::<SocketAddr>().unwrap(),
            "the viz on this machine binds the wildcard for an OBS PC; we still dial it locally"
        );
        assert_eq!(
            dial("127.0.0.1:7880", "[ctl] http_addr").unwrap(),
            "127.0.0.1:7880".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            dial("[::]:7879", "[viz] http_addr").unwrap(),
            "[::1]:7879".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn an_address_that_cannot_be_dialled_says_which_key_it_came_from() {
        let e = dial("not an address", "[ctl] http_addr").unwrap_err();
        assert!(e.to_string().contains("[ctl] http_addr"), "{e}");
    }

    #[test]
    fn features_string_names_this_build() {
        let f = features();
        assert_eq!(f.contains("logging"), cfg!(feature = "logging"));
        assert_eq!(f.contains("observability"), cfg!(feature = "observability"));
    }
}
