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

mod explorer;

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

// Help headings. Flags keep their names and defaults; only where `--help`
// lists them changes.
const H_INPUT: &str = "Input";
const H_OUTPUT: &str = "Output";
const H_FILTER: &str = "Filtering";
const H_DETECT: &str = "Advanced (flick and click detection)";
const H_LIFT: &str = "Advanced (lift / repositioning detection)";
const H_GRID: &str = "Advanced (smoothing and grid)";

const TOP_EXAMPLES: &str = "\
Examples:
  telemouse-analyze list
  telemouse-analyze report demo-session
  telemouse-analyze trend --csv trend.csv

Run `telemouse-analyze <COMMAND> --help` for the options of one command.
The Report button in telemouse-ctl runs `report` for you.";

const REPORT_EXAMPLES: &str = "\
Examples:
  telemouse-analyze report demo-session
  telemouse-analyze report recordings\\demo-session.jsonl --split-by-marker
  telemouse-analyze report demo-session --summary
  telemouse-analyze report demo-session --quiet --json report.json --csv-dir tables

The Advanced options tune the detectors; left alone they keep the built-in
defaults, which is what the numbers in the docs assume.";

const TREND_EXAMPLES: &str = "\
Examples:
  telemouse-analyze trend
  telemouse-analyze trend --json-dir reports --csv trend.csv
  telemouse-analyze trend --metric micro.band_ratio_8_12 --json

--json-dir makes the second run fast: sessions already analyzed are read back
instead of recomputed.";

const LIST_EXAMPLES: &str = "\
Examples:
  telemouse-analyze list
  telemouse-analyze list --dir D:\\telemouse\\recordings --json";

