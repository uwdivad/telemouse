//! What the window and the tray show, decided without Win32: the text of
//! the status window, the tooltip, which icon to wear, the popup menu and
//! its enabled items, and the icon pixels themselves. Everything here is a
//! pure function of a [`Snapshot`], so it is unit-tested on any OS and the
//! platform glue in `win.rs` only has to paint.

use std::net::SocketAddr;

use crate::gui::feed::Snapshot;
use crate::manager::{ComponentState, Kind, clip, describe_exit, short_hint};

/// `NOTIFYICONDATAW.szTip` is 128 UTF-16 units including the terminator.
pub const TOOLTIP_MAX_CHARS: usize = 127;

/// Width of the "last exit" column in the status window's component table.
/// A hint is far longer than this, so the cell is clipped and the full text
/// goes on its own line below the table.
const LAST_COL: usize = 18;

/// Width of the label column of the key/value block (`SAVE DATA`, `CONFIG`…).
const LABEL_COL: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// Nothing is being captured.
    Idle,
    /// The capture agent is running: the icon goes green.
    CaptureRunning,
    /// It is running, but something it reports is wrong — a sink dropping,
    /// events lost, or a mouse that has not moved in a minute. Amber.
    ///
    /// Only ever chosen in a build with the `observability` feature, which
    /// is what reads the agent's stats line; the state itself is always
    /// defined so the icon table and this enum cannot fall out of step.
    Degraded,
}

impl IconState {
    /// Index into the icon table `win.rs` keeps.
    pub fn index(self) -> usize {
        match self {
            Self::Idle => 0,
            Self::CaptureRunning => 1,
            Self::Degraded => 2,
        }
    }

    /// How many icons `win.rs` has to draw.
    pub const COUNT: usize = 3;
}

fn component<'a>(s: &'a Snapshot, id: &str) -> Option<&'a ComponentState> {
    s.components.iter().find(|c| c.id == id)
}

pub fn icon_state(s: &Snapshot) -> IconState {
    let Some(c) = component(s, "capture").filter(|c| c.running) else {
        return IconState::Idle;
    };
    #[cfg(feature = "observability")]
    if c.stats.as_ref().is_some_and(|st| st.degraded()) {
        return IconState::Degraded;
    }
    let _ = c;
    IconState::CaptureRunning
}

/// `h:mm:ss` since `since`, clamped at zero.
pub fn format_uptime(now_unix_s: u64, since_unix_s: u64) -> String {
    let d = now_unix_s.saturating_sub(since_unix_s);
    format!("{}:{:02}:{:02}", d / 3600, (d / 60) % 60, d % 60)
}

/// One word (plus uptime) for a service.
fn service_status(c: Option<&ComponentState>, now: u64) -> String {
    match c {
        Some(c) if c.running => match c.since_unix_s {
            Some(since) => format!("running {}", format_uptime(now, since)),
            None => "running".into(),
        },
        Some(c) if !c.bin_found => "not built".into(),
        Some(_) => "stopped".into(),
        None => "?".into(),
    }
}

/// "saving" / "not saving" for a running capture agent, nothing otherwise.
fn saving_note(c: Option<&ComponentState>) -> &'static str {
    match c {
        Some(c) if c.running && c.saving => " (saving)",
        Some(c) if c.running => " (not saving)",
        _ => "",
    }
}

/// The live numbers for the capture agent, short enough for a tooltip.
/// Empty in a build without `observability`, or before the agent has
/// printed its first report.
fn capture_note(c: Option<&ComponentState>) -> String {
    #[cfg(feature = "observability")]
    if let Some(c) = c.filter(|c| c.running) {
        return c
            .stats
            .as_ref()
            .map(|s| s.tooltip_note())
            .unwrap_or_default();
    }
    let _ = c;
    String::new()
}

/// The most recent failure worth a word in the tooltip: the short form of
/// the hint on a component that exited badly and is not running.
fn failure_note(s: &Snapshot) -> String {
    s.components
        .iter()
        .filter(|c| !c.running)
        .filter_map(|c| c.last_exit.as_ref().filter(|e| e.failed()).map(|e| (c, e)))
        .max_by_key(|(_, e)| e.at_unix_s)
        .map(|(c, e)| match &e.hint {
            Some(h) => format!("{}: {}", c.id, short_hint(h, 40)),
            None => format!("{} failed", c.id),
        })
        .unwrap_or_default()
}

/// Tray tooltip, never longer than [`TOOLTIP_MAX_CHARS`].
pub fn tooltip(s: &Snapshot) -> String {
    let cap = component(s, "capture");
    let mut text = format!(
        "telemouse-ctl — capture: {}{}, viz: {}",
        service_status(cap, s.now_unix_s),
        saving_note(cap),
        service_status(component(s, "viz"), s.now_unix_s),
    );
    for extra in [capture_note(cap), failure_note(s)] {
        if !extra.is_empty() {
            text.push_str(" — ");
            text.push_str(&extra);
            break;
        }
    }
    text.chars().take(TOOLTIP_MAX_CHARS).collect()
}

/// Whose log tail the window shows: the running capture agent, else the
/// running viz server, else whatever started or exited most recently.
pub fn focus_component(components: &[ComponentState]) -> Option<&ComponentState> {
    for id in ["capture", "viz"] {
        if let Some(c) = components.iter().find(|c| c.id == id && c.running) {
            return Some(c);
        }
    }
    components
        .iter()
        .filter_map(|c| {
            let t = c
                .since_unix_s
                .or(c.last_exit.as_ref().map(|e| e.at_unix_s))?;
            Some((t, c))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, c)| c)
}

fn hms_utc(unix_s: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        (unix_s / 3600) % 24,
        (unix_s / 60) % 60,
        unix_s % 60
    )
}

/// One row of the component table's "last exit" cell, clipped to the
/// column. The untruncated hint goes on a NOTES line of its own.
fn last_exit_cell(c: &ComponentState) -> String {
    let text = match &c.last_exit {
        None => "last: -".to_string(),
        Some(e) => format!("last: {}", describe_exit(e)),
    };
    clip(&text, LAST_COL)
}

/// The header every other section is read against: where the panel is,
/// which config it obeys, where it writes, and what it is.
fn header(s: &Snapshot) -> Vec<String> {
    let p = &s.places;
    let logs = if p.logs.is_empty() {
        "logging off (this build writes no log files)".to_string()
    } else {
        p.logs.clone()
    };
    /// `-` rather than an empty column, so a missing value is visible
    /// instead of looking like a rendering bug.
    fn or_dash(s: &str) -> &str {
        if s.is_empty() { "-" } else { s }
    }
    vec![
        format!(
            "{:<LABEL_COL$}{} (tray → Open in browser)",
            "PANEL",
            or_dash(&p.panel_url)
        ),
        format!("{:<LABEL_COL$}{}", "CONFIG", or_dash(&p.config)),
        format!("{:<LABEL_COL$}{logs}", "LOGS"),
        format!("{:<LABEL_COL$}{}", "VERSION", or_dash(&p.version)),
        String::new(),
    ]
}

