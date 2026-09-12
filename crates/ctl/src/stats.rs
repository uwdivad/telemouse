//! What the capture agent says about itself, read off the lines it prints.
//!
//! The agent already emits everything an operator needs once every report
//! interval — event rate, how long the mouse has been still, what each sink
//! dropped, which game is in the foreground — as a `capture stats` line with
//! `key=value` fields. The panel is reading that stream anyway to fill the
//! log ring, so parsing the last one costs a `split` and gives the card, the
//! tray tooltip and `/api/state` real numbers instead of "running".
//!
//! Every field is optional on purpose: this is the output of *another
//! binary*, quite possibly an older one, and a field that disappears or is
//! renamed must cost a missing number, never a parse failure or a panic.
//! The whole module is behind the `observability` feature; a build without
//! it shows "running" and nothing else.

use std::path::Path;

use serde::Serialize;

/// The message the capture agent's periodic report carries. Everything
/// after it on the line is `key=value` fields.
const STATS_MESSAGE: &str = "capture stats";

/// Past this, a capture agent that is "running" is not capturing anything:
/// the mouse has not moved, or the raw-input registration is gone.
pub const IDLE_DEGRADED_S: f64 = 60.0;

/// The last `capture stats` line, parsed. Fields the child did not print
/// stay `None`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ChildStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_per_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_for_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drops: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsonl_dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kafka_dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub udp_unreachable: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_hz: Option<f64>,
    /// When this line was read.
    pub at_unix_s: u64,
}

impl ChildStats {
    /// Is something wrong that the icon should say so about? A sink that is
    /// dropping, events the ring lost, a datagram nobody is listening for,
    /// or a mouse that has not moved in a minute.
    pub fn degraded(&self) -> bool {
        let dropping = [
            self.drops,
            self.jsonl_dropped,
            self.kafka_dropped,
            self.udp_unreachable,
        ]
        .iter()
        .any(|v| v.is_some_and(|n| n > 0));
        dropping || self.idle_for_s.is_some_and(|s| s > IDLE_DEGRADED_S)
    }

    /// Why it is degraded, in a few words; `None` when it is not.
    pub fn degraded_reason(&self) -> Option<String> {
        for (n, what) in [
            (self.drops, "events dropped"),
            (self.jsonl_dropped, "recording dropping"),
            (self.kafka_dropped, "kafka dropping"),
            (self.udp_unreachable, "nothing listening on the udp port"),
        ] {
            if n.is_some_and(|v| v > 0) {
                return Some(what.to_string());
            }
        }
        match self.idle_for_s {
            Some(s) if s > IDLE_DEGRADED_S => Some(format!("no events for {s:.0}s")),
            _ => None,
        }
    }

    /// One line for the card and the status window: rate, game, idle time.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(4);
        if let Some(e) = self.events_per_s {
            parts.push(format!("{e:.0} ev/s"));
        }
        if let Some(hz) = self.poll_hz {
            parts.push(format!("{hz:.0} Hz"));
        }
        if let Some(g) = self.game.as_deref().filter(|g| !g.is_empty() && *g != "-") {
            parts.push(g.to_string());
        }
        if let Some(i) = self.idle_for_s {
            parts.push(format!("idle {i:.0}s"));
        }
        if let Some(r) = self.degraded_reason() {
            parts.push(r);
        }
        parts.join(" · ")
    }

    /// The short form the 127-character tray tooltip can afford.
    pub fn tooltip_note(&self) -> String {
        if let Some(r) = self.degraded_reason() {
            return r;
        }
        match self.events_per_s {
            Some(e) => format!("{e:.0} ev/s"),
            None => String::new(),
        }
    }
}

/// The recording a capture run is writing, and what the disk it lands on
/// has left.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingLive {
    pub session: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
}

impl RecordingLive {
    /// `recording → s-1.jsonl · 12.3 MB · 431 GB free`.
    pub fn summary(&self) -> String {
        let mut s = format!("recording → {}.jsonl", self.session);
        if let Some(b) = self.size_bytes {
            s.push_str(&format!(" · {:.1} MB", b as f64 / (1024.0 * 1024.0)));
        }
        if let Some(b) = self.free_bytes {
            s.push_str(&format!(
                " · {:.0} GB free",
                b as f64 / (1024.0 * 1024.0 * 1024.0)
            ));
        }
        s
    }
}

