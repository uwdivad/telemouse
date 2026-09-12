//! The absolute locations the panel talks about, decided once at startup.
//!
//! A release is unzipped somewhere and started from a shortcut, so "the
//! config" and "the logs" are not places the user picked or can guess from
//! the working directory. Every surface — the status window header, the
//! tray menu's *Open …* items, the page — shows the same absolute strings,
//! built here so they cannot drift apart.

use std::path::Path;

use serde::Serialize;

/// This build's version, from Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where a newer build comes from. Static: the panel never phones home to
/// check, it just links.
pub const RELEASES_URL: &str = "https://github.com/uwdivad/telemouse/releases";

/// The documentation, when a release's own `docs/` is not next to the exe.
pub const DOCS_URL: &str = "https://github.com/uwdivad/telemouse#readme";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Places {
    pub version: String,
    /// `http://…/` — where a browser reaches this panel.
    pub panel_url: String,
    /// The `telemouse.toml` in force, absolute.
    pub config: String,
    /// Where `ctl.log` and the per-component logs go; empty in a build
    /// without the `logging` feature, which writes none.
    pub logs: String,
    /// Where the panel looks for the binaries it launches.
    pub bin_dir: String,
    /// The shipped `docs/` directory if there is one next to the
    /// executable, otherwise [`DOCS_URL`].
    pub docs: String,
    pub releases: String,
}

impl Places {
    /// `docs/` beside the executable when a release put one there, else the
    /// README on GitHub. Either way it is something `ShellExecuteW` and an
    /// `<a href>` can both open.
    pub fn docs_target(exe_dir: Option<&Path>) -> String {
        match exe_dir.map(|d| d.join("docs")) {
            Some(d) if d.is_dir() => d.display().to_string(),
            _ => DOCS_URL.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_the_crates_own() {
        assert!(!VERSION.is_empty());
        assert!(
            VERSION.chars().next().unwrap().is_ascii_digit(),
            "{VERSION}"
        );
    }

    #[test]
    fn docs_fall_back_to_github_when_none_are_shipped() {
        let dir = crate::manager::tmpdir("docs");
        assert_eq!(Places::docs_target(Some(&dir)), DOCS_URL);
        assert_eq!(Places::docs_target(None), DOCS_URL);
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        assert_eq!(
            Places::docs_target(Some(&dir)),
            dir.join("docs").display().to_string()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
