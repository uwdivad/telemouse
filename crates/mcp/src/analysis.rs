//! The analyzer, called as a library.
//!
//! `sessions_list`, `session_summary` and `trend` are the same code paths
//! `telemouse-analyze list`, `report --summary` and `trend --json` run —
//! `telemouse_analyze::load::scan_dir`, `trend::report_for` +
//! [`Report::summary`](telemouse_analyze::report::Report::summary) and
//! `trend::compute` — so there is one implementation of every metric and no
//! second process to find, version-match or parse the output of.
//!
//! Everything here blocks (a report on a long session is seconds of CPU and
//! hundreds of megabytes of file), so the tools call it through
//! [`tokio::task::spawn_blocking`].
//!
//! A server started `--read-only` still *reads* the panel's report cache,
//! but never creates it or writes into it: a report missing from the cache
//! is computed and returned without being stored.

use std::path::{Path, PathBuf};

use telemouse_analyze::load::{self, SessionIndexEntry};
use telemouse_analyze::report::{Report, ReportSummary};
use telemouse_analyze::series::Params;
use telemouse_analyze::trend::{self, CacheOutcome, TrendRow};
use telemouse_core::recordings::{self, SessionMeta};

use crate::args;

/// Where the recordings and the report cache are.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// `[recording] dir`, already resolved against the config's directory.
    pub dir: PathBuf,
    /// The cached `<id>.report.json` files. The same folder the control
    /// panel uses, so a report run from the panel and one run from here are
    /// the same work done once.
    pub cache: PathBuf,
    /// Read the cache, never write it (`--read-only`).
    pub read_only: bool,
}

/// The folder name the panel uses under the recordings directory.
pub const CACHE_DIR: &str = ".reports";

impl Analysis {
    /// Point at `dir` and the report cache beside it.
    pub fn new(dir: PathBuf, read_only: bool) -> Self {
        let cache = dir.join(CACHE_DIR);
        Self {
            dir,
            cache,
            read_only,
        }
    }

    /// The cache directory to hand the analyzer, which reads it *and* writes
    /// it, created if it can be. A cache that cannot be written only costs
    /// time on the next call, so a failure here is a `None` and a debug
    /// line, never an error the user sees. Always `None` read-only: that
    /// mode goes through [`Self::report_read_only`] instead.
    fn writable_cache(&self) -> Option<&Path> {
        if self.read_only {
            return None;
        }
        match std::fs::create_dir_all(&self.cache) {
            Ok(()) => Some(self.cache.as_path()),
            Err(e) => {
                tracing::debug!(dir = %self.cache.display(), error = %e, "no report cache; every report will be recomputed");
                None
            }
        }
    }

    /// `<id>.jsonl` under the recordings directory, after the id rule.
    ///
    /// Two separate failures with two separate messages: an id that could
    /// never name a recording, and one that simply is not there.
    pub fn recording(&self, id: &str) -> Result<PathBuf, String> {
        let id = args::recording_id(id)?;
        let path = self
            .dir
            .join(format!("{id}{}", recordings::RECORDING_SUFFIX));
        if !path.is_file() {
            return Err(format!(
                "there is no recording {id:?} in {}. Call sessions_list to see what is there.",
                self.dir.display()
            ));
        }
        Ok(path)
    }

    /// Every recording, newest first — a header-only scan that never loads
    /// events, with the sidecar's per-sink losses and exit reason on each
    /// row. Returns how many there are as well as the `limit` newest, so a
    /// truncated answer says that it is one.
    pub fn sessions(&self, limit: usize) -> Result<(usize, Vec<SessionIndexEntry>), String> {
        let mut entries = load::scan_dir(&self.dir).map_err(|e| {
            format!(
                "cannot list recordings in {}: {}",
                self.dir.display(),
                load::error_chain(&e)
            )
        })?;
        entries.sort_by_key(|e| std::cmp::Reverse(e.started_utc_us));
        let total = entries.len();
        entries.truncate(limit);
        Ok((total, entries))
    }

    /// The report for one recording file, from the cache when it is current.
    /// Returns whether it came from the cache.
    fn report(&self, path: &Path) -> Result<(Report, bool), String> {
        let out = if self.read_only {
            self.report_read_only(path)
        } else {
            trend::report_for(path, self.writable_cache(), Params::default())
                .map(|(r, why)| (r, why == CacheOutcome::Hit))
                .map_err(|e| load::error_chain(&e))
        };
        out.map_err(|e| format!("analyzing {}: {e}", path.display()))
    }

