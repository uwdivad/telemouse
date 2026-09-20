//! The double-click guard.
//!
//! Someone who double-clicks `telemouse-analyze.exe` in Explorer gets a
//! console that Windows creates for the process and destroys the moment it
//! exits: clap's usage error flashes by unread. When that is what happened —
//! no arguments, a console nobody else is attached to, and both stdin and
//! stdout are that console — say what the tool is and hold the window open
//! until Enter.
//!
//! Every other start is left alone. In particular anything with piped stdio
//! (ctl, scripts, CI) can never reach the blocking read, because a pipe is
//! not a terminal.

use std::io::{BufRead, IsTerminal, Write};

/// What the process knows about how it was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Launch {
    /// Arguments after the program name.
    pub arg_count: usize,
    /// Processes attached to this console, as `GetConsoleProcessList`
    /// reports it; `None` where that is not known (no console, not Windows).
    pub console_processes: Option<u32>,
    pub stdin_is_terminal: bool,
    pub stdout_is_terminal: bool,
}

impl Launch {
    fn detect() -> Self {
        Self {
            arg_count: std::env::args_os().count().saturating_sub(1),
            console_processes: console_processes(),
            stdin_is_terminal: std::io::stdin().is_terminal(),
            stdout_is_terminal: std::io::stdout().is_terminal(),
        }
    }
}

/// True only for the Explorer double-click: nothing to do, a console that
/// will vanish with us, and a person in front of it to press Enter.
pub fn should_hold_window(launch: Launch) -> bool {
    launch.arg_count == 0
        && launch.console_processes == Some(1)
        && launch.stdin_is_terminal
        && launch.stdout_is_terminal
}

/// The text shown in the held window.
pub fn message() -> String {
    format!(
        "telemouse-analyze {version}\n\
         \n\
         This turns a recorded telemouse session into a report of aim metrics\n\
         (flicks, overshoot, settle time, tremor, clicks).\n\
         \n\
         It is a command-line tool, so double-clicking it has nothing to show.\n\
         The easy way: open telemouse-ctl, pick a recording, press Report.\n\
         \n\
         From a terminal opened in this folder:\n\
         \n\
         \x20   telemouse-analyze list\n\
         \x20   telemouse-analyze report recordings\\demo-session.jsonl\n\
         \x20   telemouse-analyze --help\n",
        version = env!("CARGO_PKG_VERSION")
    )
}

/// Run the guard. Returns `true` when the message was shown and the caller
/// should exit instead of parsing arguments.
pub fn hold_window_if_double_clicked() -> bool {
    if !should_hold_window(Launch::detect()) {
        return false;
    }
    let mut out = std::io::stdout().lock();
    // A console that goes away mid-write is not worth reporting to anyone.
    let _ = write!(out, "{}\nPress Enter to close this window. ", message());
    let _ = out.flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    true
}

#[cfg(windows)]
fn console_processes() -> Option<u32> {
    // The one kernel32 call this crate needs; not worth a `windows`
    // dependency in the shipped binary.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleProcessList(process_list: *mut u32, process_count: u32) -> u32;
    }
    let mut pids = [0u32; 4];
    // SAFETY: the pointer and length describe `pids`; the call writes at
    // most that many ids and returns how many are attached (0 = no console).
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    (n != 0).then_some(n)
}

#[cfg(not(windows))]
fn console_processes() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOUBLE_CLICK: Launch = Launch {
        arg_count: 0,
        console_processes: Some(1),
        stdin_is_terminal: true,
        stdout_is_terminal: true,
    };

    #[test]
    fn a_double_click_holds_the_window() {
        assert!(should_hold_window(DOUBLE_CLICK));
    }

    #[test]
    fn any_argument_means_someone_knows_what_they_are_doing() {
        assert!(!should_hold_window(Launch {
            arg_count: 1,
            ..DOUBLE_CLICK
        }));
    }

    #[test]
    fn a_shell_sharing_the_console_keeps_it_open_already() {
        for n in [2, 3, 17] {
            assert!(!should_hold_window(Launch {
                console_processes: Some(n),
                ..DOUBLE_CLICK
            }));
        }
    }

    #[test]
    fn no_console_or_not_windows_never_blocks() {
        assert!(!should_hold_window(Launch {
            console_processes: None,
            ..DOUBLE_CLICK
        }));
    }

    #[test]
    fn piped_stdio_never_blocks() {
        // ctl spawns the analyzer with pipes; a script may redirect either end.
        assert!(!should_hold_window(Launch {
            stdin_is_terminal: false,
            ..DOUBLE_CLICK
        }));
        assert!(!should_hold_window(Launch {
            stdout_is_terminal: false,
            ..DOUBLE_CLICK
        }));
        assert!(!should_hold_window(Launch {
            stdin_is_terminal: false,
            stdout_is_terminal: false,
            ..DOUBLE_CLICK
        }));
    }

    #[test]
    fn the_message_points_at_the_panel_and_gives_a_command() {
        let m = message();
        assert!(m.contains("command-line tool"));
        assert!(m.contains("telemouse-ctl"));
        assert!(m.contains("telemouse-analyze report "));
    }

    #[test]
    fn this_test_run_is_not_a_double_click() {
        // cargo passes the harness arguments or pipes its output; either way
        // the real probe must come back false here, or CI would hang.
        let l = Launch::detect();
        if l.arg_count > 0 || !l.stdout_is_terminal {
            assert!(!should_hold_window(l));
        }
    }
}
