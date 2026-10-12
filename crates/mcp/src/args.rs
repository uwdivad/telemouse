//! What a tool accepts, checked before anything is started or written.
//!
//! Two reasons this is its own module rather than a few `if`s in the tool
//! bodies. It is the half of the server that can be tested without a socket,
//! a panel or a recording; and refusing a bad argument *here* means the model
//! gets a sentence naming the allowed values instead of a `400` relayed from
//! ctl three hops away. The allow-lists are ctl's own, read from
//! `telemouse_core`, not a weakening of them: ctl checks again, and ctl is the
//! one that counts.

use telemouse_core::recordings::{self, MAX_ID_LEN, RECORDING_SUFFIX};
use telemouse_core::session::MAX_MARKER_LABEL_CHARS;

/// Flags `capture_start` may pass on: exactly the set ctl's `capture`
/// component allows, because both read it from [`telemouse_core::ctl`].
pub use telemouse_core::ctl::CAPTURE_FLAGS;

/// Components whose log this server can tail: the ones that write a log
/// file, plus the one-shot tasks whose output ctl keeps in memory.
pub const LOG_COMPONENTS: &[&str] = &[
    "ctl", "capture", "viz", "mcp", "analyze", "doctor", "trend", "report",
];

/// Components with a `<component>.log` of their own in `[ctl] log_dir`:
/// the three ctl launches, this server (`main.rs`) and the analyzer CLI
/// (`crates/analyze/src/main.rs`).
pub const LOG_FILES: &[&str] = &["ctl", "capture", "viz", "mcp", "analyze"];

/// Components ctl launches, and so keeps a live output ring for.
pub const RING_COMPONENTS: &[&str] = &["capture", "viz", "doctor", "trend", "report"];

/// Check `given` against [`CAPTURE_FLAGS`], keeping the order asked for.
pub fn capture_flags(given: &[String]) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(given.len());
    for flag in given {
        let flag = flag.trim();
        match CAPTURE_FLAGS.iter().find(|f| **f == flag) {
            Some(ok) => {
                if !out.iter().any(|f: &String| f == ok) {
                    out.push((*ok).to_string());
                }
            }
            None => {
                return Err(format!(
                    "{flag:?} is not a flag the control panel accepts for capture. Allowed: {}. Use save=true/false instead of --record/--no-record if you only want to choose whether the session is written.",
                    CAPTURE_FLAGS.join(", ")
                ));
            }
        }
    }
    if out.iter().any(|f| f == "--record") && out.iter().any(|f| f == "--no-record") {
        return Err(
            "--record and --no-record contradict each other; pass one, or use save.".into(),
        );
    }
    Ok(out)
}

/// A marker label: one line, not blank, at most [`MAX_MARKER_LABEL_CHARS`]
/// characters — the limit ctl and the capture agent's stdin reader apply.
pub fn marker_label(label: &str) -> Result<String, String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err("a marker needs a label; it is what the interval is called in the report (\"trial 1 start\").".into());
    }
    if trimmed.contains(['\n', '\r']) {
        return Err("a marker label is one line; the agent reads one marker per line.".into());
    }
    let count = trimmed.chars().count();
    if count > MAX_MARKER_LABEL_CHARS {
        return Err(format!(
            "that label is {count} characters; the limit is {MAX_MARKER_LABEL_CHARS}."
        ));
    }
    Ok(trimmed.to_string())
}

/// A component whose log may be tailed.
pub fn log_component(name: &str) -> Result<&'static str, String> {
    let name = name.trim().to_ascii_lowercase();
    LOG_COMPONENTS
        .iter()
        .find(|c| **c == name)
        .copied()
        .ok_or_else(|| {
            format!(
                "{name:?} is not a telemouse component. Known: {}.",
                LOG_COMPONENTS.join(", ")
            )
        })
}

/// A recording id as every id-taking tool accepts it: the bare id that
/// `sessions_list` returns, or the same with `.jsonl` after it.
///
/// The one place the rule is applied, so every tool takes the same spellings
/// and refuses the rest with the same sentence.
pub fn recording_id(arg: &str) -> Result<String, String> {
    let arg = arg.trim();
    let id = recordings::id_from_file_name(arg).unwrap_or(arg);
    if recordings::is_safe_id(id) {
        Ok(id.to_string())
    } else {
        Err(format!(
            "{arg:?} is not a recording id: use 1-{MAX_ID_LEN} characters of A-Z a-z 0-9 - _, optionally followed by {RECORDING_SUFFIX} (the id sessions_list returns)."
        ))
    }
}

/// A process id to hand to ctl's kill route.
pub fn pid(pid: i64) -> Result<u32, String> {
    match u32::try_from(pid) {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!(
            "{pid} is not a process id. Take one from health or from the panel's process table; the panel refuses anything its own scan would not list."
        )),
    }
}

/// Clamp an optional count into `min..=max`, with `default` when absent.
fn clamp(n: Option<u32>, default: u32, min: u32, max: u32) -> u32 {
    n.unwrap_or(default).clamp(min, max)
}

/// How many recordings `sessions_list` returns.
pub fn list_limit(n: Option<u32>) -> usize {
    clamp(n, 50, 1, 500) as usize
}

/// How many trailing rows `trend` returns, `None` for all of them.
pub fn last_rows(n: Option<u32>) -> Option<usize> {
    n.map(|n| n.clamp(1, 500) as usize)
}

/// How many log lines `logs_tail` returns.
pub fn tail_lines(n: Option<u32>) -> usize {
    clamp(n, 50, 1, 500) as usize
}