    /// [`trend::report_for`] without its write-back: a current cached report
    /// if there is one, otherwise a fresh one that is not stored.
    fn report_read_only(&self, path: &Path) -> Result<(Report, bool), String> {
        let params = Params::default();
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let cached = trend::cache_path(&self.cache, stem);
            let (hit, why) = trend::read_cache(&cached, path, params);
            if let Some(report) = hit {
                return Ok((report, true));
            }
            tracing::debug!(session = %stem, check = why.as_str(), "read-only: computing a report that will not be cached");
        }
        trend::report_for(path, None, params)
            .map(|(r, _)| (r, false))
            .map_err(|e| load::error_chain(&e))
    }

    /// The `telemouse-report-summary/2` headline for one recording.
    pub fn summary(&self, id: &str) -> Result<ReportSummary, String> {
        let path = self.recording(id)?;
        let (report, _from_cache) = self.report(&path)?;
        Ok(report.summary(sidecar_losses(&path)))
    }

    /// One row per session, oldest first, optionally cut to the last `last`.
    pub fn trend(&self, metrics: &[String], last: Option<usize>) -> Result<Vec<TrendRow>, String> {
        let mut rows = if self.read_only {
            self.trend_read_only(metrics)?
        } else {
            trend::compute(&self.dir, self.writable_cache(), Params::default(), metrics).map_err(
                |e| {
                    format!(
                        "building a trend over {}: {}",
                        self.dir.display(),
                        load::error_chain(&e)
                    )
                },
            )?
        };
        if let Some(n) = last
            && rows.len() > n
        {
            rows.drain(..rows.len() - n);
        }
        Ok(rows)
    }

    /// [`trend::compute`] without the cache write-back, one session at a
    /// time. Sessions that cannot be analyzed are skipped with a warning, as
    /// `compute` skips them, and the rows are sorted the same way.
    fn trend_read_only(&self, metrics: &[String]) -> Result<Vec<TrendRow>, String> {
        let entries = std::fs::read_dir(&self.dir).map_err(|e| {
            format!(
                "building a trend over {}: cannot list it: {e}",
                self.dir.display()
            )
        })?;
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(recordings::RECORDING_EXT))
            .collect();
        paths.sort();
        let mut rows = Vec::with_capacity(paths.len());
        for path in &paths {
            match self.report(path) {
                Ok((report, from_cache)) => rows.push(trend::row(&report, from_cache, metrics)),
                Err(error) => tracing::warn!(path = %path.display(), %error, "skipping session"),
            }
        }
        rows.sort_by_key(|r| (r.started_utc_us, r.session_id.clone()));
        Ok(rows)
    }
}

