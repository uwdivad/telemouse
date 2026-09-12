//! One way to start logging, shared by every binary — and the one piece of it
//! that a packaged build still needs.
//!
//! [`strip_ansi`] is always here: the control panel pipes a child's stdout
//! into a log file and a browser page, and a colourised line arriving there
//! is unreadable escape noise. It costs nothing when there is nothing to
//! strip (the input is returned borrowed).
//!
//! Everything else — [`init`] and [`LogFile`] — is behind the `logging`
//! feature, because a packaged build ships without `tracing-subscriber` at
//! all. Enabling it gives every binary the same two destinations: stderr
//! (coloured only when a human is actually looking at a terminal) and,
//! optionally, `<log_dir>/<component>.log`, size-rotated so an agent left
//! running for a week cannot fill the disk.

use std::borrow::Cow;

/// Start of every ANSI escape sequence.
const ESC: u8 = 0x1b;

/// `s` with ANSI escape sequences removed, borrowed unchanged when it has
/// none.
///
/// Handles what a colourising program actually emits: CSI sequences
/// (`ESC [` … final byte, i.e. the SGR colour codes), OSC sequences
/// (`ESC ]` … `BEL` or `ESC \`, used for window titles and hyperlinks), and
/// a stray `ESC` with nothing recognisable after it. Everything else is
/// left alone, so text that merely mentions a bracket survives intact.
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().contains(&ESC) {
        return Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != ESC {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1; // the ESC itself is never kept
        let Some(&kind) = b.get(i) else { break };
        match kind {
            b'[' => {
                // CSI: parameter and intermediate bytes, then one final byte.
                i += 1;
                while i < b.len() && (0x20..=0x3f).contains(&b[i]) {
                    i += 1;
                }
                if i < b.len() && (0x40..=0x7e).contains(&b[i]) {
                    i += 1;
                }
            }
            b']' => {
                // OSC: runs to BEL or the two-byte string terminator.
                i += 1;
                while i < b.len() {
                    if b[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if b[i] == ESC && b.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            // A lone ESC: drop it and keep whatever followed.
            _ => {}
        }
    }
    // Only whole ASCII sequences were removed, so this is always valid UTF-8;
    // the lossy form is here so a hand-crafted input can never panic.
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(feature = "logging")]
pub use imp::{LOG_ROTATE_BYTES, LogFile, LogInit, LogOptions, init};

#[cfg(feature = "logging")]
mod imp {
    use std::fs::File;
    use std::io::{self, IsTerminal, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    /// A log file this large is rotated to `<component>.log.1` (the previous
    /// `.1` is dropped). Two files is the whole policy: enough to still hold
    /// the run before the one that broke, bounded at 16 MB per component.
    pub const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

    /// How a binary wants to log.
    pub struct LogOptions<'a> {
        /// `capture` | `viz` | `analyze` | `ctl`; the file is
        /// `<component>.log`.
        pub component: &'a str,
        /// When `Some`, also write `<log_dir>/<component>.log` (never
        /// colourised, size-rotated). The directory is created if needed.
        pub log_dir: Option<&'a Path>,
        /// Filter used when `RUST_LOG` is unset, e.g. `"info"` or
        /// `"info,rskafka=warn"`.
        pub default_filter: &'a str,
    }

    /// What [`init`] ended up doing, so the caller can say so in its first
    /// log line instead of the operator guessing.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct LogInit {
        /// The log file actually opened.
        pub file: Option<PathBuf>,
        /// Why no file was opened, when one was asked for. A log directory
        /// that cannot be written is a warning, never a refusal to start.
        pub file_error: Option<String>,
        /// Whether the stderr layer emits colour.
        pub ansi: bool,
    }

    /// Colour only when a human is looking: a terminal on stderr, `NO_COLOR`
    /// unset, and a `TERM` that can render it. Pure so the policy is
    /// testable without an actual console.
    fn ansi_enabled(stderr_is_terminal: bool, no_color: bool, term: Option<&str>) -> bool {
        stderr_is_terminal && !no_color && term != Some("dumb")
    }

    /// Install the process-wide subscriber. Never panics and never fails:
    /// a bad `RUST_LOG` directive is ignored (the rest of the filter still
    /// applies), an unwritable log directory costs the file layer only, and
    /// a second call is a no-op because a subscriber is already installed.
    pub fn init(opts: LogOptions<'_>) -> LogInit {
        let filter = match std::env::var("RUST_LOG") {
            Ok(v) if !v.trim().is_empty() => EnvFilter::builder().parse_lossy(v),
            _ => EnvFilter::builder().parse_lossy(opts.default_filter),
        };
        let ansi = ansi_enabled(
            io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var("TERM").ok().as_deref(),
        );

        let mut out = LogInit {
            file: None,
            file_error: None,
            ansi,
        };
        let file_layer = match opts.log_dir {
            Some(dir) => match LogFile::open(dir, opts.component) {
                Ok(f) => {
                    out.file = Some(f.path().to_path_buf());
                    Some(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_target(false)
                            .with_writer(Mutex::new(f)),
                    )
                }
                Err(e) => {
                    out.file_error = Some(format!("{}: {e}", dir.display()));
                    None
                }
            },
            None => None,
        };

        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_target(false)
            .with_writer(io::stderr);

        // `try_init` rather than `init`: a second call (a test binary, a
        // library embedding us) must not take the process down.
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .with(file_layer)
            .try_init();
        out
    }

    /// An append-only `<component>.log` that rotates itself *while running*,
    /// not only when it is opened — a chatty child (`--print` is ~14 MB an
    /// hour) used to grow one file for the life of the process.
    ///
    /// Wrap it in a [`std::sync::Mutex`] to use it as a
    /// [`tracing_subscriber::fmt::MakeWriter`].
    #[derive(Debug)]
    pub struct LogFile {
        file: File,
        path: PathBuf,
        rotated: PathBuf,
        /// Bytes in the current file: its size at open, plus what we wrote.
        written: u64,
    }

    impl LogFile {
        /// Open `<dir>/<component>.log` for appending, creating `dir` and
        /// rotating first if the file is already over
        /// [`LOG_ROTATE_BYTES`].
        pub fn open(dir: &Path, component: &str) -> io::Result<Self> {
            std::fs::create_dir_all(dir)?;
            let path = dir.join(format!("{component}.log"));
            let rotated = dir.join(format!("{component}.log.1"));
            if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_ROTATE_BYTES) {
                let _ = std::fs::rename(&path, &rotated);
            }
            let file = File::options().create(true).append(true).open(&path)?;
            let written = file.metadata().map(|m| m.len()).unwrap_or(0);
            Ok(Self {
                file,
                path,
                rotated,
                written,
            })
        }

        /// The file currently being written.
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// Move the full file aside and start a new one. A failure here
        /// keeps the current file: losing rotation is better than losing
        /// the log, and this runs inside the writer, where reporting the
        /// problem through `tracing` would re-enter the subscriber.
        fn rotate(&mut self) {
            let _ = self.file.flush();
            if std::fs::rename(&self.path, &self.rotated).is_err() {
                return;
            }
            if let Ok(f) = File::options().create(true).append(true).open(&self.path) {
                self.file = f;
                self.written = 0;
            }
        }
    }

    impl Write for LogFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = self.file.write(buf)?;
            self.written = self.written.saturating_add(n as u64);
            if self.written > LOG_ROTATE_BYTES {
                self.rotate();
            }
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn colour_only_for_a_human_at_a_terminal() {
            assert!(ansi_enabled(true, false, Some("xterm-256color")));
            assert!(ansi_enabled(true, false, None));
            assert!(!ansi_enabled(false, false, Some("xterm")), "redirected");
            assert!(!ansi_enabled(true, true, Some("xterm")), "NO_COLOR");
            assert!(!ansi_enabled(true, false, Some("dumb")), "TERM=dumb");
        }

        #[test]
        fn log_file_rotates_a_file_that_is_already_too_big() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("capture.log");
            let f = File::create(&path).unwrap();
            f.set_len(LOG_ROTATE_BYTES + 1).unwrap();
            drop(f);

            let mut log = LogFile::open(dir.path(), "capture").unwrap();
            writeln!(log, "fresh").unwrap();
            log.flush().unwrap();
            assert_eq!(log.path(), path);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
            assert_eq!(
                std::fs::metadata(dir.path().join("capture.log.1"))
                    .unwrap()
                    .len(),
                LOG_ROTATE_BYTES + 1
            );
        }

        #[test]
        fn log_file_rotates_while_running_and_keeps_two_files() {
            let dir = tempfile::tempdir().unwrap();
            let mut log = LogFile::open(dir.path(), "viz").unwrap();
            // One write past the threshold, then one after it, so the
            // rotation has to happen mid-life rather than at open.
            let big = vec![b'x'; LOG_ROTATE_BYTES as usize + 1];
            log.write_all(&big).unwrap();
            log.write_all(b"after\n").unwrap();
            log.flush().unwrap();

            let current = std::fs::read_to_string(dir.path().join("viz.log")).unwrap();
            assert_eq!(current, "after\n", "the new file starts empty");
            assert_eq!(
                std::fs::metadata(dir.path().join("viz.log.1"))
                    .unwrap()
                    .len(),
                big.len() as u64
            );
        }

        #[test]
        fn opening_creates_the_directory_and_appends() {
            let dir = tempfile::tempdir().unwrap();
            let nested = dir.path().join("a").join("b");
            {
                let mut log = LogFile::open(&nested, "ctl").unwrap();
                writeln!(log, "one").unwrap();
                log.flush().unwrap();
            }
            {
                let mut log = LogFile::open(&nested, "ctl").unwrap();
                writeln!(log, "two").unwrap();
                log.flush().unwrap();
            }
            assert_eq!(
                std::fs::read_to_string(nested.join("ctl.log")).unwrap(),
                "one\ntwo\n"
            );
        }

        /// The file layer is built from `Mutex<LogFile>`; if that ever stops
        /// being a `MakeWriter` this stops compiling.
        #[test]
        fn log_file_is_a_make_writer() {
            let dir = tempfile::tempdir().unwrap();
            let log = LogFile::open(dir.path(), "analyze").unwrap();
            let _layer = tracing_subscriber::fmt::layer::<tracing_subscriber::Registry>()
                .with_writer(Mutex::new(log));
        }

        #[test]
        fn init_is_safe_to_call_twice_and_reports_its_file() {
            let dir = tempfile::tempdir().unwrap();
            let first = init(LogOptions {
                component: "test",
                log_dir: Some(dir.path()),
                default_filter: "info",
            });
            assert_eq!(
                first.file.as_deref(),
                Some(dir.path().join("test.log").as_path())
            );
            assert_eq!(first.file_error, None);
            // A second call cannot install a subscriber, and must not panic.
            let second = init(LogOptions {
                component: "test",
                log_dir: None,
                default_filter: "not a valid filter!!",
            });
            assert_eq!(second.file, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_returned_borrowed() {
        let s = "no escapes here [1m";
        assert!(matches!(strip_ansi(s), Cow::Borrowed(_)));
        assert_eq!(strip_ansi(s), s);
    }

    #[test]
    fn colour_codes_are_removed() {
        assert_eq!(
            strip_ansi("\u{1b}[32mINFO\u{1b}[0m telemouse ready"),
            "INFO telemouse ready"
        );
        // Multi-parameter SGR, cursor movement, erase-line.
        assert_eq!(
            strip_ansi("\u{1b}[1;38;5;208mwarn\u{1b}[0m\u{1b}[2K\u{1b}[1A"),
            "warn"
        );
    }

    #[test]
    fn osc_sequences_are_removed_with_either_terminator() {
        assert_eq!(strip_ansi("\u{1b}]0;a title\u{7}text"), "text");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link"), "link");
    }

    #[test]
    fn truncated_and_lone_escapes_do_not_lose_text() {
        assert_eq!(strip_ansi("a\u{1b}"), "a");
        assert_eq!(strip_ansi("a\u{1b}[32"), "a", "unterminated CSI");
        assert_eq!(strip_ansi("a\u{1b}]0;no end"), "a", "unterminated OSC");
        assert_eq!(strip_ansi("a\u{1b}Zb"), "aZb", "lone ESC keeps its byte");
    }

    #[test]
    fn non_ascii_text_survives() {
        assert_eq!(
            strip_ansi("\u{1b}[31m2.5 µs — ünïcode\u{1b}[0m"),
            "2.5 µs — ünïcode"
        );
    }
}
