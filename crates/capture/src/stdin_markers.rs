//! Markers from standard input.
//!
//! When the agent's stdin is a pipe rather than a console — the control panel
//! starts it that way — every line written to it becomes a marker, exactly as
//! if the hotkey had been pressed: `trial-3 start\n`, or
//! `{"label":"trial-3 start"}\n` for a writer that prefers JSON. A terminal
//! stdin is left alone, so an interactive run never swallows keystrokes, and
//! a `null` stdin reads EOF at once and the reader thread simply ends.
//!
//! The timestamp is taken when the line arrives, on this side of the pipe;
//! a marker written by another process is as late as that process was.

use std::io::{BufRead, IsTerminal};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::platform;
use crate::raw_input::RingWaker;
use crate::shipping::MarkerSignal;

/// Longest label kept, in characters. A marker is a tag, not a note.
pub const MAX_LABEL_CHARS: usize = 120;

/// The label one input line asks for, or `None` for a line that is not a
/// marker: blank, JSON without a string `label` (or `marker`), or nothing
/// left after control characters are dropped.
pub fn parse_line(line: &str) -> Option<String> {
    let s = line.trim();
    if s.is_empty() {
        return None;
    }
    let label: String = if s.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        v.get("label")
            .or_else(|| v.get("marker"))?
            .as_str()?
            .trim()
            .to_string()
    } else {
        s.to_string()
    };
    let cleaned: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    (!cleaned.trim().is_empty()).then(|| cleaned.trim().to_string())
}

/// Start the reader if stdin is not a terminal. Returns whether it started.
pub fn spawn(marker_tx: Sender<MarkerSignal>, waker: Arc<RingWaker>) -> bool {
    if std::io::stdin().is_terminal() {
        return false;
    }
    let spawned = std::thread::Builder::new()
        .name("telemouse-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut lines = stdin.lock().lines();
            let mut accepted = 0u64;
            loop {
                match lines.next() {
                    Some(Ok(line)) => {
                        let ts_qpc = platform::qpc();
                        let Some(label) = parse_line(&line) else {
                            tracing::debug!(line = %line.trim(), "stdin line is not a marker; ignored");
                            continue;
                        };
                        if marker_tx
                            .send(MarkerSignal {
                                ts_qpc,
                                label: label.clone(),
                            })
                            .is_err()
                        {
                            break;
                        }
                        accepted += 1;
                        // T2 may be parked indefinitely on an idle desk.
                        waker.wake();
                        tracing::debug!(label = %label, "marker from stdin");
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "stdin marker reader stopped");
                        break;
                    }
                    None => break,
                }
            }
            tracing::debug!(accepted, "stdin closed; no more markers from it");
        });
    match spawned {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, "could not start the stdin marker reader");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_are_labels() {
        assert_eq!(parse_line("round start\n"), Some("round start".into()));
        assert_eq!(parse_line("  trial-3  "), Some("trial-3".into()));
    }

    #[test]
    fn json_lines_take_label_or_marker() {
        assert_eq!(parse_line(r#"{"label":"clutch"}"#), Some("clutch".into()));
        assert_eq!(parse_line(r#"{"marker":" t1 ","at":3}"#), Some("t1".into()));
        assert_eq!(parse_line(r#"{"label": 4}"#), None);
        assert_eq!(parse_line(r#"{"note":"x"}"#), None);
        assert_eq!(parse_line("{not json"), None);
    }

    #[test]
    fn blank_and_control_only_lines_are_ignored() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   \r\n"), None);
        assert_eq!(parse_line("\u{1}\u{2}"), None);
        assert_eq!(parse_line("a\u{0}b"), Some("ab".into()));
    }

    #[test]
    fn labels_are_capped() {
        let long = "x".repeat(MAX_LABEL_CHARS * 2);
        assert_eq!(parse_line(&long).unwrap().chars().count(), MAX_LABEL_CHARS);
    }
}
