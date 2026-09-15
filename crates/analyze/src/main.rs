//! `telemouse-analyze` — the CLI over the metrics engine in the library.
//!
//! Thin by design: parse arguments, load, hand off to
//! [`telemouse_analyze::report::build`], render. All the maths lives in the
//! library, where it is unit-tested.

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use telemouse_analyze::{
    load,
    series::Params,
    timefmt::{format_duration, format_utc_us},
    trend,
};

/// Install the shared subscriber, when this build has one.
///
/// Without the `logging` feature the crate still emits `tracing` events and
/// nothing subscribes to them — which is what a packaged build wants, and is
/// the only difference the feature makes.
fn init_logging() {
    #[cfg(feature = "logging")]
    telemouse_core::logging::init(telemouse_core::logging::LogOptions {
        component: "analyze",
        log_dir: None,
        default_filter: "info",
    });
}

#[derive(Parser, Debug)]
#[command(
    name = "telemouse-analyze",
    about = "Offline metrics over recorded telemouse sessions",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Every [`Params`] field the CLI can override. Defaults come from
/// `Params::default()`, so an unset flag never invents a value here.
#[derive(Args, Debug, Default)]
struct ParamFlags {
    /// Uniform resampling grid width, seconds.
    #[arg(long, value_name = "SECONDS")]
    grid_dt: Option<f64>,
    /// Savitzky–Golay half-window, in grid cells.
    #[arg(long, value_name = "CELLS")]
    sg_half: Option<usize>,
    /// Savitzky–Golay polynomial order.
    #[arg(long, value_name = "ORDER")]
    sg_order: Option<usize>,
    /// Speed a movement must reach to count as a flick, counts/s.
    #[arg(long, value_name = "COUNTS_PER_S")]
    flick_threshold: Option<f64>,
    /// Speed below which the hand counts as still, counts/s.
    #[arg(long, value_name = "COUNTS_PER_S")]
    still_threshold: Option<f64>,
    /// How long speed must stay below the still threshold to settle, ms.
    #[arg(long, value_name = "MS")]
    still_hold_ms: Option<u64>,
    /// Flick start → button-down search window, ms.
    #[arg(long, value_name = "MS")]
    click_window_ms: Option<f64>,
    /// Start of the pre-click stability window, ms before the down.
    #[arg(long, value_name = "MS")]
    pre_click_lo_ms: Option<f64>,
    /// End of the pre-click stability window, ms before the down.
    #[arg(long, value_name = "MS")]
    pre_click_hi_ms: Option<f64>,
    /// Longest gap between two downs that still reads as a double click, ms.
    #[arg(long, value_name = "MS")]
    double_click_max_ms: Option<f64>,
    /// Shortest run above the still threshold that counts as a movement, ms.
    #[arg(long, value_name = "MS")]
    min_segment_ms: Option<usize>,
    /// How long a reversed velocity must persist to count as a correction, ms.
    #[arg(long, value_name = "MS")]
    min_reversal_ms: Option<usize>,
    /// Boxcar width used as the tremor high-pass baseline, ms.
    #[arg(long, value_name = "MS")]
    tremor_baseline_ms: Option<usize>,
    /// Safety cap on grid size, cells.
    #[arg(long, value_name = "CELLS")]
    max_grid_cells: Option<usize>,
    /// Shortest drift that can open a repositioning lift, ms.
    #[arg(long, value_name = "MS")]
    lift_drift_min_ms: Option<usize>,
    /// Fastest a drift may be and still read as running out of pad, counts/s.
    #[arg(long, value_name = "COUNTS_PER_S")]
    lift_drift_max_speed: Option<f64>,
    /// Shortest drift displacement that can open a lift, counts.
    #[arg(long, value_name = "COUNTS")]
    lift_drift_min_counts: Option<f64>,
    /// Slowest return sweep that still closes a lift, counts/s.
    #[arg(long, value_name = "COUNTS_PER_S")]
    lift_return_min_speed: Option<f64>,
    /// Longest stillness between drift and return, ms.
    #[arg(long, value_name = "MS")]
    lift_max_gap_ms: Option<usize>,
    /// How opposed the return must be, as a cosine (−1 = exactly reversed).
    #[arg(long, value_name = "COS", allow_negative_numbers = true)]
    lift_opposite_cos: Option<f64>,
    /// Restrict degree-valued metrics to pointer-locked spans.
    #[arg(long)]
    locked_only: bool,
    /// Print the per-marker-interval breakdown.
    #[arg(long)]
    split_by_marker: bool,
}

