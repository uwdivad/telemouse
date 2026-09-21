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

use std::path::{Path, PathBuf};

use telemouse_analyze::load::{self, SessionIndexEntry};
use telemouse_analyze::report::ReportSummary;
use telemouse_analyze::series::Params;
use telemouse_analyze::trend::{self, TrendRow};
use telemouse_core::recordings;

/// Where the recordings and the report cache are.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// `[recording] dir`, already resolved against the config's directory.
    pub dir: PathBuf,
    /// The cached `<id>.report.json` files. The same folder the control
    /// panel uses, so a report run from the panel and one run from here are
    /// the same work done once.
    pub cache: PathBuf,
}

/// The folder name the panel uses under the recordings directory.
pub const CACHE_DIR: &str = ".reports";

impl Analysis {
    /// Point at `dir` and the report cache beside it.
    pub fn new(dir: PathBuf) -> Self {
        let cache = dir.join(CACHE_DIR);
        Self { dir, cache }
    }

    /// The cache directory, created if it can be. A cache that cannot be
    /// written only costs time on the next call, so a failure here is a
    /// `None` and a debug line, never an error the user sees.
    fn cache_dir(&self) -> Option<&Path> {
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
        if !recordings::is_safe_id(id) {
            return Err(format!(
                "{id:?} is not a recording id: use 1-{} characters of A-Z a-z 0-9 - _ (this is the id sessions_list returns, without the .jsonl).",
                recordings::MAX_ID_LEN
            ));
        }
        let path = self.dir.join(format!("{id}.{}", recordings::RECORDING_EXT));
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

    /// The `telemouse-report-summary/1` headline for one recording.
    pub fn summary(&self, id: &str) -> Result<ReportSummary, String> {
        let path = self.recording(id)?;
        let (report, _cache) = trend::report_for(&path, self.cache_dir(), Params::default())
            .map_err(|e| format!("analyzing {}: {e:#}", path.display()))?;
        let losses = load::read_sidecar(&path)
            .map(|m| m.losses())
            .unwrap_or_default();
        Ok(report.summary(losses))
    }

    /// One row per session, oldest first, optionally cut to the last `last`.
    pub fn trend(&self, metrics: &[String], last: Option<usize>) -> Result<Vec<TrendRow>, String> {
        let mut rows = trend::compute(&self.dir, self.cache_dir(), Params::default(), metrics)
            .map_err(|e| format!("building a trend over {}: {e:#}", self.dir.display()))?;
        if let Some(n) = last
            && rows.len() > n
        {
            rows.drain(..rows.len() - n);
        }
        Ok(rows)
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
        let a = Analysis::new(PathBuf::from("recordings"));
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
        let a = Analysis::new(dir.clone());
        let e = a.recording("no-such-session").expect_err("not there");
        assert!(e.contains("no recording"), "{e}");
        assert!(e.contains(&dir.display().to_string()), "{e}");
        assert!(e.contains("sessions_list"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_safe_and_present_id_resolves_under_the_recordings_dir() {
        let dir = tmp("present");
        std::fs::write(dir.join("s-1.jsonl"), "").unwrap();
        let a = Analysis::new(dir.clone());
        assert_eq!(a.recording("s-1").unwrap(), dir.join("s-1.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_cache_sits_beside_the_recordings_where_the_panel_puts_it() {
        let a = Analysis::new(PathBuf::from("recordings"));
        assert_eq!(a.cache, PathBuf::from("recordings").join(".reports"));
    }

    #[test]
    fn listing_an_empty_directory_is_an_empty_list_not_an_error() {
        let dir = tmp("empty");
        let a = Analysis::new(dir.clone());
        assert_eq!(a.sessions(50).unwrap(), (0, Vec::new()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_a_directory_that_is_not_there_says_which_one() {
        let a = Analysis::new(PathBuf::from("no-such-recordings-dir-here"));
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
            let mut cfg = testutil::session_cfg();
            cfg.session_id = id.into();
            cfg.started_utc_us = testutil::FIXTURE_UTC0 + offset;
            let line = Envelope::Session(cfg).to_json().unwrap();
            std::fs::write(dir.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
        }
        let a = Analysis::new(dir.clone());
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
}