/// Parse one line of child output, if it is a `capture stats` report.
///
/// The line is whatever `tracing`'s formatter wrote: a timestamp, a level,
/// the message, then `key=value` pairs separated by spaces, with string
/// values sometimes quoted (`game="cs2.exe"`) and sometimes not
/// (`session=s-1`, which is recorded through `Display`).
pub fn parse_stats_line(line: &str) -> Option<ChildStats> {
    let rest = line.split_once(STATS_MESSAGE)?.1;
    let mut out = ChildStats {
        at_unix_s: crate::manager::now_unix(),
        ..Default::default()
    };
    let mut any = false;
    for (key, value) in fields(rest) {
        any = true;
        match key {
            "session" => out.session = Some(value),
            "game" => out.game = Some(value),
            "events_per_s" => out.events_per_s = value.parse().ok(),
            "idle_for_s" => out.idle_for_s = value.parse().ok(),
            "poll_hz" => out.poll_hz = value.parse().ok(),
            "drops" => out.drops = value.parse().ok(),
            "jsonl_dropped" => out.jsonl_dropped = value.parse().ok(),
            "kafka_dropped" => out.kafka_dropped = value.parse().ok(),
            "udp_unreachable" => out.udp_unreachable = value.parse().ok(),
            // A field this build does not know is still proof that this is
            // a report: a newer agent may print more than we read.
            _ => {}
        }
    }
    any.then_some(out)
}

/// `key=value` pairs out of one line. A value runs to the next whitespace,
/// unless it opens with a quote, in which case it runs to the closing one
/// (`\"` inside stays part of the value, unescaped).
fn fields(s: &str) -> Vec<(&str, String)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // Skip to the start of a token.
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' || i == start {
            // Not a key: skip the rest of this token.
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        let key = &s[start..i];
        i += 1; // '='
        let value = if b.get(i) == Some(&b'"') {
            i += 1;
            let vstart = i;
            let mut escaped = false;
            while i < b.len() {
                if !escaped && b[i] == b'"' {
                    break;
                }
                escaped = !escaped && b[i] == b'\\';
                i += 1;
            }
            let raw = &s[vstart..i.min(s.len())];
            if i < b.len() {
                i += 1; // closing quote
            }
            raw.replace("\\\"", "\"").replace("\\\\", "\\")
        } else {
            let vstart = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            s[vstart..i].to_string()
        };
        out.push((key, value));
    }
    out
}

/// Free bytes on the volume `dir` lives on, as the user's quota sees it.
/// `None` when the OS will not say (a path that does not exist yet, or a
/// platform where this is not implemented).
#[cfg(windows)]
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;

    // The directory need not exist yet (the first recording creates it), so
    // walk up to the first ancestor that does.
    let mut probe = Some(dir);
    while let Some(p) = probe {
        if p.is_dir() {
            break;
        }
        probe = p.parent();
    }
    let p = probe?;
    let wide: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free = 0u64;
    // SAFETY: a NUL-terminated wide path that outlives the call, and one
    // out-parameter on our stack.
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None).ok()?;
    }
    Some(free)
}

