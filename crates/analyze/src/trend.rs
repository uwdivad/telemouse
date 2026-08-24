//! Longitudinal analysis — one row per session.
//!
//! The plan's session-level metrics (fatigue, warmup, consistency, sensitivity
//! experiments) are all "the same number, across sessions". That needs the
//! per-session reports to be *cheap to get at*, which is what the JSON cache
//! here is for: a report is recomputed only when the analyzer version has moved
//! on or the recording is newer than its cached report.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::report::{ANALYZER_VERSION, Report};
use crate::series::Params;
use crate::timefmt::{format_duration, format_utc_us};

/// Why a session's report was (or was not) recomputed. Logged so a surprising
/// `trend` runtime is explainable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// The cached report was current and used as-is.
    Hit,
    /// No cached report existed.
    Missing,
    /// The cache was written by a different analyzer version.
    VersionChanged,
    /// The cache was computed with different detector parameters.
    ParamsChanged,
    /// The recording has been modified since the cache was written.
    Stale,
    /// The cache file existed but could not be read or parsed.
    Unreadable,
}

impl CacheOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheOutcome::Hit => "hit",
            CacheOutcome::Missing => "missing",
            CacheOutcome::VersionChanged => "version-changed",
            CacheOutcome::ParamsChanged => "params-changed",
            CacheOutcome::Stale => "recording-newer",
            CacheOutcome::Unreadable => "unreadable",
        }
    }
}

/// Where a session's cached report lives inside `dir`.
pub fn cache_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.report.json"))
}

/// Read a cached report if it is still valid for `recording` and `params`.
///
/// Valid means: it parses, its `analyzer_version` matches this build, it was
/// computed with the same detector parameters, and the recording has not been
/// modified since the cache was written. Anything else recomputes — a stale
/// cache silently answering with the wrong thresholds is exactly the failure
/// this is meant to prevent.
pub fn read_cache(
    cache: &Path,
    recording: &Path,
    params: Params,
) -> (Option<Report>, CacheOutcome) {
    let cache_meta = match std::fs::metadata(cache) {
        Ok(m) => m,
        Err(_) => return (None, CacheOutcome::Missing),
    };
    if let (Ok(rec), Ok(cached)) = (
        std::fs::metadata(recording).and_then(|m| m.modified()),
        cache_meta.modified(),
    ) && rec > cached
    {
        return (None, CacheOutcome::Stale);
    }
    let text = match std::fs::read_to_string(cache) {
        Ok(t) => t,
        Err(_) => return (None, CacheOutcome::Unreadable),
    };
    match serde_json::from_str::<Report>(&text) {
        Ok(r) if r.analyzer_version != ANALYZER_VERSION => (None, CacheOutcome::VersionChanged),
        Ok(r) if r.params != params => (None, CacheOutcome::ParamsChanged),
        Ok(r) => (Some(r), CacheOutcome::Hit),
        Err(_) => (None, CacheOutcome::Unreadable),
    }
}

/// Load `recording`, reusing `json_dir`'s cached report when it is current.
/// Writes the cache back on a miss when `json_dir` is set.
pub fn report_for(
    recording: &Path,
    json_dir: Option<&Path>,
    params: Params,
) -> anyhow::Result<(Report, CacheOutcome)> {
    if let Some(dir) = json_dir {
        // The session id is the file stem for recordings written by the agent;
        // fall back to loading if that guess misses.
        if let Some(stem) = recording.file_stem().and_then(|s| s.to_str()) {
            let path = cache_path(dir, stem);
            let (hit, why) = read_cache(&path, recording, params);
            if let Some(r) = hit {
                tracing::info!(
                    session = %stem,
                    cache = %path.display(),
                    check = why.as_str(),
                    "reusing cached report"
                );
                return Ok((r, why));
            }
            tracing::info!(
                session = %stem,
                cache = %path.display(),
                check = why.as_str(),
                "recomputing report"
            );
            let loaded = crate::load::load_session(recording)?;
            let report = crate::report::build(loaded, params);
            report.write_json(&path)?;
            return Ok((report, why));
        }
    }
    let loaded = crate::load::load_session(recording)?;
    Ok((
        crate::report::build(loaded, params),
        CacheOutcome::Missing,
    ))
}

