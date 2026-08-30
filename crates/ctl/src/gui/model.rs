//! What the window and the tray show, decided without Win32: the text of
//! the status window, the tooltip, which icon to wear, the popup menu and
//! its enabled items, and the icon pixels themselves. Everything here is a
//! pure function of a [`Snapshot`], so it is unit-tested on any OS and the
//! platform glue in `win.rs` only has to paint.

use std::net::SocketAddr;

use crate::gui::feed::Snapshot;
use crate::manager::{ComponentState, Kind, describe_exit, recording_flags};

/// `NOTIFYICONDATAW.szTip` is 128 UTF-16 units including the terminator.
pub const TOOLTIP_MAX_CHARS: usize = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// Nothing is being captured.
    Idle,
    /// The capture agent is running: the icon goes green.
    CaptureRunning,
}

impl IconState {
    /// Index into the icon table `win.rs` keeps.
    pub fn index(self) -> usize {
        match self {
            Self::Idle => 0,
            Self::CaptureRunning => 1,
        }
    }
}

fn component<'a>(s: &'a Snapshot, id: &str) -> Option<&'a ComponentState> {
    s.components.iter().find(|c| c.id == id)
}

pub fn icon_state(s: &Snapshot) -> IconState {
    if component(s, "capture").is_some_and(|c| c.running) {
        IconState::CaptureRunning
    } else {
        IconState::Idle
    }
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

/// Tray tooltip, never longer than [`TOOLTIP_MAX_CHARS`].
pub fn tooltip(s: &Snapshot) -> String {
    let cap = component(s, "capture");
    let text = format!(
        "telemouse-ctl — capture: {}{}, viz: {}",
        service_status(cap, s.now_unix_s),
        saving_note(cap),
        service_status(component(s, "viz"), s.now_unix_s),
    );
    text.chars().take(TOOLTIP_MAX_CHARS).collect()
}

/// Flags for a tray-started capture run that should (not) save data.
pub fn start_flags(s: &Snapshot, save: bool) -> Vec<String> {
    recording_flags(s.recording_enabled, save)
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
            let t = c.since_unix_s.or(c.last_exit.map(|e| e.at_unix_s))?;
            Some((t, c))
        })
        .max_by_key(|(t, _)| *t)
        .map(|(_, c)| c)
}

fn hms_utc(unix_s: u64) -> String {
    format!("{:02}:{:02}:{:02}", (unix_s / 3600) % 24, (unix_s / 60) % 60, unix_s % 60)
}