/// Off Windows the panel is a test harness; the number is not shown.
#[cfg(not(windows))]
pub fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly what `tracing`'s formatter writes for the agent's report
    /// (`with_target(false)`), quotes and all.
    const LINE: &str = "2026-09-12T09:12:33.481234Z  INFO capture stats session=s-20260912-091100 events_per_s=1002.4 events=120400 batches_per_s=20 drops=0 idle_for_s=0.4 udp_unreachable=0 jsonl_dropped=0 kafka_dropped=0 game=\"cs2.exe\" pointer_locked=true";

    #[test]
    fn the_agents_stats_line_becomes_numbers() {
        let s = parse_stats_line(LINE).expect("a stats line");
        assert_eq!(s.session.as_deref(), Some("s-20260912-091100"));
        assert_eq!(s.events_per_s, Some(1002.4));
        assert_eq!(s.idle_for_s, Some(0.4));
        assert_eq!(s.drops, Some(0));
        assert_eq!(s.jsonl_dropped, Some(0));
        assert_eq!(s.kafka_dropped, Some(0));
        assert_eq!(s.udp_unreachable, Some(0));
        assert_eq!(s.game.as_deref(), Some("cs2.exe"));
        assert_eq!(s.poll_hz, None, "not in this build's line");
        assert!(s.at_unix_s > 1_700_000_000);
        assert!(!s.degraded());
        assert_eq!(s.degraded_reason(), None);
        assert!(s.summary().contains("1002 ev/s"));
        assert!(s.summary().contains("cs2.exe"));
        assert_eq!(s.tooltip_note(), "1002 ev/s");
    }

    #[test]
    fn anything_that_is_not_a_stats_line_is_ignored() {
        assert!(parse_stats_line("INFO telemouse starting").is_none());
        assert!(parse_stats_line("").is_none());
        assert!(parse_stats_line("--- exited: code 1 ---").is_none());
        // The message with no fields after it is not a report.
        assert!(parse_stats_line("INFO capture stats").is_none());
    }

    /// An older agent prints fewer fields, and a future one may print more;
    /// both must parse to what they do say.
    #[test]
    fn missing_and_unknown_fields_are_tolerated() {
        let s = parse_stats_line("INFO capture stats events_per_s=12 something_new=7").unwrap();
        assert_eq!(s.events_per_s, Some(12.0));
        assert_eq!(s.session, None);
        assert_eq!(s.drops, None);
        assert!(!s.degraded(), "unknown fields say nothing about health");

        // A field whose value stops being a number costs that field only.
        let s = parse_stats_line("INFO capture stats drops=lots session=s-1").unwrap();
        assert_eq!(s.drops, None);
        assert_eq!(s.session.as_deref(), Some("s-1"));

        // A newer build that prints the polling rate.
        let s = parse_stats_line("INFO capture stats poll_hz=1000 events_per_s=999").unwrap();
        assert_eq!(s.poll_hz, Some(1000.0));
        assert!(s.summary().contains("1000 Hz"));
    }

    #[test]
    fn quoted_values_keep_their_spaces() {
        let s = parse_stats_line("INFO capture stats game=\"Rocket League.exe\" drops=1").unwrap();
        assert_eq!(s.game.as_deref(), Some("Rocket League.exe"));
        assert_eq!(s.drops, Some(1));
        let s = parse_stats_line("INFO capture stats game=\"a \\\"quoted\\\" name\"").unwrap();
        assert_eq!(s.game.as_deref(), Some("a \"quoted\" name"));
        // An unterminated quote takes the rest of the line and does not hang.
        let s = parse_stats_line("INFO capture stats game=\"never closed").unwrap();
        assert_eq!(s.game.as_deref(), Some("never closed"));
    }

    #[test]
    fn degraded_is_any_loss_or_a_minute_of_silence() {
        let base = parse_stats_line(LINE).unwrap();
        assert!(!base.degraded());
        for (mutate, reason) in [
            (
                ChildStats {
                    drops: Some(3),
                    ..base.clone()
                },
                "events dropped",
            ),
            (
                ChildStats {
                    jsonl_dropped: Some(1),
                    ..base.clone()
                },
                "recording dropping",
            ),
            (
                ChildStats {
                    kafka_dropped: Some(9),
                    ..base.clone()
                },
                "kafka dropping",
            ),
            (
                ChildStats {
                    udp_unreachable: Some(2),
                    ..base.clone()
                },
                "nothing listening on the udp port",
            ),
        ] {
            assert!(mutate.degraded(), "{reason}");
            assert_eq!(mutate.degraded_reason().as_deref(), Some(reason));
            assert_eq!(mutate.tooltip_note(), reason);
        }
        let idle = ChildStats {
            idle_for_s: Some(IDLE_DEGRADED_S + 1.0),
            ..base.clone()
        };
        assert!(idle.degraded());
        assert!(idle.degraded_reason().unwrap().starts_with("no events for"));
        assert!(
            !ChildStats {
                idle_for_s: Some(IDLE_DEGRADED_S),
                ..base
            }
            .degraded(),
            "exactly at the threshold is still fine"
        );
        assert!(!ChildStats::default().degraded(), "no fields, no verdict");
    }

    #[test]
    fn a_recording_reads_as_a_file_a_size_and_what_is_left() {
        let r = RecordingLive {
            session: "s-1".into(),
            file: "C:\\tm\\recordings\\s-1.jsonl".into(),
            size_bytes: Some(12_900_000),
            free_bytes: Some(463_000_000_000),
        };
        let s = r.summary();
        assert!(s.starts_with("recording → s-1.jsonl"), "{s}");
        assert!(s.contains("12.3 MB"), "{s}");
        assert!(s.contains("431 GB free"), "{s}");
        let bare = RecordingLive {
            size_bytes: None,
            free_bytes: None,
            ..r
        };
        assert_eq!(bare.summary(), "recording → s-1.jsonl");
    }

    #[test]
    fn free_space_is_known_for_a_real_directory() {
        let dir = std::env::temp_dir();
        if cfg!(windows) {
            let free = free_bytes(&dir).expect("the temp volume reports free space");
            assert!(free > 0);
            // A path that does not exist yet answers for the nearest parent
            // that does, because the recordings directory is created late.
            let nested = dir.join("telemouse-ctl-no-such-dir").join("deeper");
            assert!(free_bytes(&nested).is_some());
        } else {
            assert_eq!(free_bytes(&dir), None);
        }
    }
}
