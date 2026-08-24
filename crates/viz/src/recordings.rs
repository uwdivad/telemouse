//! Reading the on-disk recording directory (`recordings/<session_id>.jsonl`)
//! for the replay UI.
//!
//! Everything here is pure filesystem logic with no HTTP types, so the path
//! validation that keeps `/api/session/{id}` from serving arbitrary files can
//! be tested directly.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;

/// One recorded session offered to the replay mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionEntry {
    /// File stem — also the session id, and the `{id}` in `/api/session/{id}`.
    pub id: String,
    /// Path as displayed in the UI (not used for lookup; lookup re-derives it).
    pub path: String,
    pub bytes: u64,
    pub modified_epoch_ms: i64,
}

/// True if `id` is shaped like a session id we are willing to look up.
///
/// This is the cheap first gate; [`resolve_recording`] additionally requires
/// the id to actually appear in the directory listing, so even an id that slips
/// past this can only ever name a `.jsonl` file that is directly in the
/// recording directory.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// List `*.jsonl` files directly inside `dir`, newest first.
///
/// A missing or unreadable directory is not an error: the replay UI simply
/// shows an empty list (conventions: no panics on degraded environments).
pub fn list_recordings(dir: &Path) -> Vec<SessionEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionEntry> = rd
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            if !path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
            {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            if !is_safe_id(&id) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let modified_epoch_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Some(SessionEntry {
                id,
                path: path.to_string_lossy().replace('\\', "/"),
                bytes: meta.len(),
                modified_epoch_ms,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.modified_epoch_ms
            .cmp(&a.modified_epoch_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Resolve `id` to a readable recording path, or `None` if it is not one of the
/// files this server is willing to serve.
///
/// Deliberately implemented as "must appear in the directory listing" rather
/// than as string sanitising alone: traversal (`../evil`), absolute paths,
/// alternate separators, and NTFS stream/short-name tricks all fail the same
/// membership check.
pub fn resolve_recording(dir: &Path, id: &str) -> Option<PathBuf> {
    if !is_safe_id(id) {
        return None;
    }
    list_recordings(dir)
        .into_iter()
        .find(|e| e.id == id)
        .map(|_| dir.join(format!("{id}.jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn listing_returns_jsonl_files_with_sizes() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "alpha.jsonl", "0123456789");
        write(tmp.path(), "beta.jsonl", "012345678901234");
        write(tmp.path(), "notes.txt", "ignored");
        std::fs::create_dir(tmp.path().join("subdir.jsonl")).unwrap();

        let list = list_recordings(tmp.path());
        assert_eq!(list.len(), 2, "got {list:?}");

        let mut ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["alpha", "beta"]);

        let alpha = list.iter().find(|e| e.id == "alpha").unwrap();
        assert_eq!(alpha.bytes, 10);
        assert!(alpha.path.ends_with("alpha.jsonl"));
        assert!(alpha.modified_epoch_ms > 0);
    }

    #[test]
    fn listing_is_empty_for_a_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-dir");
        assert!(list_recordings(&missing).is_empty());
    }

    #[test]
    fn listing_is_serializable_for_the_api() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "s-1.jsonl", "{}");
        let json = serde_json::to_string(&list_recordings(tmp.path())).unwrap();
        assert!(json.contains(r#""id":"s-1""#));
        assert!(json.contains(r#""bytes":2"#));
        assert!(json.contains("modified_epoch_ms"));
    }

    #[test]
    fn resolve_accepts_a_listed_id() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "demo-session.jsonl", "{}");
        let p = resolve_recording(tmp.path(), "demo-session").unwrap();
        assert_eq!(p, tmp.path().join("demo-session.jsonl"));
        assert!(p.is_file());
    }

    #[test]
    fn resolve_rejects_traversal_and_absolute_paths() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "demo.jsonl", "{}");
        // A file that exists but lives outside the recording dir.
        let outside = tmp.path().parent().unwrap().join("evil.jsonl");
        std::fs::write(&outside, "secret").ok();

        for bad in [
            "../evil",
            "..\\evil",
            "../../Windows/System32/drivers/etc/hosts",
            "/etc/passwd",
            "C:/Windows/win.ini",
            "C:\\Windows\\win.ini",
            r"\\server\share\x",
            "demo/../demo",
            "demo.jsonl",  // extension is added by us, not supplied
            "demo:$DATA",  // NTFS alternate data stream
            "demo ",       // trailing space
            ".hidden",
            "",
        ] {
            assert!(
                resolve_recording(tmp.path(), bad).is_none(),
                "id {bad:?} must be rejected"
            );
        }
        std::fs::remove_file(outside).ok();
    }

    #[test]
    fn resolve_rejects_ids_not_present_in_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "demo.jsonl", "{}");
        // Well-formed but nonexistent.
        assert!(resolve_recording(tmp.path(), "nope").is_none());
        // Exists, but not as a .jsonl recording.
        write(tmp.path(), "notes.txt", "x");
        assert!(resolve_recording(tmp.path(), "notes").is_none());
    }

    /// The checked-in demo recording must stay a valid `Envelope` stream:
    /// it is what replay mode loads out of the box.
    #[test]
    fn bundled_demo_recording_is_a_valid_envelope_stream() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap()
            .to_path_buf();
        let dir = repo_root.join("recordings");
        let Some(path) = resolve_recording(&dir, "demo-session") else {
            return; // fixture not present in this checkout; nothing to verify
        };
        let text = std::fs::read_to_string(path).unwrap();
        let mut sessions = 0;
        let mut batches = 0;
        let mut markers = 0;
        let mut events = 0;
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let env = telemouse_core::wire::Envelope::from_json(line)
                .unwrap_or_else(|e| panic!("line {} is not an envelope: {e}", i + 1));
            match env {
                telemouse_core::wire::Envelope::Session(_) => {
                    assert_eq!(i, 0, "the session envelope must be the first line");
                    sessions += 1;
                }
                telemouse_core::wire::Envelope::Batch(b) => {
                    assert!(
                        b.events.len() <= telemouse_core::wire::MAX_EVENTS_PER_BATCH,
                        "batch {} exceeds the per-batch event cap",
                        b.seq_no
                    );
                    events += b.events.len();
                    batches += 1;
                }
                telemouse_core::wire::Envelope::Marker(_) => markers += 1,
            }
        }
        assert_eq!(sessions, 1);
        assert!(batches >= 15, "expected ~20 batches, got {batches}");
        assert!(markers >= 1);
        assert!(events > 1000, "expected a dense 1kHz path, got {events}");
    }

    #[test]
    fn safe_id_charset() {
        assert!(is_safe_id("s-2026-08-23_120000"));
        assert!(is_safe_id("abc123"));
        assert!(!is_safe_id("a b"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id("a.b"));
        assert!(!is_safe_id(&"x".repeat(129)));
    }
}