/// The whole body of the status window. CRLF line endings: a Win32 `EDIT`
/// control silently drops bare `\n`. Fixed-width columns for a monospace
/// font.
pub fn render_text(s: &Snapshot) -> String {
    let now = s.now_unix_s;
    let mut out: Vec<String> = Vec::with_capacity(64);
    out.push(format!("COMPONENTS                                              refreshed {} UTC", hms_utc(now)));
    for c in &s.components {
        let state = if c.running {
            "running"
        } else if c.bin_found {
            "stopped"
        } else {
            "not built"
        };
        let pid = c.pid.map(|p| format!("pid {p}")).unwrap_or_else(|| "-".into());
        let up = match (c.running, c.since_unix_s) {
            (true, Some(since)) => format!("up {}", format_uptime(now, since)),
            _ => "-".into(),
        };
        let last = c
            .last_exit
            .map(|e| format!("last: {}", describe_exit(e)))
            .unwrap_or_else(|| "last: -".into());
        let kind = match c.kind {
            Kind::Service => "service",
            Kind::Task => "task",
        };
        out.push(format!("{:<17} {:<8} {:<10} {:<11} {:<12} {:<18} {}", c.label, kind, state, pid, up, last, c.summary));
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
    out.push(format!("SAVE DATA           default {default} (telemouse.toml [recording] enabled){now_line}"));
    out.push(String::new());
    out.push("RELATED PROCESSES".into());
    if s.processes.is_empty() {
        out.push("  (none)".into());
    } else {
        out.push(format!("{:>7}  {:<8} {:<24} {:>6} {:>9}", "PID", "KIND", "NAME", "CPU%", "MEM MB"));
        for p in &s.processes {
            let kind = format!("{:?}", p.kind).to_ascii_lowercase();
            let me = if p.is_self { "  (this panel)" } else { "" };
            out.push(format!("{:>7}  {:<8} {:<24} {:>6.1} {:>9.1}{}", p.pid, kind, p.name, p.cpu_pct, p.mem_mb, me));
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

pub const MENU_TOGGLE_WINDOW: u16 = 1001;
pub const MENU_START_CAPTURE: u16 = 1002;
pub const MENU_STOP_CAPTURE: u16 = 1003;
pub const MENU_START_VIZ: u16 = 1004;
pub const MENU_STOP_VIZ: u16 = 1005;
pub const MENU_OPEN_PANEL: u16 = 1006;
pub const MENU_EXIT: u16 = 1007;
pub const MENU_START_CAPTURE_NOSAVE: u16 = 1008;

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
    MenuEntry::Item(MenuItem { id, label: label.into(), enabled })
}

/// Start is offered when the binary exists and nothing is running; stop when
/// it is running. One of the two per service, never both.
fn service_items(c: Option<&ComponentState>, label: &str, start_id: u16, stop_id: u16) -> MenuEntry {
    match c {
        Some(c) if c.running => item(stop_id, &format!("Stop {label}"), true),
        Some(c) => item(start_id, &format!("Start {label}"), c.bin_found),
        None => item(start_id, &format!("Start {label}"), false),
    }
}

/// The capture entries: while running, one *Stop* that says whether data is
/// being saved; otherwise two *Start*s — with and without saving — both
/// greyed when the binary is missing.
fn capture_items(s: &Snapshot) -> Vec<MenuEntry> {
    match component(s, "capture") {
        Some(c) if c.running => vec![item(
            MENU_STOP_CAPTURE,
            if c.saving { "Stop capture (saving data)" } else { "Stop capture (not saving)" },
            true,
        )],
        c => {
            let ok = c.is_some_and(|c| c.bin_found);
            vec![
                item(MENU_START_CAPTURE, &format!("Start capture (save data → {})", s.recording_dir), ok),
                item(MENU_START_CAPTURE_NOSAVE, "Start capture (don't save)", ok),
            ]
        }
    }
}

pub fn menu(s: &Snapshot, window_visible: bool) -> Vec<MenuEntry> {
    let mut v = vec![
        item(MENU_TOGGLE_WINDOW, if window_visible { "Hide window" } else { "Show window" }, true),
        MenuEntry::Separator,
    ];
    v.extend(capture_items(s));
    v.extend([
        service_items(component(s, "viz"), "viz server", MENU_START_VIZ, MENU_STOP_VIZ),
        MenuEntry::Separator,
        item(MENU_OPEN_PANEL, "Open web panel", true),
        MenuEntry::Separator,
        item(MENU_EXIT, "Exit (stops what the panel started)", true),
    ]);
    v
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
/// capture agent runs.
pub fn icon_bitmap(state: IconState, size: u32) -> IconPixels {
    let (fill, ring) = match state {
        IconState::Idle => (0xFF80_8080u32, 0xFF50_5050u32),
        IconState::CaptureRunning => (0xFF2E_A043u32, 0xFF1F_6F2Eu32),
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
    use crate::procs::{ProcInfo, ProcKind};

    fn comp(id: &'static str, label: &'static str, kind: Kind, running: bool, bin_found: bool) -> ComponentState {
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
            running,
            pid: running.then_some(4242),
            since_unix_s: running.then_some(1_000_000),
            last_exit: None,
            exits: 0,
            unexpected_exits: 0,
            args: Vec::new(),
            saving: running && id == "capture",
            log: vec!["line one".into(), "line two".into()],
        }
    }

    fn snap(capture_running: bool, viz_running: bool) -> Snapshot {
        Snapshot {
            recording_enabled: true,
            recording_dir: "recordings".into(),
            now_unix_s: 1_000_000 + 3661,
            components: vec![
                comp("capture", "Capture agent", Kind::Service, capture_running, true),
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
        assert_ne!(IconState::Idle.index(), IconState::CaptureRunning.index());
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
        assert_eq!(t, "telemouse-ctl — capture: running 1:01:01 (saving), viz: stopped");
        let mut nosave = snap(true, false);
        nosave.components[0].saving = false;
        assert!(tooltip(&nosave).contains("(not saving)"));
        assert!(!tooltip(&snap(false, false)).contains("saving"), "no note while stopped");
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

    #[test]
    fn focus_prefers_running_services_then_recency() {
        let s = snap(true, true);
        assert_eq!(focus_component(&s.components).unwrap().id, "capture");
        let s = snap(false, true);
        assert_eq!(focus_component(&s.components).unwrap().id, "viz");
        let mut s = snap(false, false);
        assert!(focus_component(&s.components).is_none(), "nothing ever ran");
        s.components[2].last_exit = Some(ExitInfo { code: Some(0), at_unix_s: 500 });
        s.components[1].last_exit = Some(ExitInfo { code: Some(1), at_unix_s: 900 });
        assert_eq!(focus_component(&s.components).unwrap().id, "viz");
    }

    #[test]
    fn text_has_crlf_only_and_every_section() {
        let text = render_text(&snap(true, false));
        assert!(text.ends_with("\r\n"));
        assert!(!text.replace("\r\n", "").contains('\n'), "bare LF would vanish in an EDIT");
        assert!(text.contains("COMPONENTS"));
        assert!(text.contains("Capture agent"));
        assert!(text.contains("running"));
        assert!(text.contains("pid 4242"));
        assert!(text.contains("up 1:01:01"));
        assert!(text.contains("not built"), "a missing binary is said so");
        assert!(text.contains("SAVE DATA           default on → recordings"));
        assert!(text.contains("capture is SAVING → recordings"));
        let mut off = snap(true, false);
        off.recording_enabled = false;
        off.components[0].saving = false;
        let t = render_text(&off);
        assert!(t.contains("default off"));
        assert!(t.contains("capture is NOT saving"));
        assert!(!render_text(&snap(false, false)).contains("capture is"), "no live note while stopped");
        assert!(text.contains("RELATED PROCESSES"));
        assert!(text.contains("telemouse-ctl.exe"));
        assert!(text.contains("(this panel)"));
        assert!(text.contains("LOG — Capture agent (last 2 lines)"));
        assert!(text.contains("line two"));
        let empty = render_text(&Snapshot::default());
        assert!(empty.contains("(none)"));
        assert!(empty.contains("nothing has run yet"));
    }

    #[test]
    fn start_flags_only_override_a_disagreeing_default() {
        let on = snap(false, false);
        assert!(start_flags(&on, true).is_empty());
        assert_eq!(start_flags(&on, false), vec!["--no-record".to_string()]);
        let mut off = snap(false, false);
        off.recording_enabled = false;
        assert_eq!(start_flags(&off, true), vec!["--record".to_string()]);
        assert!(start_flags(&off, false).is_empty());
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
        assert!(!m.iter().any(|(id, _)| *id == MENU_START_CAPTURE || *id == MENU_START_CAPTURE_NOSAVE));
        let labels: Vec<String> = menu(&snap(true, false), true)
            .into_iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i.label),
                _ => None,
            })
            .collect();
        assert!(labels.iter().any(|l| l == "Stop capture (saving data)"), "{labels:?}");
        let both = ids(&menu(&snap(false, false), true));
        assert!(both.contains(&(MENU_START_CAPTURE, true)));
        assert!(both.contains(&(MENU_START_CAPTURE_NOSAVE, true)));
        assert!(m.contains(&(MENU_START_VIZ, true)));
        assert!(m.contains(&(MENU_OPEN_PANEL, true)));
        assert!(m.contains(&(MENU_EXIT, true)));
        assert!(m.contains(&(MENU_TOGGLE_WINDOW, true)));

        let mut s = snap(false, false);
        s.components[0].bin_found = false;
        let m = ids(&menu(&s, false));
        assert!(m.contains(&(MENU_START_CAPTURE, false)), "missing binary greys start");
        assert!(m.contains(&(MENU_START_CAPTURE_NOSAVE, false)));
        assert!(m.contains(&(MENU_START_VIZ, true)));
        assert!(ids(&menu(&Snapshot::default(), false)).contains(&(MENU_START_CAPTURE, false)));

        let label = |vis: bool| match &menu(&snap(false, false), vis)[0] {
            MenuEntry::Item(i) => i.label.clone(),
            _ => panic!(),
        };
        assert_eq!(label(true), "Hide window");
        assert_eq!(label(false), "Show window");
        let all: Vec<u16> = ids(&menu(&snap(false, false), true)).into_iter().map(|(i, _)| i).collect();
        let mut dedup = all.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(all.len(), dedup.len(), "menu ids are unique");
    }

    #[test]
    fn panel_url_replaces_an_unspecified_bind_address() {
        assert_eq!(panel_url("127.0.0.1:7880".parse().unwrap()), "http://127.0.0.1:7880/");
        assert_eq!(panel_url("0.0.0.0:7880".parse().unwrap()), "http://127.0.0.1:7880/");
        assert_eq!(panel_url("[::]:9000".parse().unwrap()), "http://127.0.0.1:9000/");
        assert_eq!(panel_url("192.168.1.5:7880".parse().unwrap()), "http://192.168.1.5:7880/");
    }

    #[test]
    fn icon_is_a_disc_with_transparent_corners() {
        for state in [IconState::Idle, IconState::CaptureRunning] {
            for size in [16u32, 32] {
                let px = icon_bitmap(state, size);
                let n = size as usize;
                assert_eq!(px.bgra.len(), n * n);
                assert_eq!(px.mask.len(), IconPixels::mask_stride(size) * n);
                let centre = px.bgra[(n / 2) * n + n / 2];
                assert_eq!(centre >> 24, 0xFF, "centre is opaque");
                assert_eq!(px.bgra[0], 0, "corner is transparent");
                assert_eq!(px.mask[0] & 0x80, 0x80, "corner is masked out");
                assert_eq!(px.mask[(n / 2) * IconPixels::mask_stride(size) + (n / 2) / 8] & (0x80 >> ((n / 2) % 8)), 0);
                // The ring is darker than the fill.
                // Second pixel of the middle row: inside the disc, on the ring.
                let edge = px.bgra[(n / 2) * n + 1];
                assert_ne!(edge, 0);
                assert_ne!(edge, centre);
            }
        }
        assert_eq!(IconPixels::mask_stride(16), 2);
        assert_eq!(IconPixels::mask_stride(20), 4);
        assert_ne!(icon_bitmap(IconState::Idle, 16).bgra, icon_bitmap(IconState::CaptureRunning, 16).bgra);
    }
}
