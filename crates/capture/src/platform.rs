//! The thin platform layer: clock, cursor, screens, foreground process.
//!
//! Win32 stays behind this module so every consumer of it (the context thread,
//! the shipping thread, `doctor`) is platform-neutral and testable. The
//! non-Windows fallbacks exist so `cargo test` builds anywhere; actual capture
//! is Windows-only.

use telemouse_core::session::MonitorInfo;

/// Lowercase file name of an executable path, e.g. `C:\...\cs2.exe` → `cs2.exe`.
/// Pure, and it handles both separator styles because Win32 returns either.
pub fn basename_lower(path: &str) -> Option<String> {
    let name = path.rsplit(['\\', '/']).next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_ascii_lowercase())
    }
}

#[cfg(windows)]
mod imp {
    use telemouse_core::session::MonitorInfo;
    use windows::Win32::Foundation::{CloseHandle, HMODULE, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW,
        GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows::Win32::System::Console::GetConsoleProcessList;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL,
    };
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_CXSCREEN,
        SM_CYSCREEN,
    };
    use windows::core::{s, w};

    const MONITORINFOF_PRIMARY: u32 = 1;

    /// Tell Windows this process reads real pixels.
    ///
    /// Without it a display-scaled desktop hands back *virtualised*
    /// coordinates: `SM_CXSCREEN` on a 150%-scaled 2560×1440 monitor reports
    /// 1707×960, and so do the cursor position and the monitor rectangles.
    /// Every one of those numbers ends up in the session record and in every
    /// batch, where a consumer converting counts to screen space would be
    /// quietly wrong by the scaling factor. Failure is ignored: it means the
    /// awareness was already set (by a manifest, or by a second call), which
    /// is the outcome we wanted anyway.
    pub fn set_dpi_awareness() {
        // SAFETY: no arguments beyond a well-known constant; the call only
        // ever changes this process's own DPI mode.
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    }

    /// True when this process is the only one attached to its console — i.e.
    /// the window was opened *for* it, by Explorer or a shortcut, and closes
    /// with it. Started from a shell, the shell is attached too.
    pub fn owns_console() -> bool {
        let mut pids = [0u32; 4];
        // SAFETY: a plain array; the call reports how many processes fit.
        unsafe { GetConsoleProcessList(&mut pids) == 1 }
    }

    /// `OSVERSIONINFOW`, declared here rather than pulled from a metadata
    /// feature: the one function that reports the truth on Windows 10+ lives
    /// in ntdll and is resolved by name below.
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform_id: u32,
        csd_version: [u16; 128],
    }

    /// `Windows <major>.<minor>.<build>`, e.g. `Windows 10.0.19045`.
    ///
    /// `GetVersionExW` has lied since Windows 8.1 unless the executable
    /// carries a compatibility manifest — it reports 6.2 for everything —
    /// so the build number, the only part that identifies what the machine
    /// actually is, comes from `RtlGetVersion`, which is not manifest-gated.
    pub fn os_version() -> Option<String> {
        type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfoW) -> i32;
        // SAFETY: ntdll is mapped into every Win32 process, `RtlGetVersion`
        // has the signature above, and the struct is the documented layout
        // with its size filled in.
        unsafe {
            let ntdll: HMODULE = GetModuleHandleW(w!("ntdll.dll")).ok()?;
            let proc = GetProcAddress(ntdll, s!("RtlGetVersion"))?;
            let rtl_get_version =
                std::mem::transmute::<unsafe extern "system" fn() -> isize, RtlGetVersion>(proc);
            let mut info: OsVersionInfoW = std::mem::zeroed();
            info.size = size_of::<OsVersionInfoW>() as u32;
            if rtl_get_version(&mut info) != 0 {
                return None;
            }
            Some(format!(
                "Windows {}.{}.{}",
                info.major, info.minor, info.build
            ))
        }
    }

    pub fn qpc() -> u64 {
        let mut v = 0i64;
        // Documented never to fail on Windows XP or later.
        unsafe { QueryPerformanceCounter(&mut v) }.ok();
        v as u64
    }

    pub fn qpc_freq() -> u64 {
        let mut v = 0i64;
        unsafe { QueryPerformanceFrequency(&mut v) }.ok();
        if v <= 0 { 10_000_000 } else { v as u64 }
    }

    pub fn primary_screen() -> (u32, u32) {
        unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN).max(0) as u32,
                GetSystemMetrics(SM_CYSCREEN).max(0) as u32,
            )
        }
    }

    pub fn cursor_pos() -> Option<(i32, i32)> {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) }.ok()?;
        Some((p.x, p.y))
    }

    /// PID owning the foreground window, or 0. Costs no handle at all.
    pub fn foreground_pid() -> u32 {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() {
                return 0;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            pid
        }
    }

    /// Executable name of `pid`, lowercased, read from a snapshot of the
    /// process table.
    ///
    /// No handle to `pid` is ever opened: the snapshot is a kernel-built list
    /// of names and ids, the same thing Task Manager shows, so the agent
    /// never holds a handle on a game — the one thing an anti-cheat driver
    /// could have logged about it — and an elevated game, which would refuse
    /// `OpenProcess` from here, is named just the same. The walk costs a few
    /// milliseconds; the caller (see [`super::ForegroundCache`]) only asks
    /// when the foreground PID actually changed.
    pub fn process_name(pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        // SAFETY: Toolhelp snapshot iteration with a correctly sized entry;
        // the snapshot handle is closed on every path, and nothing is read
        // from any other process.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut name = None;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    if entry.th32ProcessID == pid {
                        let end = entry
                            .szExeFile
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(entry.szExeFile.len());
                        name = Some(String::from_utf16_lossy(&entry.szExeFile[..end]));
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            super::basename_lower(&name?)
        }
    }

    /// Nudge the calling thread (T1 only) above normal so a busy desktop cannot
    /// delay `WM_INPUT` delivery. Failure is a warning, never fatal.
    pub fn raise_capture_thread_priority() {
        let r = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
        match r {
            Ok(()) => tracing::info!(priority = "above_normal", "capture thread priority set"),
            Err(e) => tracing::warn!(error = %e, "could not raise capture thread priority"),
        }
    }

    /// Opt the whole process out of EcoQoS / power throttling, so Windows does
    /// not park us onto a throttled E-core while a game owns the P-cores.
    pub fn disable_power_throttling() {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            // Control the knob, and set it to "not throttled".
            StateMask: 0,
        };
        let r = unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const PROCESS_POWER_THROTTLING_STATE as *const std::ffi::c_void,
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
        match r {
            Ok(()) => tracing::info!(qos = "high_performance", "power throttling disabled"),
            // Older Windows 10 builds do not know this information class.
            Err(e) => tracing::warn!(error = %e, "could not opt out of power throttling"),
        }
    }

    unsafe extern "system" fn enum_monitor(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> windows::core::BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let ok = unsafe {
            GetMonitorInfoW(
                hmonitor,
                &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
            )
        };
        if ok.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            out.push(MonitorInfo {
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                refresh_hz: refresh_for_device(&info.szDevice),
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        true.into()
    }

    fn refresh_for_device(device: &[u16; 32]) -> Option<u32> {
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let ok = unsafe {
            EnumDisplaySettingsW(
                windows::core::PCWSTR(device.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
        };
        if ok.as_bool() && mode.dmDisplayFrequency > 1 {
            Some(mode.dmDisplayFrequency)
        } else {
            None
        }
    }

    pub fn monitors() -> Vec<MonitorInfo> {
        let mut out: Vec<MonitorInfo> = Vec::new();
        let ok = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(enum_monitor),
                LPARAM(&mut out as *mut Vec<MonitorInfo> as isize),
            )
        };
        if !ok.as_bool() || out.is_empty() {
            // Degrade to the primary screen rather than shipping nothing.
            let (w, h) = primary_screen();
            out.push(MonitorInfo {
                width: w,
                height: h,
                refresh_hz: None,
                primary: true,
            });
        }
        out
    }
}

#[cfg(not(windows))]
mod imp {
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use telemouse_core::session::MonitorInfo;

    fn origin() -> Instant {
        use std::sync::OnceLock;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    /// Nanosecond monotonic clock stands in for QPC off Windows.
    pub fn qpc() -> u64 {
        let _ = UNIX_EPOCH;
        let _ = SystemTime::now();
        origin().elapsed().as_nanos() as u64
    }

    pub fn qpc_freq() -> u64 {
        1_000_000_000
    }

    pub fn primary_screen() -> (u32, u32) {
        (0, 0)
    }

    pub fn cursor_pos() -> Option<(i32, i32)> {
        None
    }

    pub fn foreground_pid() -> u32 {
        0
    }

    pub fn process_name(_pid: u32) -> Option<String> {
        None
    }

    pub fn raise_capture_thread_priority() {}

    pub fn disable_power_throttling() {}

    pub fn set_dpi_awareness() {}

    pub fn owns_console() -> bool {
        false
    }

    pub fn os_version() -> Option<String> {
        None
    }

    pub fn monitors() -> Vec<MonitorInfo> {
        Vec::new()
    }
}

/// `QueryPerformanceCounter`, or a monotonic nanosecond clock off Windows.
pub fn qpc() -> u64 {
    imp::qpc()
}

/// `QueryPerformanceFrequency` in ticks per second.
pub fn qpc_freq() -> u64 {
    imp::qpc_freq()
}

/// Primary screen size in pixels (`SM_CXSCREEN`/`SM_CYSCREEN`).
pub fn primary_screen() -> (u32, u32) {
    imp::primary_screen()
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    imp::cursor_pos()
}

/// Lowercase foreground process name, e.g. `cs2.exe`. Uncached — `doctor` and
/// tests only; the context thread uses [`ForegroundCache`].
pub fn foreground_process_name() -> Option<String> {
    imp::process_name(imp::foreground_pid())
}

/// `THREAD_PRIORITY_ABOVE_NORMAL` on the calling thread. No-op off Windows.
pub fn raise_capture_thread_priority() {
    imp::raise_capture_thread_priority();
}

/// Opt the process out of EcoQoS power throttling. No-op off Windows.
pub fn disable_power_throttling() {
    imp::disable_power_throttling();
}

/// Declare per-monitor DPI awareness, so screen, monitor and cursor geometry
/// are real pixels rather than scaled ones. No-op off Windows.
pub fn set_dpi_awareness() {
    imp::set_dpi_awareness();
}

/// True when this process is the only one attached to its console, i.e. the
/// window will vanish with it. Always false off Windows.
pub fn owns_console() -> bool {
    imp::owns_console()
}

/// The host OS as it goes into the session record, e.g.
/// `Windows 10.0.19045`. `None` when it cannot be determined.
pub fn os_version() -> Option<String> {
    imp::os_version()
}

pub fn monitors() -> Vec<MonitorInfo> {
    imp::monitors()
}

/// Remembers the foreground PID→name mapping so alt-tabbing costs one
/// process-table snapshot, not four a second.
///
/// The plan's "no handles into the game process" rule, kept literally: the
/// name comes from a Toolhelp snapshot, which opens no handle on any process,
/// and while a game holds the foreground nothing is asked at all.
#[derive(Debug, Default, Clone)]
pub struct ForegroundCache {
    pid: u32,
    name: Option<String>,
}

impl ForegroundCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current foreground process name, re-resolving only on a PID change.
    pub fn get(&mut self) -> Option<String> {
        self.resolve(imp::foreground_pid(), imp::process_name)
    }

    /// The pure half: `resolve` is only invoked when `pid` differs from the
    /// cached one, which is what makes the caching testable without Win32.
    pub fn resolve(
        &mut self,
        pid: u32,
        resolve: impl FnOnce(u32) -> Option<String>,
    ) -> Option<String> {
        if pid != self.pid {
            self.pid = pid;
            self.name = if pid == 0 { None } else { resolve(pid) };
        }
        self.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_lowercases_and_strips_both_separators() {
        assert_eq!(
            basename_lower(r"C:\Program Files\CS2\CS2.EXE").as_deref(),
            Some("cs2.exe")
        );
        assert_eq!(basename_lower("/usr/bin/Foo").as_deref(), Some("foo"));
        assert_eq!(
            basename_lower("notepad.exe").as_deref(),
            Some("notepad.exe")
        );
        assert_eq!(basename_lower(r"C:\dir\"), None);
        assert_eq!(basename_lower(""), None);
    }

    #[test]
    fn qpc_is_monotonic_and_has_a_sane_frequency() {
        let freq = qpc_freq();
        assert!(freq >= 1_000_000, "suspicious qpc frequency {freq}");
        let a = qpc();
        let b = qpc();
        assert!(b >= a);
    }

    #[test]
    fn the_foreground_cache_only_resolves_on_a_pid_change() {
        use std::cell::Cell;

        let calls = Cell::new(0u32);
        let mut cache = ForegroundCache::new();
        // Captures only `&Cell`, so it is `Copy` and can be handed over again.
        let lookup = |pid: u32| -> Option<String> {
            calls.set(calls.get() + 1);
            Some(format!("proc-{pid}.exe"))
        };

        assert_eq!(cache.resolve(100, lookup).as_deref(), Some("proc-100.exe"));
        assert_eq!(calls.get(), 1);
        // Same PID four times a second: no further handles opened.
        for _ in 0..10 {
            assert_eq!(cache.resolve(100, lookup).as_deref(), Some("proc-100.exe"));
        }
        assert_eq!(calls.get(), 1);
        // Alt-tab: one new lookup.
        assert_eq!(cache.resolve(200, lookup).as_deref(), Some("proc-200.exe"));
        assert_eq!(calls.get(), 2);
        // ...and back again is a lookup too (we cache one PID, not a table).
        assert_eq!(cache.resolve(100, lookup).as_deref(), Some("proc-100.exe"));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn no_foreground_window_costs_nothing_and_reports_nothing() {
        let mut cache = ForegroundCache::new();
        let mut called = false;
        let name = cache.resolve(0, |_| {
            called = true;
            Some("nope".into())
        });
        assert_eq!(name, None);
        assert!(!called, "pid 0 must never open a handle");
    }

    #[test]
    fn an_unresolvable_pid_is_cached_as_unknown() {
        use std::cell::Cell;

        let mut cache = ForegroundCache::new();
        let calls = Cell::new(0u32);
        for _ in 0..3 {
            let n = cache.resolve(7, |_| {
                calls.set(calls.get() + 1);
                None
            });
            assert_eq!(n, None);
        }
        assert_eq!(
            calls.get(),
            1,
            "a failed lookup must not be retried every tick"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_itself_with_a_build_number() {
        let v = os_version().expect("RtlGetVersion is in every Win32 process");
        assert!(
            v.starts_with("Windows 10.") || v.starts_with("Windows 11."),
            "{v}"
        );
        // Three dot-separated numbers, the last one the build.
        let parts: Vec<&str> = v.trim_start_matches("Windows ").split('.').collect();
        assert_eq!(parts.len(), 3, "{v}");
        assert!(parts[2].parse::<u32>().unwrap() > 0, "{v}");
    }

    /// The process table names this very process without a handle being
    /// opened on it — the whole point of reading names that way.
    #[cfg(windows)]
    #[test]
    fn the_process_table_names_this_process_and_nobody_else() {
        let me = super::imp::process_name(std::process::id()).expect("own name");
        assert!(me.ends_with(".exe"), "{me}");
        assert_eq!(me, me.to_ascii_lowercase());
        assert!(me.contains("telemouse"), "{me}");
        assert_eq!(super::imp::process_name(u32::MAX - 7), None);
        assert_eq!(super::imp::process_name(0), None);
    }

    #[cfg(windows)]
    #[test]
    fn dpi_awareness_and_console_ownership_are_safe_to_ask_about() {
        // Both are best-effort facts about the process: they must never
        // panic, and calling twice must be as harmless as calling once.
        set_dpi_awareness();
        set_dpi_awareness();
        let _ = owns_console();
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_a_primary_screen_and_at_least_one_monitor() {
        let (w, h) = primary_screen();
        assert!(w > 0 && h > 0, "primary screen {w}x{h}");
        let mons = monitors();
        assert!(!mons.is_empty());
        assert!(mons.iter().any(|m| m.primary));
    }
}
