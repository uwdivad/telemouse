//! The one atomic-publish routine every cache in this crate writes through.
//!
//! The listing index and the per-session report cache are both disposable
//! acceleration structures that another process (a second `list`, the control
//! panel, the MCP server) may be reading at the same moment. A reader must see
//! the previous document or the new one, never half of one, so both are
//! written to a uniquely named sibling, flushed and synced, then renamed over
//! the published name.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes temp files from concurrent writers inside one process; the
/// process id distinguishes processes.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Publish `path` atomically. `write` fills a buffered writer over a fresh
/// sibling file; on success the sibling is flushed, synced to disk and renamed
/// over `path`, on failure it is removed. Parent directories are created.
///
/// The temp name starts with a dot and ends in `.tmp`, so it can never be
/// mistaken for a recording (`*.jsonl`) or a report (`*.report.json`) by a
/// directory listing that runs while it exists.
pub(crate) fn write_atomic(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(".{name}.{}.{seq}.tmp", std::process::id()));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|file| {
            let mut w = BufWriter::with_capacity(1 << 20, file);
            write(&mut w)?;
            w.flush()?;
            w.get_ref().sync_all()?;
            drop(w);
            fs::rename(&temp, path)
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_publish_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("x.json");
        write_atomic(&path, |w| w.write_all(b"one")).unwrap();
        write_atomic(&path, |w| w.write_all(b"two")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("x.json")]);
    }

    #[test]
    fn a_failed_write_keeps_the_old_document_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        write_atomic(&path, |w| w.write_all(b"old")).unwrap();
        let err = write_atomic(&path, |_| Err(io::Error::other("boom"))).unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