/// What the window's embedded browser is doing, for the text view that
/// shows while it is not (yet) showing the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebStatus {
    /// The WebView2 runtime is being probed or the page is being created.
    Loading,
    /// The page is on screen; the text view is hidden.
    Hosted,
    /// The page could not be hosted; the text view is the window.
    Fallback {
        reason: String,
        /// The page was opened in the default browser instead.
        browser_opened: bool,
    },
}

/// The lines above the status text that explain why it is text and not
/// the page: nothing while hosted, one line while loading, a short
/// paragraph when hosting failed.
pub fn web_banner(status: &WebStatus, panel_url: &str) -> Vec<String> {
    match status {
        WebStatus::Hosted => Vec::new(),
        WebStatus::Loading => vec!["Loading the panel…".to_string(), String::new()],
        WebStatus::Fallback {
            reason,
            browser_opened,
        } => {
            let mut v = vec![format!("The panel page is not shown here: {reason}.")];
            v.push(if *browser_opened {
                format!(
                    "It is open in your browser at {panel_url} — tray → Open in browser reopens it."
                )
            } else {
                format!("Open it in your browser: {panel_url} (tray → Open in browser).")
            });
            if !reason.contains("--no-webview") {
                v.push(
                    "Install the WebView2 Runtime from Microsoft to see the panel in this window."
                        .to_string(),
                );
            }
            v.push(String::new());
            v
        }
    }
}

/// How long a control may keep text whose *numbers* have moved on — a
/// clock, an uptime, a CPU column — before it is rewritten anyway, so the
/// view still reads as live. Anything that changes a word is shown at once.
pub const SLOW_REPAINT_S: u64 = 5;

/// A string that was pushed into a Win32 control, and the snapshot second
/// it was pushed at. `at_unix_s = 0` is "so long ago it does not matter",
/// which is also what a never-painted control gets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Painted {
    pub text: String,
    pub at_unix_s: u64,
}

impl Painted {
    /// Record what the control now shows.
    pub fn set(&mut self, text: String, now_unix_s: u64) {
        self.text = text;
        self.at_unix_s = now_unix_s;
    }
}

/// Every run of ASCII digits collapsed to a single `#`: what a rendering
/// says, with the numbers taken out.
fn worded(s: &str) -> impl Iterator<Item = char> + '_ {
    let mut in_digits = false;
    s.chars().filter_map(move |c| {
        if c.is_ascii_digit() {
            let first = !in_digits;
            in_digits = true;
            first.then_some('#')
        } else {
            in_digits = false;
            Some(c)
        }
    })
}

/// Whether `next` is worth pushing into a control that shows `shown`.
///
/// Both the text view and the tray tooltip carry a clock, uptimes and
/// counters, so they differ on nearly every refresh even when nothing the
/// reader cares about moved — and rewriting the fallback `EDIT` costs
/// ~0.3% of a core at one refresh a second (docs/PERFORMANCE-2026-09-20.md,
/// L2). So: identical text is never pushed again; text whose words changed
/// (a state, a path, a new log line) is pushed at once; text where only
/// digits moved waits until the control has been stale for
/// [`SLOW_REPAINT_S`].
pub fn worth_painting(shown: &Painted, next: &str, now_unix_s: u64) -> bool {
    if shown.text == next {
        return false;
    }
    if !worded(&shown.text).eq(worded(next)) {
        return true;
    }
    now_unix_s.saturating_sub(shown.at_unix_s) >= SLOW_REPAINT_S
}

/// What the hosted page is allowed to cost while the window is not on
/// screen.
///
/// `SetIsVisible(false)` alone only stops rendering: the page's timers, its
/// `/api/state` polling and all six `msedgewebview2` processes carry on
/// (0.32% of a core and 309 MiB, measured in
/// `docs/PERFORMANCE-2026-09-20.md`). Hiding to the tray therefore also asks
/// the runtime to suspend the page, which stops the lot until it is shown
/// again. The transitions are here so they can be tested without a runtime;
/// `webview.rs` only carries out the [`PowerAction`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerState {
    /// The window is on screen and the page is running.
    #[default]
    Visible,
    /// Hidden; `TrySuspend` has been asked for and has not answered yet.
    Suspending,
    /// Hidden and suspended: no timers, no polling, no rendering.
    Suspended,
    /// Hidden but awake — a runtime too old for `ICoreWebView2_3`, a page
    /// that had not finished loading, or a suspension the runtime refused.
    /// What the window did before: rendering stops and nothing else.
    HiddenAwake,
}

/// What `webview.rs` has to do to the controller for a transition. Every
/// step is best-effort; nothing blocks on a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// The transition needs no COM call.
    Nothing,
    /// Tell the page to stop polling, then `SetIsVisible(false)`.
    Hide,
    /// The same, then `TrySuspend` — which is refused while the controller
    /// is still visible, so the order is part of the contract.
    HideAndSuspend,
    /// `SetIsVisible(true)`, `Resume` first when `resume`, then focus and
    /// one immediate refresh of the page.
    Show { resume: bool },
    /// `Resume` alone: a `TrySuspend` answered after the window was back,
    /// so a page that did suspend has to be woken under a visible window.
    Resume,
}

/// The window is being shown — tray click, tray menu, or anything else that
/// goes through `win::set_visible`.
pub fn power_show(state: PowerState) -> (PowerState, PowerAction) {
    let resume = matches!(state, PowerState::Suspending | PowerState::Suspended);
    (PowerState::Visible, PowerAction::Show { resume })
}

/// The window is being hidden to the tray. `can_suspend` is "this runtime
/// has `ICoreWebView2_3` *and* the page has finished loading"; a page that
/// finishes loading later is offered again from `on_navigated`, which is
/// why [`PowerState::HiddenAwake`] is not a dead end.
pub fn power_hide(state: PowerState, can_suspend: bool) -> (PowerState, PowerAction) {
    match (state, can_suspend) {
        (PowerState::Visible | PowerState::HiddenAwake, true) => {
            (PowerState::Suspending, PowerAction::HideAndSuspend)
        }
        (PowerState::Visible, false) => (PowerState::HiddenAwake, PowerAction::Hide),
        // Already hidden, and nothing new to try.
        (s, _) => (s, PowerAction::Nothing),
    }
}