/// One session's line in the longitudinal table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendRow {
    pub session_id: String,
    pub path: String,
    pub started_utc_us: i64,
    pub started_utc: String,
    pub duration_s: f64,
    pub events: usize,
    pub drops: u64,
    pub game: Option<String>,
    pub flicks: usize,
    pub flicks_per_min: f64,
    pub overshoot_median: f64,
    pub settle_median_ms: f64,
    pub tremor_rms_counts_s: f64,
    pub path_efficiency: f64,
    pub clicks_per_min: f64,
    pub distance_m: f64,
    pub lifts: usize,
    /// Whether this row came out of the cache.
    pub from_cache: bool,
    /// Extra metrics requested with `--metric`, in the order they were asked
    /// for. Dotted paths into the report JSON.
    #[serde(default)]
    pub extra: Vec<(String, Option<f64>)>,
}

/// Resolve a dotted path such as `micro.band_ratio_8_12` in a report.
pub fn metric_at(report: &Report, path: &str) -> Option<f64> {
    let v = serde_json::to_value(report).ok()?;
    let mut cur = &v;
    for part in path.split('.') {
        cur = match part.parse::<usize>() {
            Ok(i) => cur.get(i)?,
            Err(_) => cur.get(part)?,
        };
    }
    cur.as_f64()
}

/// Summarize one report as a trend row.
pub fn row(report: &Report, from_cache: bool, metrics: &[String]) -> TrendRow {
    TrendRow {
        session_id: report.session.session_id.clone(),
        path: report.session.path.clone(),
        started_utc_us: report.session.started_utc_us,
        started_utc: report.session.started_utc.clone(),
        duration_s: report.session.duration_s,
        events: report.session.event_count,
        drops: report.quality.ring_drops,
        game: report.session.game.clone(),
        flicks: report.flicks.count,
        flicks_per_min: report.flicks.per_minute,
        overshoot_median: report.flicks.overshoot_ratio.median,
        settle_median_ms: report.flicks.settle_ms.median,
        tremor_rms_counts_s: report.micro.tremor_rms_counts_s,
        path_efficiency: report.kinematics.path_efficiency_weighted,
        clicks_per_min: report.clicks.clicks_per_min,
        distance_m: report.kinematics.total_distance_m,
        lifts: report.lifts.count,
        from_cache,
        extra: metrics
            .iter()
            .map(|m| (m.clone(), metric_at(report, m)))
            .collect(),
    }
}

/// Build the trend table over every `*.jsonl` in `dir`, oldest session first.
pub fn compute(
    dir: &Path,
    json_dir: Option<&Path>,
    params: Params,
    metrics: &[String],
) -> anyhow::Result<Vec<TrendRow>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();

    let mut rows = Vec::with_capacity(paths.len());
    for p in &paths {
        match report_for(p, json_dir, params) {
            Ok((report, why)) => rows.push(row(&report, why == CacheOutcome::Hit, metrics)),
            Err(e) => tracing::warn!(path = %p.display(), error = %e, "skipping session"),
        }
    }
    rows.sort_by_key(|r| (r.started_utc_us, r.session_id.clone()));
    Ok(rows)
}

pub fn write_csv<W: Write>(w: &mut W, rows: &[TrendRow]) -> io::Result<()> {
    let extra: Vec<&str> = rows
        .first()
        .map(|r| r.extra.iter().map(|(k, _)| k.as_str()).collect())
        .unwrap_or_default();
    write!(
        w,
        "session_id,started_utc,started_utc_us,duration_s,events,drops,game,flicks,\
flicks_per_min,overshoot_median,settle_median_ms,tremor_rms_counts_s,path_efficiency,\
clicks_per_min,distance_m,lifts"
    )?;
    for k in &extra {
        write!(w, ",{}", crate::per_second::csv_field(k))?;
    }
    writeln!(w)?;

    for r in rows {
        write!(
            w,
            "{},{},{},{:.3},{},{},{},{},{:.3},{:.5},{:.2},{:.3},{:.5},{:.3},{:.4},{}",
            crate::per_second::csv_field(&r.session_id),
            crate::per_second::csv_field(&r.started_utc),
            r.started_utc_us,
            r.duration_s,
            r.events,
            r.drops,
            crate::per_second::csv_field(r.game.as_deref().unwrap_or("")),
            r.flicks,
            r.flicks_per_min,
            r.overshoot_median,
            r.settle_median_ms,
            r.tremor_rms_counts_s,
            r.path_efficiency,
            r.clicks_per_min,
            r.distance_m,
            r.lifts,
        )?;
        for (_, v) in &r.extra {
            match v {
                Some(x) => write!(w, ",{x}")?,
                None => write!(w, ",")?,
            }
        }
        writeln!(w)?;
    }
    Ok(())
}

