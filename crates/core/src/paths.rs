//! Where the config is, and what its relative paths mean.
//!
//! telemouse runs two ways: from the repo, where the working directory is the
//! repo root and `telemouse.toml` sits right there, and from an unzipped
//! release, where a shortcut or the tray starts a binary with whatever
//! working directory Explorer felt like — often `C:\Windows\system32`. The
//! rules here make both work without the user thinking about it: a config
//! named but not found relative to the working directory is looked for next
//! to the executable, and **paths inside `telemouse.toml` are relative to the
//! config file's own directory**, so `dir = "recordings"` means the folder
//! beside the config, not beside whatever spawned the process.

use std::path::{Path, PathBuf};

/// The directory the running executable lives in.
pub fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(Path::to_path_buf)
}

/// Make `p` absolute without touching the filesystem, leaving it alone if
/// that is not possible (an empty path, say).
fn absolutize(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve the `--config` path a user gave into the file to actually read.
///
/// An absolute path, or one that exists relative to the working directory, is
/// used as given. Otherwise it is taken relative to the executable's
/// directory — which is where a release zip puts `telemouse.toml` and where
/// `telemouse-ctl` seeds one on first start. The result is absolute whenever
/// the platform can make it so, so every later error message names a real
/// location instead of a path that means something different per process.
pub fn locate_config(cli: &Path) -> PathBuf {
    if cli.is_absolute() || cli.exists() {
        return absolutize(cli);
    }
    match exe_dir() {
        Some(dir) => absolutize(&dir.join(cli)),
        None => absolutize(cli),
    }
}

/// The directory that relative paths inside `config` resolve against: the
/// config file's own directory, falling back to the executable's directory
/// and then the working directory when it has none.
pub fn config_base(config: &Path) -> PathBuf {
    match config.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => exe_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executable_has_a_directory() {
        let dir = exe_dir().expect("a test binary has a path");
        assert!(dir.is_absolute());
        assert!(dir.is_dir());
    }

    #[test]
    fn an_existing_or_absolute_path_is_used_as_given() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("telemouse.toml");
        std::fs::write(&cfg, "").unwrap();
        assert_eq!(locate_config(&cfg), cfg);

        // Absolute but missing: still used as given, so the "not found"
        // error names what the user asked for.
        let missing = tmp.path().join("nope.toml");
        assert_eq!(locate_config(&missing), missing);
    }

    #[test]
    fn a_missing_relative_path_falls_back_to_the_executable_directory() {
        let got = locate_config(Path::new("telemouse-does-not-exist.toml"));
        assert!(got.is_absolute());
        assert!(got.starts_with(exe_dir().unwrap()));
        assert_eq!(
            got.file_name().unwrap(),
            "telemouse-does-not-exist.toml",
            "the file name is preserved"
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_config_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("sub").join("telemouse.toml");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        assert_eq!(config_base(&cfg), tmp.path().join("sub"));
    }

    #[test]
    fn a_bare_file_name_resolves_against_the_executable_directory() {
        let base = config_base(Path::new("telemouse.toml"));
        assert!(base.is_absolute(), "{}", base.display());
        assert_eq!(Some(base.as_path()), exe_dir().as_deref());
    }
}