/// `TrySuspend` answered (or was refused outright).
pub fn power_suspended(state: PowerState, ok: bool) -> (PowerState, PowerAction) {
    match (state, ok) {
        (PowerState::Suspending, true) => (PowerState::Suspended, PowerAction::Nothing),
        (PowerState::Suspending, false) => (PowerState::HiddenAwake, PowerAction::Nothing),
        // Shown again before the answer arrived: a suspension that went
        // through anyway would leave a frozen page in a visible window.
        (PowerState::Visible, true) => (PowerState::Visible, PowerAction::Resume),
        (s, _) => (s, PowerAction::Nothing),
    }
}

/// The text view: the web banner, then [`render_text`].
pub fn window_text(s: &Snapshot, status: &WebStatus) -> String {
    let mut out = web_banner(status, &s.places.panel_url).join("\r\n");
    if !out.is_empty() {
        out.push_str("\r\n");
    }
    out.push_str(&render_text(s));
    out
}

/// The whole body of the status window. CRLF line endings: a Win32 `EDIT`
/// control silently drops bare `\n`. Fixed-width columns for a monospace
/// font.
pub fn render_text(s: &Snapshot) -> String {
    let now = s.now_unix_s;
    let mut out: Vec<String> = Vec::with_capacity(64);
    out.extend(header(s));
    out.push(format!(
        "COMPONENTS                                              refreshed {} UTC",
        hms_utc(now)
    ));
    for c in &s.components {
        let state = if c.running {
            "running"
        } else if c.bin_found {
            "stopped"
        } else {
            "not built"
        };
        let pid = c
            .pid
            .map(|p| format!("pid {p}"))
            .unwrap_or_else(|| "-".into());
        let up = match (c.running, c.since_unix_s) {
            (true, Some(since)) => format!("up {}", format_uptime(now, since)),
            _ => "-".into(),
        };
        let kind = match c.kind {
            Kind::Service => "service",
            Kind::Task => "task",
        };
        out.push(format!(
            "{:<17} {:<8} {:<10} {:<11} {:<12} {:<LAST_COL$} {}",
            c.label,
            kind,
            state,
            pid,
            up,
            last_exit_cell(c),
            c.summary
        ));
    }
    // The clipped cells above lose the part that says what to do about it.
    for c in &s.components {
        for line in exit_notes(c) {
            out.push(format!("  ! {} {line}", c.label));
        }
    }
    out.push(String::new());
    let cap = component(s, "capture");
    let default = if s.recording_enabled {
        format!("on → {}", s.recording_dir)
    } else {
        "off".to_string()
    };
    let now_line = match cap {
        Some(c) if c.running && c.saving => format!("; capture is SAVING → {}", s.recording_dir),
        Some(c) if c.running => "; capture is NOT saving".to_string(),
        _ => String::new(),
    };
    out.push(format!(
        "{:<LABEL_COL$}default {default} (telemouse.toml [recording] enabled){now_line}",
        "SAVE DATA"
    ));
    for line in capture_lines(cap) {
        out.push(format!("{:<LABEL_COL$}{line}", "CAPTURE"));
    }
    let hotkey = if s.hotkey.is_empty() {
        "none (telemouse.toml [ctl] hotkey)".to_string()
    } else if s.hotkey_registered == Some(false) {
        format!(
            "{} — NOT REGISTERED (another program holds it; change [ctl] hotkey)",
            s.hotkey
        )
    } else {
        format!(
            "{} → new session: (re)start capture, saving → {}",
            s.hotkey, s.recording_dir
        )
    };
    out.push(format!("{:<LABEL_COL$}{hotkey}", "HOTKEY"));
    out.push(String::new());
    out.push("RELATED PROCESSES".into());
    if s.processes.is_empty() {
        out.push("  (none)".into());
    } else {
        out.push(format!(
            "{:>7}  {:<8} {:<24} {:>6} {:>9}",
            "PID", "KIND", "NAME", "CPU%", "MEM MB"
        ));
        for p in &s.processes {
            let kind = format!("{:?}", p.kind).to_ascii_lowercase();
            let me = if p.is_self { "  (this panel)" } else { "" };
            out.push(format!(
                "{:>7}  {:<8} {:<24} {:>6.1} {:>9.1}{}",
                p.pid, kind, p.name, p.cpu_pct, p.mem_mb, me
            ));
        }
    }
    out.push(String::new());
    match focus_component(&s.components) {
        Some(c) => {
            out.push(format!("LOG — {} (last {} lines)", c.label, c.log.len()));
            out.extend(c.log.iter().cloned());
        }
        None => out.push("LOG — nothing has run yet".into()),
    }
    let mut text = out.join("\r\n");
    text.push_str("\r\n");
    text
}

/// The full hint and the child's own last line, for a component whose last
/// exit was a failure. Empty otherwise.
fn exit_notes(c: &ComponentState) -> Vec<String> {
    let Some(e) = c.last_exit.as_ref().filter(|e| e.failed()) else {
        return Vec::new();
    };
    #[cfg_attr(not(feature = "observability"), allow(unused_mut))]
    let mut out = Vec::new();
    if let Some(h) = &e.hint {
        out.push(h.clone());
    }
    if let Some(l) = &e.last_line {
        out.push(format!("last line: {l}"));
    }
    out
}

/// What the capture agent itself reports: its rate and, while it saves, the
/// recording and the disk. Empty in a build without `observability`.
fn capture_lines(c: Option<&ComponentState>) -> Vec<String> {
    #[cfg_attr(not(feature = "observability"), allow(unused_mut))]
    let mut out = Vec::new();
    #[cfg(feature = "observability")]
    if let Some(c) = c.filter(|c| c.running) {
        if let Some(st) = c.stats.as_ref() {
            let s = st.summary();
            if !s.is_empty() {
                out.push(s);
            }
        }
        if let Some(r) = c.recording.as_ref() {
            out.push(r.summary());
        }
    }
    let _ = c;
    out
}

pub const MENU_TOGGLE_WINDOW: u16 = 1001;
pub const MENU_START_CAPTURE: u16 = 1002;
pub const MENU_STOP_CAPTURE: u16 = 1003;
pub const MENU_START_VIZ: u16 = 1004;
pub const MENU_STOP_VIZ: u16 = 1005;
pub const MENU_OPEN_PANEL: u16 = 1006;
pub const MENU_EXIT: u16 = 1007;
pub const MENU_START_CAPTURE_NOSAVE: u16 = 1008;
/// Stop the running capture agent and start one that saves: a fresh
/// session file. What the hotkey does.
pub const MENU_NEW_SESSION: u16 = 1009;
pub const MENU_OPEN_LOGS: u16 = 1010;
pub const MENU_OPEN_RECORDINGS: u16 = 1011;
pub const MENU_EDIT_CONFIG: u16 = 1012;
pub const MENU_OPEN_DOCS: u16 = 1013;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub id: u16,
    pub label: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

fn item(id: u16, label: &str, enabled: bool) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        id,
        label: label.into(),
        enabled,
    })
}

