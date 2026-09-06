//! Reading the on-disk recording directory (`recordings/<session_id>.jsonl`)
//! for the replay UI.
//!
//! Everything here is pure filesystem logic with no HTTP types, so the path
//! validation that keeps `/api/session/{id}` from serving arbitrary files can
//! be tested directly.

use std::io::{Read, Seek, SeekFrom};
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
    /// Wall-clock UTC µs of the session envelope's anchor (`anchor.utc_us`,
    /// falling back to `started_utc_us`), or `None` if the first line is not
    /// a session envelope. This is the zero of the page's replay timeline,
    /// which is what lets "go to 21:14:03" become a seek offset.
    pub started_utc_us: Option<i64>,
    /// Wall-clock UTC µs of the last batch or marker in the file, read from
    /// the file's tail so a 500 MB recording costs the same as a 5 KB one.
    /// `None` if the tail holds no timestamped line.
    pub ended_utc_us: Option<i64>,
}

/// How much of a recording's head and tail is inspected for timestamps.
/// A session envelope (device list, sens table, monitors) is a few KB; a
/// batch line at 448 events is ~20 KB. 64 KB covers both with room.
const PROBE_BYTES: u64 = 64 * 1024;

/// Wall-clock span `(started, ended)` of one recording, each `None` when the
/// corresponding end of the file does not carry a usable timestamp.
///
/// Reads at most [`PROBE_BYTES`] from each end: the first line for the
/// session anchor, the last timestamped line for the end. Deliberately
/// tolerant — a truncated last line (the agent was killed mid-write) simply
/// falls back to the previous complete line.
pub fn probe_time_range(path: &Path) -> (Option<i64>, Option<i64>) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (None, None);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);

    let mut head = Vec::new();
    if f.by_ref().take(PROBE_BYTES).read_to_end(&mut head).is_err() {
        return (None, None);
    }
    let first = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let started = serde_json::from_slice::<serde_json::Value>(first)
        .ok()
        .filter(|v| v["type"] == "session")
        .and_then(|v| {
            v["anchor"]["utc_us"]
                .as_i64()
                .or_else(|| v["started_utc_us"].as_i64())
        });

    let tail_start = len.saturating_sub(PROBE_BYTES);
    let mut tail = Vec::new();
    if tail_start > 0 {
        if f.seek(SeekFrom::Start(tail_start)).is_err() || f.read_to_end(&mut tail).is_err() {
            return (started, None);
        }
    } else {
        tail = head;
    }
    let ended = tail
        .split(|&b| b == b'\n')
        .rev()
        .filter(|l| !l.is_empty())
        .find_map(line_utc_us);
    (started, ended)
}

/// The wall-clock UTC µs a batch or marker line ends at, if it parses.
///
/// A batch's `ts_anchor_us` is its first event; adding the last event's QPC
/// offset would need the session's `qpc_freq`, and a batch spans ≤ one
/// `window_ms` (50 ms by default), which is below what "go to a time" can
/// usefully resolve anyway.
fn line_utc_us(line: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    match v["type"].as_str()? {
        "batch" => v["ts_anchor_us"].as_i64(),
        "marker" => v["ts_utc_us"].as_i64(),
        _ => None,
    }
}