pub fn to_csv(rows: &[TrendRow]) -> String {
    let mut buf = Vec::new();
    write_csv(&mut buf, rows).expect("writing to a Vec cannot fail");
    String::from_utf8(buf).expect("UTF-8 output")
}

/// The terminal table.
pub fn render(rows: &[TrendRow]) -> String {
    let mut o = String::new();
    if rows.is_empty() {
        return "no sessions found\n".to_string();
    }
    let extra: Vec<&str> = rows[0].extra.iter().map(|(k, _)| k.as_str()).collect();
    o.push_str(&format!(
        "{:<22} {:<20} {:>10} {:>10} {:>7} {:>7} {:>9} {:>9} {:>8} {:>8}",
        "SESSION", "STARTED (UTC)", "DURATION", "EVENTS", "DROPS", "FLICKS", "OVERSHOOT",
        "SETTLE ms", "TREMOR", "PATH EFF"
    ));
    for k in &extra {
        o.push_str(&format!(" {k:>14}"));
    }
    o.push('\n');
    for r in rows {
        o.push_str(&format!(
            "{:<22} {:<20} {:>10} {:>10} {:>7} {:>7} {:>9.4} {:>9.1} {:>8.0} {:>8.3}",
            truncate(&r.session_id, 22),
            &r.started_utc[..19.min(r.started_utc.len())],
            format_duration(r.duration_s),
            r.events,
            r.drops,
            r.flicks,
            r.overshoot_median,
            r.settle_median_ms,
            r.tremor_rms_counts_s,
            r.path_efficiency,
        ));
        for (_, v) in &r.extra {
            match v {
                Some(x) => o.push_str(&format!(" {x:>14.4}")),
                None => o.push_str(&format!(" {:>14}", "—")),
            }
        }
        o.push('\n');
    }
    let cached = rows.iter().filter(|r| r.from_cache).count();
    o.push_str(&format!(
        "\n{} session(s), {} from cache, first {}\n",
        rows.len(),
        cached,
        format_utc_us(rows[0].started_utc_us)
    ));
    o
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).chain(std::iter::once('…')).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;
    use telemouse_core::event::buttons;

    fn flicky_events(n: usize) -> Vec<telemouse_core::RawEvent> {
        let mut b = testutil::StreamBuilder::new();
        b.idle_ms(200);
        for _ in 0..n {
            b.move_ms(25, 60, 0)
                .idle_ms(10)
                .move_ms(10, -10, 0)
                .idle_ms(5)
                .button(buttons::LEFT_DOWN)
                .idle_ms(39)
                .button(buttons::LEFT_UP)
                .idle_ms(500);
        }
        b.into_events()
    }

    /// Two synthetic sessions in a temp dir, the second starting a day later
    /// and containing twice the flicks.
    fn two_sessions(dir: &Path) {
        let mut a = testutil::session_cfg();
        a.session_id = "s-day1".into();
        testutil::write_session(dir, &a, Some("cs2.exe"), &flicky_events(3), &[], 256);

        let mut b = testutil::session_cfg();
        b.session_id = "s-day2".into();
        b.started_utc_us += 86_400_000_000;
        b.anchor.utc_us += 86_400_000_000;
        testutil::write_session(dir, &b, Some("cs2.exe"), &flicky_events(6), &[], 256);
    }

    #[test]
    fn a_trend_over_two_sessions_has_one_row_each_in_date_order() {
        let dir = tempfile::tempdir().unwrap();
        two_sessions(dir.path());

        let rows = compute(dir.path(), None, Params::default(), &[]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, "s-day1");
        assert_eq!(rows[1].session_id, "s-day2");
        assert!(rows[1].started_utc_us > rows[0].started_utc_us);
        assert_eq!(rows[0].flicks, 3);
        assert_eq!(rows[1].flicks, 6);
        assert!(rows[0].overshoot_median > 0.0);
        assert!(rows[0].path_efficiency > 0.0);
        assert!(rows.iter().all(|r| !r.from_cache));
        assert_eq!(rows[0].game.as_deref(), Some("cs2.exe"));

        let csv = to_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("session_id,started_utc,"));
        assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());

        let text = render(&rows);
        assert!(text.contains("s-day1"), "{text}");
        assert!(text.contains("OVERSHOOT"), "{text}");
    }

    #[test]
    fn the_second_run_comes_out_of_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        two_sessions(dir.path());

        let first = compute(dir.path(), Some(&cache), Params::default(), &[]).unwrap();
        assert!(first.iter().all(|r| !r.from_cache));
        assert!(cache.join("s-day1.report.json").exists());
        assert!(cache.join("s-day2.report.json").exists());

        let second = compute(dir.path(), Some(&cache), Params::default(), &[]).unwrap();
        assert!(second.iter().all(|r| r.from_cache), "{second:#?}");
        // Same numbers either way.
        assert_eq!(first[0].flicks, second[0].flicks);
        assert!((first[1].overshoot_median - second[1].overshoot_median).abs() < 1e-9);
    }

    #[test]
    fn a_cache_from_another_analyzer_version_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        two_sessions(dir.path());
        compute(dir.path(), Some(&cache_dir), Params::default(), &[]).unwrap();

        let path = cache_path(&cache_dir, "s-day1");
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["analyzer_version"] = serde_json::Value::String("0.0.0-ancient".into());
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();

        let recording = dir.path().join("s-day1.jsonl");
        let (hit, why) = read_cache(&path, &recording, Params::default());
        assert!(hit.is_none());
        assert_eq!(why, CacheOutcome::VersionChanged);

        // ...and the next run rewrites it with the current version.
        let rows = compute(dir.path(), Some(&cache_dir), Params::default(), &[]).unwrap();
        assert!(!rows[0].from_cache);
        assert!(rows[1].from_cache);
        let (hit, why) = read_cache(&path, &recording, Params::default());
        assert_eq!(why, CacheOutcome::Hit);
        assert_eq!(hit.unwrap().analyzer_version, ANALYZER_VERSION);
    }

    /// A cache computed with different thresholds answers a different
    /// question, so it must never be handed back for this one.
    #[test]
    fn a_cache_computed_with_other_parameters_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        two_sessions(dir.path());
        compute(dir.path(), Some(&cache_dir), Params::default(), &[]).unwrap();

        // The fixture's pulls peak at 60 000 counts/s, so this threshold puts
        // the flick count at zero — a visibly different answer.
        let other = Params {
            flick_speed: 500_000.0,
            ..Default::default()
        };
        let path = cache_path(&cache_dir, "s-day1");
        let recording = dir.path().join("s-day1.jsonl");
        let (hit, why) = read_cache(&path, &recording, other);
        assert!(hit.is_none());
        assert_eq!(why, CacheOutcome::ParamsChanged);

        // Recomputing under the new thresholds finds no flicks at all, which
        // is exactly the answer a stale cache would have hidden.
        let rows = compute(dir.path(), Some(&cache_dir), other, &[]).unwrap();
        assert!(rows.iter().all(|r| !r.from_cache));
        assert_eq!(rows[0].flicks, 0);
    }

    /// The params comparison above is only sound if `Params` survives the JSON
    /// round-trip bit-for-bit; every field is pinned here so a future float
    /// value that does not cannot slip in unnoticed.
    #[test]
    fn params_round_trip_through_json_exactly() {
        let p = Params::default();
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Params>(&text).unwrap(), p);
        let odd = Params {
            grid_dt_s: 1.0 / 3.0,
            still_speed: 0.1 + 0.2,
            lift_opposite_cos: -0.7,
            ..p
        };
        let text = serde_json::to_string(&odd).unwrap();
        assert_eq!(serde_json::from_str::<Params>(&text).unwrap(), odd);
    }

    #[test]
    fn a_missing_cache_file_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (hit, why) = read_cache(
            &dir.path().join("nope.json"),
            &dir.path().join("x.jsonl"),
            Params::default(),
        );
        assert!(hit.is_none());
        assert_eq!(why, CacheOutcome::Missing);
    }

    #[test]
    fn extra_metrics_are_resolved_by_dotted_path() {
        let dir = tempfile::tempdir().unwrap();
        two_sessions(dir.path());
        let metrics = vec![
            "micro.band_ratio_8_12".to_string(),
            "kinematics.moving_fraction".to_string(),
            "nope.not_here".to_string(),
        ];
        let rows = compute(dir.path(), None, Params::default(), &metrics).unwrap();
        assert_eq!(rows[0].extra.len(), 3);
        assert_eq!(rows[0].extra[0].0, "micro.band_ratio_8_12");
        assert!(rows[0].extra[0].1.is_some());
        assert!(rows[0].extra[1].1.unwrap() > 0.0);
        assert!(rows[0].extra[2].1.is_none());
        let csv = to_csv(&rows);
        assert!(csv.lines().next().unwrap().contains("micro.band_ratio_8_12"));
    }

    #[test]
    fn an_empty_directory_yields_an_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let rows = compute(dir.path(), None, Params::default(), &[]).unwrap();
        assert!(rows.is_empty());
        assert_eq!(render(&rows), "no sessions found\n");
    }
}