/// Start is offered when the binary exists and nothing is running; stop when
/// it is running. One of the two per service, never both.
fn service_items(
    c: Option<&ComponentState>,
    label: &str,
    start_id: u16,
    stop_id: u16,
) -> MenuEntry {
    match c {
        Some(c) if c.running => item(stop_id, &format!("Stop {label}"), true),
        Some(c) => item(start_id, &format!("Start {label}"), c.bin_found),
        None => item(start_id, &format!("Start {label}"), false),
    }
}

/// The capture entries: while running, one *Stop* that says whether data is
/// being saved plus *New session* (stop, then start saving); otherwise two
/// *Start*s — with and without saving — both greyed when the binary is
/// missing. The item the hotkey is equivalent to carries the chord as its
/// accelerator text (after a tab, which a Win32 menu right-aligns) — but
/// only while the chord is actually registered: an accelerator that does
/// nothing is worse than none.
fn capture_items(s: &Snapshot) -> Vec<MenuEntry> {
    let accel = if s.hotkey.is_empty() || s.hotkey_registered == Some(false) {
        String::new()
    } else {
        format!("\t{}", s.hotkey)
    };
    match component(s, "capture") {
        Some(c) if c.running => vec![
            item(
                MENU_STOP_CAPTURE,
                if c.saving {
                    "Stop capture (saving data)"
                } else {
                    "Stop capture (not saving)"
                },
                true,
            ),
            item(
                MENU_NEW_SESSION,
                &format!(
                    "New session (restart capture, save data → {}){accel}",
                    s.recording_dir
                ),
                true,
            ),
        ],
        c => {
            let ok = c.is_some_and(|c| c.bin_found);
            vec![
                item(
                    MENU_START_CAPTURE,
                    &format!("Start capture (save data → {}){accel}", s.recording_dir),
                    ok,
                ),
                item(MENU_START_CAPTURE_NOSAVE, "Start capture (don't save)", ok),
            ]
        }
    }
}

pub fn menu(s: &Snapshot, window_visible: bool) -> Vec<MenuEntry> {
    let mut v = vec![
        item(
            MENU_TOGGLE_WINDOW,
            if window_visible {
                "Hide window"
            } else {
                "Show window"
            },
            true,
        ),
        MenuEntry::Separator,
    ];
    v.extend(capture_items(s));
    v.extend([
        service_items(
            component(s, "viz"),
            "viz server",
            MENU_START_VIZ,
            MENU_STOP_VIZ,
        ),
        MenuEntry::Separator,
        item(MENU_OPEN_PANEL, "Open in browser", true),
        // Everything a support question ends up asking for, one click away.
        item(
            MENU_OPEN_LOGS,
            "Open logs folder",
            !s.places.logs.is_empty(),
        ),
        item(
            MENU_OPEN_RECORDINGS,
            "Open recordings folder",
            !s.recording_dir.is_empty(),
        ),
        item(
            MENU_EDIT_CONFIG,
            "Edit telemouse.toml",
            !s.places.config.is_empty(),
        ),
        item(MENU_OPEN_DOCS, "Open docs", !s.places.docs.is_empty()),
        MenuEntry::Separator,
        item(MENU_EXIT, "Exit (stops what the panel started)", true),
    ]);
    v
}

/// What a menu item opens, when it opens something. `None` for the items
/// that do something instead.
pub fn menu_target(s: &Snapshot, id: u16) -> Option<&str> {
    match id {
        MENU_OPEN_PANEL => Some(s.places.panel_url.as_str()),
        MENU_OPEN_LOGS => Some(s.places.logs.as_str()),
        MENU_OPEN_RECORDINGS => Some(s.recording_dir.as_str()),
        MENU_EDIT_CONFIG => Some(s.places.config.as_str()),
        MENU_OPEN_DOCS => Some(s.places.docs.as_str()),
        _ => None,
    }
    .filter(|t| !t.is_empty())
}

/// Where a browser reaches the panel. An unspecified bind address
/// (`0.0.0.0`, `::`) is not a destination; loopback is.
pub fn panel_url(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        format!("http://127.0.0.1:{}/", addr.port())
    } else {
        format!("http://{addr}/")
    }
}

/// Icon pixels for `CreateIconIndirect`: a 32-bit BGRA colour bitmap and a
/// 1-bit AND mask (1 = transparent), rows padded to 16-bit boundaries as
/// `CreateBitmap` requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconPixels {
    pub size: u32,
    pub bgra: Vec<u32>,
    pub mask: Vec<u8>,
}

impl IconPixels {
    pub fn mask_stride(size: u32) -> usize {
        (size as usize).div_ceil(16) * 2
    }
}