/// How long `live_stats` samples for. Zero is a single reading; the ceiling
/// keeps a tool call inside any client's patience.
pub fn sample_seconds(n: Option<u32>) -> u64 {
    clamp(n, 5, 0, 60) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// Every flag ctl allows is accepted here, and nothing else is: the
    /// list is core's, so this checks the use of it rather than a copy.
    #[test]
    fn every_flag_ctl_allows_is_accepted_and_nothing_else() {
        for flag in telemouse_core::ctl::CAPTURE_FLAGS {
            assert_eq!(
                capture_flags(&v(&[flag])).unwrap(),
                [*flag],
                "{flag} is on ctl's list and must pass"
            );
        }
        for bad in ["--duration-secs", "--config", "--log-dir", "doctor"] {
            assert!(
                capture_flags(&v(&[bad])).is_err(),
                "{bad} is not on ctl's list"
            );
        }
    }

    #[test]
    fn allowed_flags_pass_through_in_order_and_without_duplicates() {
        assert_eq!(
            capture_flags(&v(&["--no-kafka", "--print", "--no-kafka"])).unwrap(),
            ["--no-kafka", "--print"]
        );
        assert!(capture_flags(&[]).unwrap().is_empty());
        assert_eq!(capture_flags(&v(&[" --print "])).unwrap(), ["--print"]);
    }

    #[test]
    fn anything_else_is_refused_with_the_list() {
        for bad in ["--duration-secs 5", "-p", "--config", "run", "--RECORD", ""] {
            let e = capture_flags(&v(&[bad])).expect_err("must be refused");
            assert!(e.contains("--no-kafka"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn contradictory_recording_flags_are_refused() {
        let e = capture_flags(&v(&["--record", "--no-record"])).unwrap_err();
        assert!(e.contains("contradict"), "{e}");
    }

    #[test]
    fn a_label_is_one_non_blank_line() {
        assert_eq!(marker_label("  trial 1 start "), Ok("trial 1 start".into()));
        assert!(marker_label("   ").unwrap_err().contains("needs a label"));
        assert!(marker_label("a\nb").unwrap_err().contains("one line"));
        assert!(marker_label("a\rb").unwrap_err().contains("one line"));
        assert!(marker_label(&"x".repeat(MAX_MARKER_LABEL_CHARS)).is_ok());
        let e = marker_label(&"x".repeat(MAX_MARKER_LABEL_CHARS + 1)).unwrap_err();
        assert!(e.contains(&(MAX_MARKER_LABEL_CHARS + 1).to_string()), "{e}");
    }

    #[test]
    fn a_label_is_counted_in_characters_not_bytes() {
        // 120 multi-byte characters is a legal label; 120 bytes of them is
        // not the same thing.
        assert!(marker_label(&"é".repeat(MAX_MARKER_LABEL_CHARS)).is_ok());
        assert!(marker_label(&"é".repeat(MAX_MARKER_LABEL_CHARS + 1)).is_err());
    }

    #[test]
    fn components_are_named_case_insensitively_and_nothing_else_is() {
        assert_eq!(log_component("Capture"), Ok("capture"));
        assert_eq!(log_component(" viz "), Ok("viz"));
        assert_eq!(log_component("ctl"), Ok("ctl"));
        assert_eq!(log_component("MCP"), Ok("mcp"));
        let e = log_component("kafka").unwrap_err();
        assert!(e.contains("capture"), "{e}");
    }

    #[test]
    fn every_log_file_is_a_component_that_can_be_named() {
        for f in LOG_FILES {
            assert_eq!(log_component(f), Ok(*f));
        }
        assert!(LOG_FILES.contains(&"mcp") && LOG_FILES.contains(&"analyze"));
        for c in LOG_COMPONENTS {
            assert!(
                LOG_FILES.contains(c) || RING_COMPONENTS.contains(c),
                "{c} can be named but has neither a file nor a ring"
            );
        }
    }

    #[test]
    fn a_recording_id_is_taken_with_or_without_its_suffix_and_never_as_a_path() {
        assert_eq!(recording_id("s-1"), Ok("s-1".into()));
        assert_eq!(recording_id(" s-1.jsonl "), Ok("s-1".into()));
        for bad in [
            "../s-1",
            "a/b.jsonl",
            "",
            ".x",
            "s 1",
            "s-1.jsonl.jsonl",
            ".jsonl",
            "c:stream",
        ] {
            let e = recording_id(bad).expect_err("must be refused");
            assert!(e.contains("is not a recording id"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn a_pid_must_be_a_positive_u32() {
        assert_eq!(pid(4321), Ok(4321));
        for bad in [0, -1, i64::from(u32::MAX) + 1] {
            assert!(pid(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn counts_are_clamped_rather_than_refused() {
        assert_eq!(list_limit(None), 50);
        assert_eq!(list_limit(Some(0)), 1);
        assert_eq!(list_limit(Some(100_000)), 500);
        assert_eq!(tail_lines(None), 50);
        assert_eq!(tail_lines(Some(1_000)), 500);
        assert_eq!(last_rows(None), None);
        assert_eq!(last_rows(Some(0)), Some(1));
        assert_eq!(last_rows(Some(9_999)), Some(500));
        assert_eq!(sample_seconds(None), 5);
        assert_eq!(sample_seconds(Some(0)), 0, "zero means one reading");
        assert_eq!(sample_seconds(Some(600)), 60);
    }
}