#[derive(Parser, Debug)]
#[command(
    name = "telemouse-analyze",
    about = "Turn recorded telemouse sessions into aim metrics: flicks, overshoot, settle time, tremor, clicks",
    after_help = TOP_EXAMPLES,
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
    /// Only count in-game aiming (pointer locked) for the metrics given in degrees.
    #[arg(long, help_heading = H_FILTER)]
    locked_only: bool,
    /// Also break the report down by the stretches between markers.
    #[arg(long, help_heading = H_OUTPUT)]
    split_by_marker: bool,
    /// Speed a movement must reach to count as a flick, in counts/s.
    #[arg(long, value_name = "COUNTS_PER_S", help_heading = H_DETECT)]
    flick_threshold: Option<f64>,
    /// Speed below which the hand counts as still, in counts/s.
    #[arg(long, value_name = "COUNTS_PER_S", help_heading = H_DETECT)]
    still_threshold: Option<f64>,
    /// How long the hand must stay still for a flick to count as settled, in ms.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    still_hold_ms: Option<u64>,
    /// Longest time from the start of a flick to the click that ends it, in ms.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    click_window_ms: Option<f64>,
    /// Start of the steadiness window before a click, in ms before the press.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    pre_click_lo_ms: Option<f64>,
    /// End of the steadiness window before a click, in ms before the press.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    pre_click_hi_ms: Option<f64>,
    /// Longest gap between two presses that still counts as a double click, in ms.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    double_click_max_ms: Option<f64>,
    /// Ignore movements shorter than this, in ms.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    min_segment_ms: Option<usize>,
    /// How long a change of direction must last to count as a correction, in ms.
    #[arg(long, value_name = "MS", help_heading = H_DETECT)]
    min_reversal_ms: Option<usize>,
    /// Shortest slow drift that can start a lift, in ms.
    #[arg(long, value_name = "MS", help_heading = H_LIFT)]
    lift_drift_min_ms: Option<usize>,
    /// Fastest a drift may be and still read as running out of pad, in counts/s.
    #[arg(long, value_name = "COUNTS_PER_S", help_heading = H_LIFT)]
    lift_drift_max_speed: Option<f64>,
    /// Shortest drift distance that can start a lift, in counts.
    #[arg(long, value_name = "COUNTS", help_heading = H_LIFT)]
    lift_drift_min_counts: Option<f64>,
    /// Slowest return sweep that still ends a lift, in counts/s.
    #[arg(long, value_name = "COUNTS_PER_S", help_heading = H_LIFT)]
    lift_return_min_speed: Option<f64>,
    /// Longest pause between the drift and the return sweep, in ms.
    #[arg(long, value_name = "MS", help_heading = H_LIFT)]
    lift_max_gap_ms: Option<usize>,
    /// How opposite the return sweep must be, as a cosine (-1 = exactly reversed).
    #[arg(
        long,
        value_name = "COS",
        allow_negative_numbers = true,
        help_heading = H_LIFT
    )]
    lift_opposite_cos: Option<f64>,
    /// Time step the movement is resampled to, in seconds.
    #[arg(long, value_name = "SECONDS", help_heading = H_GRID)]
    grid_dt: Option<f64>,
    /// Smoothing (Savitzky–Golay) half-window, in time steps.
    #[arg(long, value_name = "CELLS", help_heading = H_GRID)]
    sg_half: Option<usize>,
    /// Smoothing (Savitzky–Golay) polynomial order.
    #[arg(long, value_name = "ORDER", help_heading = H_GRID)]
    sg_order: Option<usize>,
    /// Averaging width used to separate tremor from intended movement, in ms.
    #[arg(long, value_name = "MS", help_heading = H_GRID)]
    tremor_baseline_ms: Option<usize>,
    /// Safety cap on how much of a very long session is analyzed, in time steps.
    #[arg(long, value_name = "CELLS", help_heading = H_GRID)]
    max_grid_cells: Option<usize>,
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
    /// Analyze one recording and print its aim metrics.
    #[command(after_help = REPORT_EXAMPLES)]
    Report {
        /// The recording: a path to a `.jsonl` file, or just the session id
        /// that `list` and the control panel show.
        session: String,
        /// Folder a session id is looked up in.
        #[arg(
            long,
            default_value = "recordings",
            value_name = "DIR",
            help_heading = H_INPUT
        )]
        dir: PathBuf,
        /// Print only the headline numbers, as a few KB of JSON, instead of
        /// the text report.
        #[arg(long, help_heading = H_OUTPUT)]
        summary: bool,
        /// Also save the full report as JSON to this file.
        #[arg(long, value_name = "FILE", help_heading = H_OUTPUT)]
        json: Option<PathBuf>,
        /// Keep `<session>.report.json` files in this folder and reuse them
        /// while the recording and the analyzer are unchanged.
        #[arg(long, value_name = "DIR", help_heading = H_OUTPUT)]
        json_dir: Option<PathBuf>,
        /// Also save the report's tables as CSV files in this folder.
        #[arg(long, value_name = "DIR", help_heading = H_OUTPUT)]
        csv_dir: Option<PathBuf>,
        /// Show how long each analysis step took.
        #[arg(long, help_heading = H_OUTPUT)]
        timing: bool,
        /// Do not print the text report (useful with --json or --csv-dir).
        #[arg(long, help_heading = H_OUTPUT)]
        quiet: bool,
        #[command(flatten)]
        params: ParamFlags,
    },
    /// Compare sessions over time: one row per recording in a folder.
    #[command(after_help = TREND_EXAMPLES)]
    Trend {
        /// Folder of recordings to compare.
        #[arg(
            long,
            default_value = "recordings",
            value_name = "DIR",
            help_heading = H_INPUT
        )]
        dir: PathBuf,
        /// Keep per-session `<session>.report.json` files here and reuse
        /// them, so only new recordings are analyzed.
        #[arg(long, value_name = "DIR", help_heading = H_OUTPUT)]
        json_dir: Option<PathBuf>,
        /// Add a column for another metric, named by its dotted path in the
        /// JSON report, e.g. `micro.band_ratio_8_12`. Repeatable.
        #[arg(long, value_name = "PATH", help_heading = H_OUTPUT)]
        metric: Vec<String>,
        /// Also save the table as CSV to this file.
        #[arg(long, value_name = "FILE", help_heading = H_OUTPUT)]
        csv: Option<PathBuf>,
        /// Print the table as JSON instead of text.
        #[arg(long, help_heading = H_OUTPUT)]
        json: bool,
        #[command(flatten)]
        params: ParamFlags,
    },
    /// List the recordings in a folder, with their size and health.
    #[command(after_help = LIST_EXAMPLES)]
    List {
        /// Folder of recordings to list.
        #[arg(
            long,
            default_value = "recordings",
            value_name = "DIR",
            help_heading = H_INPUT
        )]
        dir: PathBuf,
        /// Print the listing as JSON instead of text.
        #[arg(long, help_heading = H_OUTPUT)]
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
    // Before anything else: a double-click from Explorer gets an explanation
    // instead of a window that flashes and vanishes. Exit 2 is what clap's
    // own "no subcommand" usage error returns.
    if explorer::hold_window_if_double_clicked() {
        std::process::exit(2);
    }

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