/// A filled disc with a darker 1-px ring: grey when idle, green while the
/// capture agent runs, amber while it runs but reports trouble.
pub fn icon_bitmap(state: IconState, size: u32) -> IconPixels {
    let (fill, ring) = match state {
        IconState::Idle => (0xFF80_8080u32, 0xFF50_5050u32),
        IconState::CaptureRunning => (0xFF2E_A043u32, 0xFF1F_6F2Eu32),
        // BGRA: amber is a lot of blue-channel-zero, so this is 0xFFB02E RGB.
        IconState::Degraded => (0xFF2E_B0FFu32, 0xFF1F_78B0u32),
    };
    let n = size as usize;
    let stride = IconPixels::mask_stride(size);
    let mut bgra = vec![0u32; n * n];
    let mut mask = vec![0u8; stride * n];
    let centre = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 0.5;
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - centre, y as f32 - centre);
            let d = (dx * dx + dy * dy).sqrt();
            let px = if d <= radius - 1.2 {
                fill
            } else if d <= radius {
                ring
            } else {
                0
            };
            bgra[y * n + x] = px;
            if px == 0 {
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    IconPixels { size, bgra, mask }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::{ExitInfo, Flag};
    use crate::places::Places;
    use crate::procs::{ProcInfo, ProcKind};

    /// The key/value lines are padded to a fixed label column; building the
    /// expectation the same way keeps these assertions from being a
    /// space-counting exercise.
    fn labelled(label: &str, value: &str) -> String {
        format!("{label:<LABEL_COL$}{value}")
    }

    fn comp(
        id: &'static str,
        label: &'static str,
        kind: Kind,
        running: bool,
        bin_found: bool,
    ) -> ComponentState {
        ComponentState {
            id,
            label,
            summary: "summary",
            kind,
            bin: id,
            bin_path: format!("{id}.exe"),
            bin_found,
            flags: Vec::<Flag>::new(),
            takes_session: false,
            markers: false,
            running,
            pid: running.then_some(4242),
            since_unix_s: running.then_some(1_000_000),
            last_exit: None,
            exits: 0,
            unexpected_exits: 0,
            args: Vec::new(),
            saving: running && id == "capture",
            log: vec!["line one".into(), "line two".into()],
            log_seq: 2,
            #[cfg(feature = "observability")]
            stats: None,
            #[cfg(feature = "observability")]
            recording: None,
            #[cfg(feature = "observability")]
            foreground_seen: Vec::new(),
        }
    }

    fn places() -> Places {
        Places {
            version: "0.1.1".into(),
            panel_url: "http://127.0.0.1:7880/".into(),
            config: "C:\\tm\\telemouse.toml".into(),
            logs: "C:\\tm\\logs".into(),
            bin_dir: "C:\\tm".into(),
            docs: "C:\\tm\\docs".into(),
            releases: crate::places::RELEASES_URL.into(),
            webview_data: "C:\\Users\\x\\AppData\\Local\\telemouse\\WebView2".into(),
        }
    }

    fn snap(capture_running: bool, viz_running: bool) -> Snapshot {
        Snapshot {
            recording_enabled: true,
            recording_dir: "recordings".into(),
            hotkey: "Ctrl+Alt+R".into(),
            hotkey_registered: Some(true),
            now_unix_s: 1_000_000 + 3661,
            places: places(),
            components: vec![
                comp(
                    "capture",
                    "Capture agent",
                    Kind::Service,
                    capture_running,
                    true,
                ),
                comp("viz", "Viz server", Kind::Service, viz_running, true),
                comp("doctor", "Doctor", Kind::Task, false, false),
            ],
            processes: vec![ProcInfo {
                pid: 77,
                parent: None,
                name: "telemouse-ctl.exe".into(),
                cmd: "telemouse-ctl".into(),
                exe: None,
                kind: ProcKind::Ctl,
                cpu_pct: 0.25,
                mem_mb: 11.5,
                started_unix_s: 1,
                is_self: true,
            }],
        }
    }

    #[test]
    fn icon_follows_capture_only() {
        assert_eq!(icon_state(&snap(false, true)), IconState::Idle);
        assert_eq!(icon_state(&snap(true, false)), IconState::CaptureRunning);
        assert_eq!(icon_state(&Snapshot::default()), IconState::Idle);
        let indices: Vec<usize> = [
            IconState::Idle,
            IconState::CaptureRunning,
            IconState::Degraded,
        ]
        .iter()
        .map(|s| s.index())
        .collect();
        assert_eq!(indices, vec![0, 1, 2]);
        assert_eq!(IconState::COUNT, 3);
    }

    /// The amber state: capture is running, and what it reports is not fine.
    #[cfg(feature = "observability")]
    #[test]
    fn a_dropping_capture_turns_the_icon_amber() {
        let mut s = snap(true, false);
        s.components[0].stats = Some(crate::stats::ChildStats {
            events_per_s: Some(1000.0),
            drops: Some(0),
            ..Default::default()
        });
        assert_eq!(icon_state(&s), IconState::CaptureRunning);
        s.components[0].stats = Some(crate::stats::ChildStats {
            events_per_s: Some(1000.0),
            jsonl_dropped: Some(12),
            ..Default::default()
        });
        assert_eq!(icon_state(&s), IconState::Degraded);
        assert!(
            tooltip(&s).contains("recording dropping"),
            "{}",
            tooltip(&s)
        );
        assert!(render_text(&s).contains("recording dropping"));
        // Degraded only counts while it is actually running.
        s.components[0].running = false;
        assert_eq!(icon_state(&s), IconState::Idle);
    }

    #[cfg(feature = "observability")]
    #[test]
    fn the_window_shows_the_recording_and_what_the_disk_has_left() {
        let mut s = snap(true, false);
        s.components[0].stats = Some(crate::stats::ChildStats {
            events_per_s: Some(998.0),
            session: Some("s-1".into()),
            ..Default::default()
        });
        s.components[0].recording = Some(crate::stats::RecordingLive {
            session: "s-1".into(),
            file: "C:\\tm\\recordings\\s-1.jsonl".into(),
            size_bytes: Some(12_900_000),
            free_bytes: Some(463_000_000_000),
        });
        let t = render_text(&s);
        assert!(t.contains(&labelled("CAPTURE", "998 ev/s")), "{t}");
        assert!(
            t.contains("recording → s-1.jsonl · 12.3 MB · 431 GB free"),
            "{t}"
        );
        assert!(tooltip(&s).contains("998 ev/s"));
    }

    #[test]
    fn uptime_is_h_mm_ss_and_never_negative() {
        assert_eq!(format_uptime(3661, 0), "1:01:01");
        assert_eq!(format_uptime(59, 0), "0:00:59");
        assert_eq!(format_uptime(0, 10), "0:00:00");
        assert_eq!(format_uptime(360_000, 0), "100:00:00");
    }

    #[test]
    fn tooltip_names_both_services_and_is_bounded() {
        let t = tooltip(&snap(true, false));
        assert_eq!(
            t,
            "telemouse-ctl — capture: running 1:01:01 (saving), viz: stopped"
        );
        let mut nosave = snap(true, false);
        nosave.components[0].saving = false;
        assert!(tooltip(&nosave).contains("(not saving)"));
        assert!(
            !tooltip(&snap(false, false)).contains("saving"),
            "no note while stopped"
        );
        assert!(tooltip(&Snapshot::default()).contains("capture: ?"));
        let mut long = snap(false, false);
        long.components[0].running = true;
        long.components[0].since_unix_s = Some(0);
        long.now_unix_s = u64::MAX / 2;
        assert!(tooltip(&long).chars().count() <= TOOLTIP_MAX_CHARS);
        let mut nb = snap(false, false);
        nb.components[1].bin_found = false;
        assert!(tooltip(&nb).ends_with("viz: not built"));
    }

    /// A component that died carries its reason into the tooltip, short.
    #[test]
    fn a_failure_reaches_the_tooltip_in_short_form() {
        let mut s = snap(false, false);
        s.components[1].last_exit = Some(
            ExitInfo::new(Some(1), 900).with_hint(Some(crate::manager::HINT_PORT_IN_USE.into())),
        );
        let t = tooltip(&s);
        assert!(t.contains("viz: port already in use"), "{t}");
        assert!(t.chars().count() <= TOOLTIP_MAX_CHARS);
        // A clean exit adds no failure note (the status itself still names viz).
        s.components[1].last_exit = Some(ExitInfo::new(Some(0), 900));
        let t = tooltip(&s);
        assert!(!t.contains(" — viz"), "{t}");
        assert!(!t.contains("port already in use"), "{t}");
    }

    #[test]
    fn web_banner_explains_the_text_view() {
        let s = snap(false, false);
        assert_eq!(window_text(&s, &WebStatus::Hosted), render_text(&s));
        let loading = window_text(&s, &WebStatus::Loading);
        assert!(
            loading.starts_with("Loading the panel…\r\n\r\nPANEL"),
            "{loading}"
        );
        let fb = window_text(
            &s,
            &WebStatus::Fallback {
                reason: "WebView2 runtime not installed".into(),
                browser_opened: true,
            },
        );
        assert!(fb.contains("WebView2 runtime not installed"), "{fb}");
        assert!(
            fb.contains("open in your browser at http://127.0.0.1:7880/"),
            "{fb}"
        );
        assert!(fb.contains("Install the WebView2 Runtime"), "{fb}");
        let off = window_text(
            &s,
            &WebStatus::Fallback {
                reason: "disabled with --no-webview".into(),
                browser_opened: false,
            },
        );
        assert!(
            off.contains("Open it in your browser: http://127.0.0.1:7880/"),
            "{off}"
        );
        assert!(!off.contains("Install the WebView2 Runtime"), "{off}");
    }

    /// The repaint rule the window and the tray both obey: nothing for
    /// unchanged text, at once for changed words, on the slow lane for a
    /// clock that moved.
    #[test]
    fn only_a_changed_word_repaints_at_once() {
        let painted = |text: &str, at: u64| Painted {
            text: text.into(),
            at_unix_s: at,
        };
        // Nothing has been painted yet: anything is worth painting.
        assert!(worth_painting(&Painted::default(), "PANEL  -", 0));
        // The same text, however old, is never pushed again.
        assert!(!worth_painting(
            &painted("up 1:01:01", 10),
            "up 1:01:01",
            9_999
        ));
        // Only digits moved: the slow lane, counted from the last paint.
        let clock = painted("refreshed 12:00:01 UTC", 100);
        assert!(!worth_painting(&clock, "refreshed 12:00:02 UTC", 101));
        assert!(!worth_painting(
            &clock,
            "refreshed 12:00:04 UTC",
            100 + SLOW_REPAINT_S - 1
        ));
        assert!(worth_painting(
            &clock,
            "refreshed 12:00:05 UTC",
            100 + SLOW_REPAINT_S
        ));
        // A number that grew a digit is still just a number.
        assert!(!worth_painting(&painted("9.9 MB", 100), "10.1 MB", 101));
        // A word changed: now, whatever the clock says.
        assert!(worth_painting(
            &painted("viz: stopped", 100),
            "viz: running",
            100
        ));
        assert!(worth_painting(
            &painted("line one", 100),
            "line one\r\nline two",
            100
        ));
    }

    /// The same rule over real renderings: a tick that only advanced the
    /// clock and the uptimes must not rewrite the control.
    #[test]
    fn a_tick_that_only_moves_the_clock_is_not_worth_a_repaint() {
        let s = snap(true, false);
        let shown = Painted {
            text: window_text(&s, &WebStatus::Hosted),
            at_unix_s: s.now_unix_s,
        };
        let mut later = s.clone();
        later.now_unix_s += 1;
        let next = window_text(&later, &WebStatus::Hosted);
        assert_ne!(next, shown.text, "the clock and the uptime did move");
        assert!(!worth_painting(&shown, &next, later.now_unix_s));

        // Capture stopping is a word, not a number.
        let mut stopped = later.clone();
        stopped.components[0].running = false;
        assert!(worth_painting(
            &shown,
            &window_text(&stopped, &WebStatus::Hosted),
            stopped.now_unix_s
        ));
        // So is a new log line, and so is the banner the fallback wears.
        let mut logged = later.clone();
        logged.components[0].log.push("line three".into());
        assert!(worth_painting(
            &shown,
            &window_text(&logged, &WebStatus::Hosted),
            logged.now_unix_s
        ));
        assert!(worth_painting(
            &shown,
            &window_text(&later, &WebStatus::Loading),
            later.now_unix_s
        ));
        // Tooltips go through the same rule.
        let tip = Painted {
            text: tooltip(&s),
            at_unix_s: s.now_unix_s,
        };
        assert!(!worth_painting(&tip, &tooltip(&later), later.now_unix_s));
        assert!(worth_painting(&tip, &tooltip(&stopped), stopped.now_unix_s));
    }

    /// Hidden to the tray the page is suspended; shown it is resumed, and
    /// every show refreshes it whether or not it was suspended.
    #[test]
    fn hiding_to_the_tray_suspends_the_page_and_showing_it_resumes() {
        let (hidden, act) = power_hide(PowerState::Visible, true);
        assert_eq!(hidden, PowerState::Suspending);
        assert_eq!(act, PowerAction::HideAndSuspend);
        let (asleep, act) = power_suspended(hidden, true);
        assert_eq!(asleep, PowerState::Suspended);
        assert_eq!(act, PowerAction::Nothing);
        // A second hide while already hidden asks for nothing again.
        assert_eq!(
            power_hide(asleep, true),
            (PowerState::Suspended, PowerAction::Nothing)
        );
        assert_eq!(
            power_show(asleep),
            (PowerState::Visible, PowerAction::Show { resume: true })
        );
        // Showing a window that never left only refreshes it.
        assert_eq!(
            power_show(PowerState::Visible),
            (PowerState::Visible, PowerAction::Show { resume: false })
        );
    }

    /// A runtime without `ICoreWebView2_3` (or a page that has not loaded)
    /// degrades to what the window always did: hide, and nothing else.
    #[test]
    fn a_runtime_that_cannot_suspend_just_hides() {
        let (state, act) = power_hide(PowerState::Visible, false);
        assert_eq!(state, PowerState::HiddenAwake);
        assert_eq!(act, PowerAction::Hide);
        assert_eq!(
            power_hide(state, false),
            (PowerState::HiddenAwake, PowerAction::Nothing),
            "nothing new to try"
        );
        // The page finished loading while hidden: now it can be suspended.
        assert_eq!(
            power_hide(state, true),
            (PowerState::Suspending, PowerAction::HideAndSuspend)
        );
        assert_eq!(
            power_show(state),
            (PowerState::Visible, PowerAction::Show { resume: false })
        );
    }

    /// `TrySuspend` answers on the message loop, so the window can be back
    /// on screen by then. A refusal is just as normal as a success.
    #[test]
    fn a_suspension_that_lands_after_the_window_is_back_is_undone() {
        assert_eq!(
            power_suspended(PowerState::Visible, true),
            (PowerState::Visible, PowerAction::Resume)
        );
        assert_eq!(
            power_suspended(PowerState::Visible, false),
            (PowerState::Visible, PowerAction::Nothing)
        );
        assert_eq!(
            power_suspended(PowerState::Suspending, false),
            (PowerState::HiddenAwake, PowerAction::Nothing),
            "a refusal leaves the page awake, not half-suspended"
        );
        // A late answer for a state nobody is waiting on changes nothing.
        assert_eq!(
            power_suspended(PowerState::Suspended, true),
            (PowerState::Suspended, PowerAction::Nothing)
        );
        assert_eq!(PowerState::default(), PowerState::Visible);
    }

    #[test]
    fn focus_prefers_running_services_then_recency() {
        let s = snap(true, true);
        assert_eq!(focus_component(&s.components).unwrap().id, "capture");
        let s = snap(false, true);
        assert_eq!(focus_component(&s.components).unwrap().id, "viz");
        let mut s = snap(false, false);
        assert!(focus_component(&s.components).is_none(), "nothing ever ran");
        s.components[2].last_exit = Some(ExitInfo::new(Some(0), 500));
        s.components[1].last_exit = Some(ExitInfo::new(Some(1), 900));
        assert_eq!(focus_component(&s.components).unwrap().id, "viz");
    }

    /// The block at the top says where everything is. A support question
    /// starts here, so it must be present even before anything has run.
    #[test]
    fn the_header_names_the_panel_config_logs_and_version() {
        let t = render_text(&snap(false, false));
        assert!(
            t.starts_with(&labelled(
                "PANEL",
                "http://127.0.0.1:7880/ (tray → Open in browser)"
            )),
            "{}",
            &t[..120.min(t.len())]
        );
        assert!(
            t.contains(&labelled("CONFIG", "C:\\tm\\telemouse.toml")),
            "{t}"
        );
        assert!(t.contains(&labelled("LOGS", "C:\\tm\\logs")), "{t}");
        assert!(t.contains(&labelled("VERSION", "0.1.1")), "{t}");

        // A build with no logging says so rather than showing a path that
        // will never hold a file.
        let mut off = snap(false, false);
        off.places.logs.clear();
        assert!(
            render_text(&off).contains(&labelled("LOGS", "logging off")),
            "{}",
            render_text(&off)
        );
        // An empty snapshot renders without panicking and without lying.
        let empty = render_text(&Snapshot::default());
        assert!(empty.contains(&labelled("PANEL", "-")));
        assert!(empty.contains(&labelled("VERSION", "-")));
    }

    #[test]
    fn text_has_crlf_only_and_every_section() {
        let text = render_text(&snap(true, false));
        assert!(text.ends_with("\r\n"));
        assert!(
            !text.replace("\r\n", "").contains('\n'),
            "bare LF would vanish in an EDIT"
        );
        assert!(text.contains("COMPONENTS"));
        assert!(text.contains("Capture agent"));
        assert!(text.contains("running"));
        assert!(text.contains("pid 4242"));
        assert!(text.contains("up 1:01:01"));
        assert!(text.contains("not built"), "a missing binary is said so");
        assert!(text.contains(&labelled("SAVE DATA", "default on → recordings")));
        assert!(text.contains("capture is SAVING → recordings"));
        assert!(text.contains(&labelled("HOTKEY", "Ctrl+Alt+R → new session")));
        let mut nokey = snap(true, false);
        nokey.hotkey.clear();
        assert!(render_text(&nokey).contains(&labelled("HOTKEY", "none")));
        let mut off = snap(true, false);
        off.recording_enabled = false;
        off.components[0].saving = false;
        let t = render_text(&off);
        assert!(t.contains("default off"));
        assert!(t.contains("capture is NOT saving"));
        assert!(
            !render_text(&snap(false, false)).contains("capture is"),
            "no live note while stopped"
        );
        assert!(text.contains("RELATED PROCESSES"));
        assert!(text.contains("telemouse-ctl.exe"));
        assert!(text.contains("(this panel)"));
        assert!(text.contains("LOG — Capture agent (last 2 lines)"));
        assert!(text.contains("line two"));
        let empty = render_text(&Snapshot::default());
        assert!(empty.contains("(none)"));
        assert!(empty.contains("nothing has run yet"));
    }

    /// A chord another program already owns must not be presented as if it
    /// worked — neither in the window nor as a menu accelerator.
    #[test]
    fn an_unregistered_hotkey_says_so_and_loses_its_accelerator() {
        let mut s = snap(false, false);
        s.hotkey_registered = Some(false);
        let t = render_text(&s);
        assert!(
            t.contains(&labelled(
                "HOTKEY",
                "Ctrl+Alt+R — NOT REGISTERED (another program holds it; change [ctl] hotkey)"
            )),
            "{t}"
        );
        assert!(
            menu(&s, true).iter().all(|e| match e {
                MenuEntry::Item(i) => !i.label.contains('\t'),
                _ => true,
            }),
            "a dead chord must not be shown as an accelerator"
        );
        // Not yet attempted: neither claim is made.
        s.hotkey_registered = None;
        assert!(render_text(&s).contains(&labelled("HOTKEY", "Ctrl+Alt+R → new session")));
    }

    /// The exit column is fixed width; the explanation goes underneath it.
    #[test]
    fn a_long_hint_is_clipped_in_the_table_and_spelled_out_below() {
        let mut s = snap(false, false);
        s.components[1].last_exit = Some(
            ExitInfo::new(Some(1), 900)
                .with_hint(Some(crate::manager::HINT_PORT_IN_USE.into()))
                .with_last_line(Some("Error: failed to bind http 127.0.0.1:7879".into())),
        );
        let t = render_text(&s);
        let row = t
            .lines()
            .find(|l| l.starts_with("Viz server"))
            .expect("the viz row");
        assert!(
            row.contains(&clip(
                &format!("last: code 1 ({})", crate::manager::HINT_PORT_IN_USE),
                LAST_COL
            )),
            "the cell is clipped: {row:?}"
        );
        assert!(row.contains('…'), "something was clipped: {row:?}");
        assert!(
            row.ends_with("summary"),
            "the summary column still lines up: {row:?}"
        );
        assert!(t.contains(crate::manager::HINT_PORT_IN_USE), "{t}");
        assert!(
            t.contains("  ! Viz server last line: Error: failed to bind http 127.0.0.1:7879"),
            "{t}"
        );
        // A clean exit gets no note.
        s.components[1].last_exit = Some(ExitInfo::new(Some(0), 900));
        assert!(!render_text(&s).contains("  ! Viz server"));
    }

    #[test]
    fn menu_offers_start_or_stop_never_both() {
        let ids = |entries: &[MenuEntry]| -> Vec<(u16, bool)> {
            entries
                .iter()
                .filter_map(|e| match e {
                    MenuEntry::Item(i) => Some((i.id, i.enabled)),
                    MenuEntry::Separator => None,
                })
                .collect()
        };
        let m = ids(&menu(&snap(true, false), true));
        assert!(m.contains(&(MENU_STOP_CAPTURE, true)));
        assert!(
            !m.iter()
                .any(|(id, _)| *id == MENU_START_CAPTURE || *id == MENU_START_CAPTURE_NOSAVE)
        );
        let labels: Vec<String> = menu(&snap(true, false), true)
            .into_iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i.label),
                _ => None,
            })
            .collect();
        assert!(
            labels.iter().any(|l| l == "Stop capture (saving data)"),
            "{labels:?}"
        );
        // While running, New session is offered and wears the hotkey.
        assert!(m.contains(&(MENU_NEW_SESSION, true)));
        assert!(
            labels
                .iter()
                .any(|l| l == "New session (restart capture, save data → recordings)\tCtrl+Alt+R"),
            "{labels:?}"
        );
        let both = ids(&menu(&snap(false, false), true));
        assert!(both.contains(&(MENU_START_CAPTURE, true)));
        assert!(both.contains(&(MENU_START_CAPTURE_NOSAVE, true)));
        assert!(!both.iter().any(|(id, _)| *id == MENU_NEW_SESSION));
        // Stopped, the hotkey is a saving start, so that item wears it.
        let stopped_labels: Vec<String> = menu(&snap(false, false), true)
            .into_iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i.label),
                _ => None,
            })
            .collect();
        assert!(
            stopped_labels
                .iter()
                .any(|l| l == "Start capture (save data → recordings)\tCtrl+Alt+R"),
            "{stopped_labels:?}"
        );
        let mut nokey = snap(false, false);
        nokey.hotkey.clear();
        assert!(
            menu(&nokey, true).iter().all(|e| match e {
                MenuEntry::Item(i) => !i.label.contains('\t'),
                _ => true,
            }),
            "no accelerator text without a hotkey"
        );
        assert!(m.contains(&(MENU_START_VIZ, true)));
        assert!(m.contains(&(MENU_OPEN_PANEL, true)));
        assert!(m.contains(&(MENU_EXIT, true)));
        assert!(m.contains(&(MENU_TOGGLE_WINDOW, true)));

        let mut s = snap(false, false);
        s.components[0].bin_found = false;
        let m = ids(&menu(&s, false));
        assert!(
            m.contains(&(MENU_START_CAPTURE, false)),
            "missing binary greys start"
        );
        assert!(m.contains(&(MENU_START_CAPTURE_NOSAVE, false)));
        assert!(m.contains(&(MENU_START_VIZ, true)));
        assert!(ids(&menu(&Snapshot::default(), false)).contains(&(MENU_START_CAPTURE, false)));

        let label = |vis: bool| match &menu(&snap(false, false), vis)[0] {
            MenuEntry::Item(i) => i.label.clone(),
            _ => panic!(),
        };
        assert_eq!(label(true), "Hide window");
        assert_eq!(label(false), "Show window");
        let all: Vec<u16> = ids(&menu(&snap(false, false), true))
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        let mut dedup = all.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(all.len(), dedup.len(), "menu ids are unique");
    }

    /// The four *Open …* items and what each of them opens.
    #[test]
    fn the_menu_opens_every_place_the_panel_writes_to() {
        let s = snap(false, false);
        let enabled = |id: u16| {
            menu(&s, true).iter().any(|e| match e {
                MenuEntry::Item(i) => i.id == id && i.enabled,
                _ => false,
            })
        };
        for id in [
            MENU_OPEN_LOGS,
            MENU_OPEN_RECORDINGS,
            MENU_EDIT_CONFIG,
            MENU_OPEN_DOCS,
        ] {
            assert!(enabled(id), "{id} should be offered");
        }
        assert_eq!(menu_target(&s, MENU_OPEN_LOGS), Some("C:\\tm\\logs"));
        assert_eq!(menu_target(&s, MENU_OPEN_RECORDINGS), Some("recordings"));
        assert_eq!(
            menu_target(&s, MENU_EDIT_CONFIG),
            Some("C:\\tm\\telemouse.toml")
        );
        assert_eq!(menu_target(&s, MENU_OPEN_DOCS), Some("C:\\tm\\docs"));
        assert_eq!(
            menu_target(&s, MENU_OPEN_PANEL),
            Some("http://127.0.0.1:7880/")
        );
        assert_eq!(menu_target(&s, MENU_EXIT), None);

        // A build that writes no logs does not offer to open the folder.
        let mut nolog = s.clone();
        nolog.places.logs.clear();
        assert!(menu(&nolog, true).iter().any(|e| match e {
            MenuEntry::Item(i) => i.id == MENU_OPEN_LOGS && !i.enabled,
            _ => false,
        }));
        assert_eq!(menu_target(&nolog, MENU_OPEN_LOGS), None);
    }

    #[test]
    fn panel_url_replaces_an_unspecified_bind_address() {
        assert_eq!(
            panel_url("127.0.0.1:7880".parse().unwrap()),
            "http://127.0.0.1:7880/"
        );
        assert_eq!(
            panel_url("0.0.0.0:7880".parse().unwrap()),
            "http://127.0.0.1:7880/"
        );
        assert_eq!(
            panel_url("[::]:9000".parse().unwrap()),
            "http://127.0.0.1:9000/"
        );
        assert_eq!(
            panel_url("192.168.1.5:7880".parse().unwrap()),
            "http://192.168.1.5:7880/"
        );
    }

    #[test]
    fn icon_is_a_disc_with_transparent_corners() {
        for state in [
            IconState::Idle,
            IconState::CaptureRunning,
            IconState::Degraded,
        ] {
            for size in [16u32, 32] {
                let px = icon_bitmap(state, size);
                let n = size as usize;
                assert_eq!(px.bgra.len(), n * n);
                assert_eq!(px.mask.len(), IconPixels::mask_stride(size) * n);
                let centre = px.bgra[(n / 2) * n + n / 2];
                assert_eq!(centre >> 24, 0xFF, "centre is opaque");
                assert_eq!(px.bgra[0], 0, "corner is transparent");
                assert_eq!(px.mask[0] & 0x80, 0x80, "corner is masked out");
                assert_eq!(
                    px.mask[(n / 2) * IconPixels::mask_stride(size) + (n / 2) / 8]
                        & (0x80 >> ((n / 2) % 8)),
                    0
                );
                // The ring is darker than the fill.
                // Second pixel of the middle row: inside the disc, on the ring.
                let edge = px.bgra[(n / 2) * n + 1];
                assert_ne!(edge, 0);
                assert_ne!(edge, centre);
            }
        }
        assert_eq!(IconPixels::mask_stride(16), 2);
        assert_eq!(IconPixels::mask_stride(20), 4);
        // Every state is visibly a different colour.
        let mut seen: Vec<u32> = [
            IconState::Idle,
            IconState::CaptureRunning,
            IconState::Degraded,
        ]
        .iter()
        .map(|s| icon_bitmap(*s, 16).bgra[8 * 16 + 8])
        .collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), IconState::COUNT);
    }
}
