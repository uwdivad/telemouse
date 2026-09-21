//! What a tool accepts, checked before anything is started or written.
//!
//! Two reasons this is its own module rather than a few `if`s in the tool
//! bodies. It is the half of the server that can be tested without a socket,
//! a panel or a recording; and refusing a bad argument *here* means the model
//! gets a sentence naming the allowed values instead of a `400` relayed from
//! ctl three hops away. The allow-lists are deliberately copies of ctl's, not
//! a weakening of them: ctl checks again, and ctl is the one that counts.

/// Flags `capture_start` may pass on, exactly the set ctl's `capture`
/// component allows (`crates/ctl/src/manager.rs`, `COMPONENTS`).
pub const CAPTURE_FLAGS: &[&str] = &[
    "--print",
    "--no-kafka",
    "--no-udp",
    "--record",
    "--no-record",
];

/// Longest marker label, as ctl and the capture agent's stdin reader count
/// it.
pub const MAX_LABEL: usize = 120;

/// Components whose log this server can tail: the three that write a log
/// file, plus the one-shot tasks whose output ctl keeps in memory.
pub const LOG_COMPONENTS: &[&str] = &["ctl", "capture", "viz", "doctor", "trend", "report"];

/// Components with a `<component>.log` of their own in `[ctl] log_dir`.
pub const LOG_FILES: &[&str] = &["ctl", "capture", "viz"];

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

/// A marker label: one line, not blank, at most [`MAX_LABEL`] characters.
pub fn marker_label(label: &str) -> Result<String, String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err("a marker needs a label; it is what the interval is called in the report (\"trial 1 start\").".into());
    }
    if trimmed.contains(['\n', '\r']) {
        return Err("a marker label is one line; the agent reads one marker per line.".into());
    }
    let count = trimmed.chars().count();
    if count > MAX_LABEL {
        return Err(format!(
            "that label is {count} characters; the limit is {MAX_LABEL}."
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

/// `session` as `capture_start` and ctl spell it: `<id>.jsonl`.
///
/// Accepts the bare id too, because that is what `sessions_list` returns and
/// what a model will reach for.
pub fn session_file(arg: &str) -> Result<String, String> {
    let arg = arg.trim();
    let id = arg
        .strip_suffix(&format!(".{}", telemouse_core::recordings::RECORDING_EXT))
        .unwrap_or(arg);
    telemouse_core::recordings::recording_file_name(id).ok_or_else(|| {
        format!(
            "{arg:?} is not a recording id: use 1-{} characters of A-Z a-z 0-9 - _, optionally with .jsonl.",
            telemouse_core::recordings::MAX_ID_LEN
        )
    })
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

    #[test]
    fn the_capture_flag_list_is_the_one_ctl_allows() {
        // If ctl's allow-list changes, this is the copy that must follow.
        assert_eq!(
            CAPTURE_FLAGS,
            &[
                "--print",
                "--no-kafka",
                "--no-udp",
                "--record",
                "--no-record"
            ]
        );
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
        assert!(marker_label(&"x".repeat(MAX_LABEL)).is_ok());
        let e = marker_label(&"x".repeat(MAX_LABEL + 1)).unwrap_err();
        assert!(e.contains("121"), "{e}");
    }

    #[test]
    fn a_label_is_counted_in_characters_not_bytes() {
        // 120 multi-byte characters is a legal label; 120 bytes of them is
        // not the same thing.
        assert!(marker_label(&"é".repeat(MAX_LABEL)).is_ok());
        assert!(marker_label(&"é".repeat(MAX_LABEL + 1)).is_err());
    }

    #[test]
    fn components_are_named_case_insensitively_and_nothing_else_is() {
        assert_eq!(log_component("Capture"), Ok("capture"));
        assert_eq!(log_component(" viz "), Ok("viz"));
        assert_eq!(log_component("ctl"), Ok("ctl"));
        let e = log_component("kafka").unwrap_err();
        assert!(e.contains("capture"), "{e}");
    }

    #[test]
    fn a_session_argument_becomes_a_file_name_and_never_a_path() {
        assert_eq!(session_file("s-1"), Ok("s-1.jsonl".into()));
        assert_eq!(session_file(" s-1.jsonl "), Ok("s-1.jsonl".into()));
        for bad in ["../s-1", "a/b.jsonl", "", ".x", "s 1"] {
            assert!(session_file(bad).is_err(), "{bad:?} must be refused");
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