impl ParamFlags {
    fn apply(&self) -> Params {
        let mut p = Params::default();
        macro_rules! set {
            ($field:ident, $flag:ident) => {
                if let Some(v) = self.$flag {
                    p.$field = v;
                }
            };
        }
        set!(grid_dt_s, grid_dt);
        set!(sg_half, sg_half);
        set!(sg_order, sg_order);
        set!(flick_speed, flick_threshold);
        set!(still_speed, still_threshold);
        set!(still_hold_ms, still_hold_ms);
        set!(click_window_ms, click_window_ms);
        set!(pre_click_lo_ms, pre_click_lo_ms);
        set!(pre_click_hi_ms, pre_click_hi_ms);
        set!(double_click_max_ms, double_click_max_ms);
        set!(min_segment_ms, min_segment_ms);
        set!(min_reversal_ms, min_reversal_ms);
        set!(tremor_baseline_ms, tremor_baseline_ms);
        set!(max_grid_cells, max_grid_cells);
        set!(lift_drift_min_ms, lift_drift_min_ms);
        set!(lift_drift_max_speed, lift_drift_max_speed);
        set!(lift_drift_min_counts, lift_drift_min_counts);
        set!(lift_return_min_speed, lift_return_min_speed);
        set!(lift_max_gap_ms, lift_max_gap_ms);
        set!(lift_opposite_cos, lift_opposite_cos);
        p.locked_only = self.locked_only;
        p.split_by_marker = self.split_by_marker;
        p
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Analyze one recording and print the metrics summary.
    Report {
        /// Path to a `recordings/<session_id>.jsonl` file, or a bare
        /// session id looked up in `--dir`.
        session: String,
        /// Where a bare session id is looked up.
        #[arg(long, default_value = "recordings", value_name = "DIR")]
        dir: PathBuf,
        /// Print the headline numbers as JSON instead of the terminal
        /// summary: a few KB that says what the full report says.
        #[arg(long)]
        summary: bool,
        /// Also write the full structured report here.
        #[arg(long, value_name = "FILE")]
        json: Option<PathBuf>,
        /// Cache directory for `<session_id>.report.json`. A cached report is
        /// reused when it was written by this analyzer version and the
        /// recording has not changed since.
        #[arg(long, value_name = "DIR")]
        json_dir: Option<PathBuf>,
        /// Also write the derived-table CSVs into this directory.
        #[arg(long, value_name = "DIR")]
        csv_dir: Option<PathBuf>,
        /// Print the per-phase timing table at the end.
        #[arg(long)]
        timing: bool,
        /// Suppress the terminal summary (useful with --json).
        #[arg(long)]
        quiet: bool,
        #[command(flatten)]
        params: ParamFlags,
    },
    /// One row per session across a directory of recordings.
    Trend {
        #[arg(long, default_value = "recordings", value_name = "DIR")]
        dir: PathBuf,
        /// Read and write cached per-session reports here.
        #[arg(long, value_name = "DIR")]
        json_dir: Option<PathBuf>,
        /// Extra dotted metric paths to add as columns, e.g.
        /// `micro.band_ratio_8_12`. Repeatable.
        #[arg(long, value_name = "PATH")]
        metric: Vec<String>,
        /// Write the table as CSV here.
        #[arg(long, value_name = "FILE")]
        csv: Option<PathBuf>,
        /// Emit the table as JSON instead of the terminal rendering.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        params: ParamFlags,
    },
    /// List the recordings in a directory.
    List {
        #[arg(long, default_value = "recordings", value_name = "DIR")]
        dir: PathBuf,
        /// Emit the listing as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// The recording `arg` names: an existing file, or a session id (with or
/// without `.jsonl`) under `dir`. Tools that know a session only by the id
/// the panel and `list` show should not have to know the recordings dir.
fn resolve_session(arg: &str, dir: &std::path::Path) -> Result<PathBuf> {
    let as_path = PathBuf::from(arg);
    if as_path.is_file() {
        return Ok(as_path);
    }
    let id = arg.strip_suffix(".jsonl").unwrap_or(arg);
    match telemouse_core::recordings::recording_file_name(id) {
        Some(name) => {
            let in_dir = dir.join(name);
            if in_dir.is_file() {
                Ok(in_dir)
            } else {
                anyhow::bail!(
                    "no recording {arg}: neither {} nor {} exists",
                    as_path.display(),
                    in_dir.display()
                )
            }
        }
        None => anyhow::bail!(
            "no recording {arg}: {} is not a file and {arg:?} is not a session id",
            as_path.display()
        ),
    }
}

fn main() -> Result<()> {
    init_logging();
    telemouse_core::panic_hook::install("analyze");

    let started = Instant::now();
    match Cli::parse().cmd {
        Cmd::Report {
            session,
            dir,
            summary,
            json,
            json_dir,
            csv_dir,
            timing,
            quiet,
            params,
        } => {
            let params = params.apply();
            let session = resolve_session(&session, &dir)?;

            let t = Instant::now();
            let (mut report, cache) = trend::report_for(&session, json_dir.as_deref(), params)
                .with_context(|| format!("analyzing {}", session.display()))?;
            if cache == trend::CacheOutcome::Hit {
                tracing::info!(
                    path = %session.display(),
                    elapsed_ms = t.elapsed().as_millis(),
                    "loaded a cached report"
                );
            }

            if summary {
                let losses = load::read_sidecar(&session)
                    .map(|m| m.losses())
                    .unwrap_or_default();
                println!("{}", serde_json::to_string_pretty(&report.summary(losses))?);
            } else if !quiet {
                let t = Instant::now();
                let text = report.render();
                report.push_timing("render", t.elapsed().as_secs_f64() * 1000.0);
                let stdout = std::io::stdout();
                let mut w = BufWriter::new(stdout.lock());
                w.write_all(text.as_bytes())?;
                w.flush()?;
            }
            if let Some(path) = json {
                let t = Instant::now();
                report
                    .write_json(&path)
                    .with_context(|| format!("writing {}", path.display()))?;
                report.push_timing("json", t.elapsed().as_secs_f64() * 1000.0);
                tracing::info!(path = %path.display(), "wrote JSON report");
            }
            if let Some(dir) = csv_dir {
                let t = Instant::now();
                let written = report
                    .write_csvs(&dir)
                    .with_context(|| format!("writing CSVs into {}", dir.display()))?;
                report.push_timing("csv", t.elapsed().as_secs_f64() * 1000.0);
                for p in written {
                    tracing::info!(path = %p.display(), "wrote CSV");
                }
            }
            if timing {
                print!("{}", report.timing_table());
            }
        }

        Cmd::Trend {
            dir,
            json_dir,
            metric,
            csv,
            json,
            params,
        } => {
            let rows = trend::compute(&dir, json_dir.as_deref(), params.apply(), &metric)
                .with_context(|| format!("building a trend over {}", dir.display()))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print!("{}", trend::render(&rows));
            }
            if let Some(path) = csv {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                let mut w = BufWriter::new(
                    std::fs::File::create(&path)
                        .with_context(|| format!("writing {}", path.display()))?,
                );
                trend::write_csv(&mut w, &rows)?;
                w.flush()?;
                tracing::info!(path = %path.display(), rows = rows.len(), "wrote trend CSV");
            }
        }

        Cmd::List { dir, json } => {
            let entries =
                load::scan_dir(&dir).with_context(|| format!("listing {}", dir.display()))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("no recordings in {}", dir.display());
            } else {
                println!(
                    "{:<24}  {:<28}  {:>12}  {:>12}  {:>8}  {:>5}  {:<16}  {:<12}  GAME",
                    "SESSION",
                    "STARTED (UTC)",
                    "DURATION",
                    "EVENTS",
                    "DROPS",
                    "BAD",
                    "LOSS",
                    "EXIT"
                );
                for e in &entries {
                    println!(
                        "{:<24}  {:<28}  {:>12}  {:>12}  {:>8}  {:>5}  {:<16}  {:<12}  {}",
                        e.session_id,
                        format_utc_us(e.started_utc_us),
                        format_duration(e.duration_s),
                        e.events,
                        e.drops,
                        e.bad_lines.flag(),
                        e.losses_text(),
                        e.exit_text(),
                        e.games.first().map_or("—", |g| g.as_str()),
                    );
                }
                let total_events: u64 = entries.iter().map(|e| e.events).sum();
                let total_drops: u64 = entries.iter().map(|e| e.drops).sum();
                let lossy = entries.iter().filter(|e| !e.losses.is_empty()).count();
                let unfinished = entries
                    .iter()
                    .filter(|e| e.exit.as_deref() == Some("running"))
                    .count();
                println!(
                    "\n{} session(s), {} events, {} drops, {} with sink losses, {} unfinished\n\
                     BAD = unparseable JSONL lines (a bare count is a cut-off tail; ! means corruption inside the file); \
                     LOSS = sink=envelopes not delivered and EXIT = how the run ended, both from the .meta.json sidecar \
                     (\"running\" = it never stopped cleanly)",
                    entries.len(),
                    total_events,
                    total_drops,
                    lossy,
                    unfinished
                );
            }
        }
    }

    tracing::info!(
        total_ms = format_args!("{:.1}", started.elapsed().as_secs_f64() * 1000.0),
        "analysis complete"
    );
    Ok(())
}
