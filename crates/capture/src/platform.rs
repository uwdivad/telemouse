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
    use windows::Win32::Foundation::{CloseHandle, LPARAM, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, HDC, HMONITOR,
        MONITORINFO, MONITORINFOEXW, GetMonitorInfoW,
    };
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, OpenProcess, PROCESS_NAME_WIN32,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, ProcessPowerThrottling,
        QueryFullProcessImageNameW, SetProcessInformation, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_CXSCREEN,
        SM_CYSCREEN,
    };
    use windows::core::PWSTR;

    const MONITORINFOF_PRIMARY: u32 = 1;

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

    /// Executable name of `pid`, lowercased. Read-only and handle-free: the
    /// process handle is opened with the minimum right and closed immediately.
    /// The caller (see [`super::ForegroundCache`]) only calls this when the
    /// foreground PID actually changed.
    pub fn process_name(pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if !ok {
                return None;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            super::basename_lower(&path)
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
            GetMonitorInfoW(hmonitor, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO)
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

pub fn monitors() -> Vec<MonitorInfo> {
    imp::monitors()
}

/// Remembers the foreground PID→name mapping so alt-tabbing costs one process
/// handle, not four a second.
///
/// The plan's "no handles into the game process" spirit: while a game holds the
/// foreground, this opens nothing at all.
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
        assert_eq!(basename_lower("notepad.exe").as_deref(), Some("notepad.exe"));
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
    fn windows_reports_a_primary_screen_and_at_least_one_monitor() {
        let (w, h) = primary_screen();
        assert!(w > 0 && h > 0, "primary screen {w}x{h}");
        let mons = monitors();
        assert!(!mons.is_empty());
        assert!(mons.iter().any(|m| m.primary));
    }
}
