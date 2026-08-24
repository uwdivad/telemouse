//! Offline metrics over recorded telemouse sessions — Phase 5's analysis layer.
//!
//! The plan keeps raw HID counts on the wire and derives everything in
//! consumers; this crate is that consumer. It reads a
//! `recordings/<session_id>.jsonl` file, resamples the irregular event stream
//! onto a uniform grid, and computes the plan's metrics catalog:
//!
//! | Module | Metric group |
//! |---|---|
//! | [`kinematics`] | velocity / acceleration / jerk, distance, path efficiency |
//! | [`flicks`] | flick detection: amplitude, peak velocity, overshoot, settle, time-to-click |
//! | [`micro`] | sub-movements, corrections, tremor band power, micro-adjustment sizes |
//! | [`clicks`] | trigger discipline: pre-click stability, holds, double-clicks |
//! | [`lifts`] | repositioning lifts, inferred from drift-then-snap-back |
//! | [`quality`] | interval histogram, ring drops, batch loss, monotonicity |
//! | [`per_second`] | the per-second derived table |
//! | [`per_minute`] | the per-minute table — the fatigue and warmup substrate |
//! | [`markers`] | marker segmentation and per-interval headline metrics |
//! | [`trend`] | one row per session, over a cache of per-session reports |
//!
//! [`report::build`] runs all of them and produces a [`report::Report`], which
//! renders to a terminal summary, JSON, or CSV.
//!
//! ```no_run
//! use telemouse_analyze::{load, report, series::Params};
//! # fn main() -> anyhow::Result<()> {
//! let session = load::load_session(std::path::Path::new("recordings/s-1.jsonl"))?;
//! let report = report::build(session, Params::default());
//! println!("{}", report.render());
//! # Ok(())
//! # }
//! ```
//!
//! Numerics live in [`savgol`] (Savitzky–Golay smoothing/differentiation) and
//! [`stats`]; neither pulls in a dependency.
//!
//! # Scale
//!
//! A three-hour 1 kHz session is ~8 M events and ~10.8 M grid cells, and this
//! crate is written for that: the grid is stored as sparse runs (see
//! [`series`]), the loader sizes and streams rather than growing, `Prepared`
//! owns the event vector instead of copying it, and every session-length pass
//! is linear with no per-item allocation. `benches/hot_math.rs` covers the hot
//! paths at 1 M and 10 M cells.

pub mod clicks;
pub mod flicks;
pub mod kinematics;
pub mod lifts;
pub mod load;
pub mod markers;
pub mod micro;
pub mod per_minute;
pub mod per_second;
pub mod quality;
pub mod report;
pub mod savgol;
pub mod series;
pub mod stats;
pub mod testutil;
pub mod timefmt;
pub mod trend;

pub use load::{LoadedSession, SessionIndexEntry, load_session, scan_dir, scan_session};
pub use report::{Report, build};
pub use series::{Params, Prepared, prepare};
