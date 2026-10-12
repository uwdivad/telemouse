//! Vocabulary shared by the control panel and the tools that drive it.
//!
//! `telemouse-ctl` starts the other binaries only with flags from a fixed
//! allow-list; the MCP server checks the same list before it forwards a
//! request, so a model gets a precise refusal instead of a relayed `400`.
//! Both read it from here so the two can never disagree.

/// Flags the control panel may pass to `telemouse run` (the capture agent),
/// in display order. **The single source of truth**: ctl's catalogue and
/// the MCP server's `capture_start` both check against this list, and ctl
/// remains the one that enforces it.
pub const CAPTURE_FLAGS: &[&str] = &[
    "--print",
    "--no-kafka",
    "--no-udp",
    "--no-record",
    "--record",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_flags_are_unique_long_flags() {
        for (i, f) in CAPTURE_FLAGS.iter().enumerate() {
            assert!(f.starts_with("--") && f.len() > 2, "{f}");
            assert!(!CAPTURE_FLAGS[..i].contains(f), "duplicate {f}");
        }
    }
}