/// Per-sink losses from the `<id>.meta.json` sidecar beside `recording`.
///
/// No sidecar is normal (an older agent, or a run that is still going) and
/// says nothing. A sidecar that is there but cannot be read is worth a
/// warning: the summary then shows no losses where there may have been some.
fn sidecar_losses(recording: &Path) -> Vec<(String, u64)> {
    let path = recording.with_extension(recordings::META_SUFFIX.trim_start_matches('.'));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(sidecar = %path.display(), error = %e, "cannot read the sidecar; the summary shows no per-sink losses");
            return Vec::new();
        }
    };
    match SessionMeta::from_json(&text) {
        Ok(meta) => meta.losses(),
        Err(e) => {
            tracing::warn!(sidecar = %path.display(), error = %e, "the sidecar does not parse; the summary shows no per-sink losses");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("telemouse-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_unsafe_id_is_refused_before_the_filesystem_is_touched() {
        let a = Analysis::new(PathBuf::from("recordings"), false);
        for bad in [
            "../secrets",
            "a/b",
            "c:stream",
            ".hidden",
            "",
            "has space",
            &"x".repeat(recordings::MAX_ID_LEN + 1),
        ] {
            let e = a.recording(bad).expect_err("{bad:?} must be refused");
            assert!(
                e.contains("is not a recording id"),
                "{bad:?} gave the wrong error: {e}"
            );
        }
    }

    #[test]
    fn a_safe_but_missing_id_says_where_it_looked() {
        let dir = tmp("missing");
        let a = Analysis::new(dir.clone(), false);
        let e = a.recording("no-such-session").expect_err("not there");
        assert!(e.contains("no recording"), "{e}");
        assert!(e.contains(&dir.display().to_string()), "{e}");
        assert!(e.contains("sessions_list"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_safe_and_present_id_resolves_under_the_recordings_dir_with_or_without_jsonl() {
        let dir = tmp("present");
        std::fs::write(dir.join("s-1.jsonl"), "").unwrap();
        let a = Analysis::new(dir.clone(), false);
        assert_eq!(a.recording("s-1").unwrap(), dir.join("s-1.jsonl"));
        assert_eq!(a.recording("s-1.jsonl").unwrap(), dir.join("s-1.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_cache_sits_beside_the_recordings_where_the_panel_puts_it() {
        let a = Analysis::new(PathBuf::from("recordings"), false);
        assert_eq!(a.cache, PathBuf::from("recordings").join(".reports"));
    }

    #[test]
    fn listing_an_empty_directory_is_an_empty_list_not_an_error() {
        let dir = tmp("empty");
        let a = Analysis::new(dir.clone(), false);
        assert_eq!(a.sessions(50).unwrap(), (0, Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_a_directory_that_is_not_there_says_which_one() {
        let a = Analysis::new(PathBuf::from("no-such-recordings-dir-here"), false);
        let e = a.sessions(50).expect_err("no such directory");
        assert!(e.contains("no-such-recordings-dir-here"), "{e}");
    }

    /// `list` sorts newest first and `limit` keeps the newest, which is what
    /// "my last few sessions" means.
    #[test]
    fn sessions_are_newest_first_and_the_limit_keeps_the_newest() {
        use telemouse_analyze::testutil;
        use telemouse_core::Envelope;

        let dir = tmp("order");
        for (id, offset) in [("old", 0i64), ("mid", 60_000_000), ("new", 120_000_000)] {
            let cfg = testutil::session_cfg_at(id, testutil::FIXTURE_UTC0 + offset);
            let line = Envelope::Session(cfg).to_json().unwrap();
            std::fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
        }
        let a = Analysis::new(dir.clone(), false);
        let ids = |limit: usize| -> (usize, Vec<String>) {
            let (total, rows) = a.sessions(limit).unwrap();
            (total, rows.into_iter().map(|e| e.session_id).collect())
        };
        assert_eq!(ids(50), (3, vec!["new".into(), "mid".into(), "old".into()]));
        assert_eq!(
            ids(2),
            (3, vec!["new".into(), "mid".into()]),
            "a truncated answer still says how many there are"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A recording the analyzer can build a report from.
    fn write_recording(dir: &Path, id: &str) {
        use telemouse_analyze::testutil::{self, StreamBuilder};
        let mut cfg = testutil::session_cfg();
        cfg.session_id = id.into();
        let mut s = StreamBuilder::new();
        s.move_ms(200, 40, 0).idle_ms(100).move_ms(200, -40, 0);
        testutil::write_session(dir, &cfg, None, &s.into_events(), &[], 32);
    }

    /// `--read-only` reads the panel's cache but never creates or fills it.
    #[test]
    fn read_only_never_writes_the_report_cache_but_still_reads_it() {
        let dir = tmp("read-only-cache");
        write_recording(&dir, "s-ro");
        let ro = Analysis::new(dir.clone(), true);

        ro.summary("s-ro").expect("a summary without a cache");
        assert_eq!(ro.trend(&[], None).expect("a trend").len(), 1);
        assert!(
            !ro.cache.exists(),
            "read-only created {}",
            ro.cache.display()
        );

        // The writable server fills the cache; read-only then answers from
        // it without touching it.
        let rw = Analysis::new(dir.clone(), false);
        rw.summary("s-ro").expect("a summary that fills the cache");
        let cached = trend::cache_path(&rw.cache, "s-ro");
        let before = std::fs::metadata(&cached).unwrap().modified().unwrap();
        let rows = ro.trend(&[], None).unwrap();
        assert!(rows[0].from_cache, "read-only must still use the cache");
        assert_eq!(
            std::fs::metadata(&cached).unwrap().modified().unwrap(),
            before
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_sidecar_means_no_losses_not_an_error() {
        let dir = tmp("sidecar");
        let rec = dir.join("s-1.jsonl");
        assert!(sidecar_losses(&rec).is_empty(), "no sidecar");
        std::fs::write(dir.join("s-1.meta.json"), "{ not json").unwrap();
        assert!(sidecar_losses(&rec).is_empty(), "a corrupt sidecar");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