/// True if `id` is shaped like a session id we are willing to look up.
///
/// This is the cheap first gate; [`resolve_recording`] additionally requires
/// the derived path to be a regular `.jsonl` file directly in the recording
/// directory.
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
            let (started_utc_us, ended_utc_us) = probe_time_range(&path);
            Some(SessionEntry {
                id,
                path: path.to_string_lossy().replace('\\', "/"),
                bytes: meta.len(),
                modified_epoch_ms,
                started_utc_us,
                ended_utc_us,
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
/// The strict id alphabet makes `dir.join("{id}.jsonl")` a single direct child:
/// traversal (`../evil`), absolute paths, alternate separators, and NTFS
/// stream/short-name tricks cannot enter the derived path. `symlink_metadata`
/// also requires that child to be a regular file rather than following a link.
/// This intentionally avoids listing and timestamp-probing every recording in
/// the directory for a single download.
pub fn resolve_recording(dir: &Path, id: &str) -> Option<PathBuf> {
    if !is_safe_id(id) {
        return None;
    }
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::symlink_metadata(&path)
        .ok()?
        .file_type()
        .is_file()
        .then_some(path)
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
        assert!(json.contains(r#""started_utc_us":null"#));
        assert!(json.contains(r#""ended_utc_us":null"#));
    }

    const SESSION_LINE: &str = r#"{"type":"session","session_id":"s","started_utc_us":1756000000000000,"qpc_freq":10000000,"anchor":{"qpc":5000000000,"utc_us":1756000000000500,"qpc_freq":10000000}}"#;

    fn batch_line(ts_anchor_us: i64, pad: usize) -> String {
        // `pad` bloats the line so tests can push the file past PROBE_BYTES.
        format!(
            r#"{{"type":"batch","session_id":"s","seq_no":1,"ts_anchor_us":{ts_anchor_us},"note":"{}","events":[]}}"#,
            "x".repeat(pad)
        )
    }

    #[test]
    fn time_range_comes_from_the_anchor_and_the_last_timestamped_line() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!(
            "{SESSION_LINE}\n{}\n{}\n{{\"type\":\"marker\",\"session_id\":\"s\",\"seq_no\":3,\"ts_qpc\":1,\"ts_utc_us\":1756000009000000,\"label\":\"m\"}}\n",
            batch_line(1756000001000000, 0),
            batch_line(1756000005000000, 0),
        );
        write(tmp.path(), "s.jsonl", &body);
        let list = list_recordings(tmp.path());
        assert_eq!(
            list[0].started_utc_us,
            Some(1756000000000500),
            "anchor.utc_us wins over started_utc_us"
        );
        assert_eq!(list[0].ended_utc_us, Some(1756000009000000));
    }

    #[test]
    fn time_range_survives_a_truncated_last_line_and_a_large_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut body = format!("{SESSION_LINE}\n");
        // Well past PROBE_BYTES so the head and tail windows do not overlap.
        for i in 0..20 {
            body.push_str(&batch_line(1756000000000000 + i * 1_000_000, 10_000));
            body.push('\n');
        }
        // The agent died mid-write: no trailing newline, unparseable JSON.
        body.push_str(r#"{"type":"batch","ts_anchor_us":1756000099"#);
        write(tmp.path(), "s.jsonl", &body);
        assert!(body.len() as u64 > 2 * PROBE_BYTES);

        let (started, ended) = probe_time_range(&tmp.path().join("s.jsonl"));
        assert_eq!(started, Some(1756000000000500));
        assert_eq!(ended, Some(1756000019000000));
    }

    #[test]
    fn time_range_is_none_for_files_without_envelopes() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "junk.jsonl", "not json\n{\"type\":\"other\"}\n");
        write(tmp.path(), "empty.jsonl", "");
        assert_eq!(
            probe_time_range(&tmp.path().join("junk.jsonl")),
            (None, None)
        );
        assert_eq!(
            probe_time_range(&tmp.path().join("empty.jsonl")),
            (None, None)
        );
    }

    #[test]
    fn bundled_demo_recording_reports_its_time_range() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap();
        let Some(path) = resolve_recording(&repo_root.join("recordings"), "demo-session") else {
            return;
        };
        let (started, ended) = probe_time_range(&path);
        assert_eq!(started, Some(1_756_000_000_000_000));
        assert!(ended.unwrap() > started.unwrap());
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
            "demo.jsonl", // extension is added by us, not supplied
            "demo:$DATA", // NTFS alternate data stream
            "demo ",      // trailing space
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
        // A directory with the right suffix is not a recording.
        std::fs::create_dir(tmp.path().join("folder.jsonl")).unwrap();
        assert!(resolve_recording(tmp.path(), "folder").is_none());
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
