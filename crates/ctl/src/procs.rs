//! Discovery of telemouse-related processes, and the one thing the panel may
//! do to them: terminate.
//!
//! "Related" is decided by [`classify`], a pure function over the process
//! name and command line, so the vocabulary is unit-tested without a process
//! table. [`Scanner::kill`] re-checks the same predicate on the live process
//! immediately before terminating it, so the API can never be pointed at an
//! arbitrary PID — and never at this process.
//!
//! ## Why the scan is hand-rolled
//!
//! The first version walked the whole process table through `sysinfo`, which
//! on Windows opens a handle to *every* process and queries each one's times,
//! memory and command line — ~16 ms of kernel CPU per scan, and the single
//! largest CPU consumer of the whole telemouse stack while a page was polling
//! (0.45% of a core at a 4 s scan; 5% at 1.5 s). The table only ever shows a
//! handful of processes, so the work is split in two tiers:
//!
//! * **Enumeration** — one `Toolhelp32` process snapshot, names and PIDs
//!   only. Still ~7 ms of kernel time on a box with a few hundred processes
//!   (the kernel walks every process *and thread* to build it), so it runs
//!   at most every [`ENUMERATE_EVERY`], or immediately after something the
//!   panel did itself changed the table (start, stop, kill).
//! * **Details** — per-process queries (image path, CPU time, working set,
//!   command line) **only for the PIDs whose name matched**: ~20 µs each,
//!   refreshed on every scan so the CPU column stays live and a process that
//!   exited disappears at the next poll, not the next enumeration.
//!
//! Everything platform-specific lives in the private `sys` module; the policy
//! above it is platform-neutral.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

/// What a related process is, by its executable name (and, for build-tool
/// wrappers, its command line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcKind {
    /// `telemouse` — the capture agent.
    Capture,
    /// `telemouse-viz` — the live viz / replay server.
    Viz,
    /// `telemouse-analyze` — offline metrics.
    Analyze,
    /// `telemouse-ctl` — this panel (or another instance of it).
    Ctl,
    /// A `cargo run` / `cargo test` wrapper whose command line names telemouse.
    Cargo,
    /// Anything else whose executable name starts with `telemouse`.
    Other,
}

impl ProcKind {
    /// Sort weight: services first, tooling last.
    fn order(self) -> u8 {
        match self {
            Self::Capture => 0,
            Self::Viz => 1,
            Self::Analyze => 2,
            Self::Ctl => 3,
            Self::Other => 4,
            Self::Cargo => 5,
        }
    }
}

/// Classify a process by executable name and command line. `None` means
/// unrelated: the panel neither lists nor touches it.
pub fn classify(name: &str, cmd: &str) -> Option<ProcKind> {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    match stem {
        "telemouse" => Some(ProcKind::Capture),
        "telemouse-viz" => Some(ProcKind::Viz),
        "telemouse-analyze" => Some(ProcKind::Analyze),
        "telemouse-ctl" => Some(ProcKind::Ctl),
        "cargo" => cmd
            .to_ascii_lowercase()
            .contains("telemouse")
            .then_some(ProcKind::Cargo),
        _ if stem.starts_with("telemouse") => Some(ProcKind::Other),
        _ => None,
    }
}

/// Could this process be related, judging by its name alone? The cheap
/// pre-filter that decides which processes get the per-process queries: a
/// `cargo` needs its command line to be sure, everything else is decided by
/// the name.
pub fn name_is_candidate(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    stem == "cargo" || stem.starts_with("telemouse")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cmd: String,
    pub exe: Option<String>,
    pub kind: ProcKind,
    /// Since the previous scan; 0 on the first one.
    pub cpu_pct: f32,
    pub mem_mb: f64,
    pub started_unix_s: u64,
    /// This panel's own process — listed for honesty, never killable.
    pub is_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KillError {
    #[error("no such process")]
    NotFound,
    #[error("refusing to kill the control panel itself")]
    IsSelf,
    #[error("process is not a telemouse process")]
    NotRelated,
    /// The OS refused (access denied, or it exited in between).
    #[error("the OS refused to terminate the process")]
    Failed,
}

/// CPU time of one process at one scan, so the next scan can report a rate.
#[derive(Debug, Clone, Copy)]
struct CpuSample {
    at: Instant,
    cpu_100ns: u64,
}

/// Per-interval CPU percentage of one core from two samples. Pure. The first
/// sighting (no previous sample) is 0, like the table has always shown.
pub fn cpu_pct(prev: Option<(Duration, u64)>, cpu_100ns: u64) -> f32 {
    match prev {
        Some((elapsed, prev_cpu)) if !elapsed.is_zero() => {
            let d = cpu_100ns.saturating_sub(prev_cpu) as f64 / 1e7;
            (100.0 * d / elapsed.as_secs_f64()) as f32
        }
        _ => 0.0,
    }
}

/// How long an enumeration of the process table stays good for. Processes
/// the panel did not start itself are rare and long-lived; anything the
/// panel does start, stop or kill invalidates it immediately.
pub const ENUMERATE_EVERY: Duration = Duration::from_secs(30);

/// A process table scanner. Remembers each related process's CPU time
/// between scans so `cpu_pct` is measured over the interval since the
/// previous scan.
pub struct Scanner {
    self_pid: u32,
    cpu: Mutex<HashMap<u32, CpuSample>>,
    /// Candidates (by name) from the last enumeration, and when it ran.
    known: Mutex<Option<(Instant, Vec<Entry>)>>,
    /// The last rendered list and when it was taken, for [`Self::scan_cached`].
    last: Mutex<Option<(Instant, Vec<ProcInfo>)>>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            self_pid: std::process::id(),
            cpu: Mutex::new(HashMap::new()),
            known: Mutex::new(None),
            last: Mutex::new(None),
        }
    }

    pub fn self_pid(&self) -> u32 {
        self.self_pid
    }

    /// Forget both caches: the next scan enumerates the table afresh. Called
    /// after anything the panel did that changes the table.
    pub fn invalidate(&self) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.known.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// [`Self::scan`], but reuse a scan younger than `ttl`. A process list
    /// does not change at page-poll rate, and a kill invalidates the cache so
    /// the table never shows a process the panel just terminated.
    pub fn scan_cached(&self, ttl: Duration) -> Vec<ProcInfo> {
        {
            let last = self.last.lock().unwrap_or_else(|p| p.into_inner());
            if let Some((at, list)) = last.as_ref()
                && at.elapsed() < ttl
            {
                return list.clone();
            }
        }
        let fresh = self.scan();
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((Instant::now(), fresh.clone()));
        fresh
    }

    /// The name-matched candidates: re-enumerated when the last enumeration
    /// is older than [`ENUMERATE_EVERY`] (or was invalidated), otherwise the
    /// remembered list.
    fn candidates(&self) -> Vec<Entry> {
        let mut known = self.known.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, list)) = known.as_ref()
            && at.elapsed() < ENUMERATE_EVERY
        {
            return list.clone();
        }
        let fresh: Vec<Entry> = sys::enumerate()
            .into_iter()
            .filter(|e| name_is_candidate(&e.name))
            .collect();
        *known = Some((Instant::now(), fresh.clone()));
        fresh
    }

    /// Every related process, services first, then by PID.
    pub fn scan(&self) -> Vec<ProcInfo> {
        let now = Instant::now();
        let mut cpu = self.cpu.lock().unwrap_or_else(|p| p.into_inner());
        let mut seen: HashMap<u32, CpuSample> = HashMap::new();
        let mut out: Vec<ProcInfo> = self
            .candidates()
            .into_iter()
            .filter_map(|e| {
                // Gone since the enumeration: drop it now rather than at the
                // next one. A live process that refuses the handle (elevated)
                // still classifies by name; only a cargo wrapper genuinely
                // needs the command line.
                let d = match sys::details(e.pid) {
                    Some(d) if d.exited => return None,
                    Some(d) => d,
                    None if sys::exists(e.pid) => Details::default(),
                    None => return None,
                };
                let kind = classify(&e.name, &d.cmd)?;
                let prev = cpu
                    .get(&e.pid)
                    .map(|s| (now.duration_since(s.at), s.cpu_100ns));
                seen.insert(
                    e.pid,
                    CpuSample {
                        at: now,
                        cpu_100ns: d.cpu_100ns,
                    },
                );
                Some(ProcInfo {
                    pid: e.pid,
                    parent: e.parent,
                    name: e.name,
                    cmd: d.cmd,
                    exe: d.exe,
                    kind,
                    cpu_pct: cpu_pct(prev, d.cpu_100ns),
                    mem_mb: d.mem_bytes as f64 / (1024.0 * 1024.0),
                    started_unix_s: d.started_unix_s,
                    is_self: e.pid == self.self_pid,
                })
            })
            .collect();
        // Forget processes that are gone, so a recycled PID never inherits a
        // dead process's CPU sample.
        *cpu = seen;
        out.sort_by_key(|p| (p.kind.order(), p.pid));
        out
    }

    /// Terminate `pid`, but only if it is still a related process and not us.
    pub fn kill(&self, pid: u32) -> Result<ProcInfo, KillError> {
        if pid == self.self_pid {
            return Err(KillError::IsSelf);
        }
        // Whatever happens next, the table is re-read before it is shown
        // again: a process the panel just terminated must not linger.
        self.invalidate();
        let entry = sys::enumerate()
            .into_iter()
            .find(|e| e.pid == pid)
            .ok_or(KillError::NotFound)?;
        kill_entry(entry, sys::open_for_kill)
    }
}

/// An open handle on a process, good for both reading what it is and
/// terminating it. Reading and terminating through the *same* handle is the
/// point: a PID can be recycled between two `OpenProcess` calls, a handle
/// cannot change the process it names.
pub trait KillHandle {
    /// Image path, command line, times and memory, read through this handle.
    fn details(&self) -> Details;
    /// Terminate the process this handle names.
    fn terminate(&self) -> bool;
}

/// The policy half of [`Scanner::kill`], over whatever opens the handle:
///
/// 1. the snapshot's name must be a candidate *before* anything is opened —
///    the panel never opens a handle on a process whose name says it is not
///    telemouse's (the anticheat posture: games are never touched);
/// 2. one handle, for querying and terminating, is opened;
/// 3. the process is classified again from what that handle reads (its own
///    image name and command line), so a PID that was recycled since the
///    snapshot is refused;
/// 4. and terminated through the same handle.
pub fn kill_entry<H: KillHandle>(
    entry: Entry,
    open: impl FnOnce(u32) -> Option<H>,
) -> Result<ProcInfo, KillError> {
    if !name_is_candidate(&entry.name) {
        return Err(KillError::NotRelated);
    }
    // Access denied (an elevated process), or it is gone already.
    let handle = open(entry.pid).ok_or(KillError::Failed)?;
    let d = handle.details();
    if d.exited {
        return Err(KillError::NotFound);
    }
    // The name the handle reports beats the snapshot's: they differ only
    // when the PID now belongs to another process.
    let name = d
        .exe
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.name.clone());
    if !name_is_candidate(&name) {
        return Err(KillError::NotRelated);
    }
    let kind = classify(&name, &d.cmd).ok_or(KillError::NotRelated)?;
    let info = ProcInfo {
        pid: entry.pid,
        parent: entry.parent,
        name: entry.name,
        cmd: d.cmd,
        exe: d.exe,
        kind,
        cpu_pct: 0.0,
        mem_mb: d.mem_bytes as f64 / (1024.0 * 1024.0),
        started_unix_s: d.started_unix_s,
        is_self: false,
    };
    if handle.terminate() {
        Ok(info)
    } else {
        Err(KillError::Failed)
    }
}

/// One row of the process table: what the cheap enumeration knows.
#[derive(Debug, Clone)]
pub struct Entry {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
}

/// What the per-process queries add, for the few processes that matter.
#[derive(Debug, Clone, Default)]
pub struct Details {
    pub cmd: String,
    pub exe: Option<String>,
    /// Kernel + user time, in 100 ns units.
    pub cpu_100ns: u64,
    /// Resident (working set) size.
    pub mem_bytes: u64,
    pub started_unix_s: u64,
    /// The process has exited (its handle still answers because someone —
    /// often this panel's own child slot — holds it open).
    pub exited: bool,
}

#[cfg(windows)]
mod sys {
    use windows::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, UNICODE_STRING,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    };
    use windows::core::PWSTR;

    use super::{Details, Entry};

    /// Seconds between the FILETIME epoch (1601) and the Unix epoch, in 100 ns.
    const FILETIME_UNIX_DIFF: u64 = 116_444_736_000_000_000;

    fn ft(f: FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
    }

    fn wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// One snapshot of the process table: PIDs, parents and image names.
    pub fn enumerate() -> Vec<Entry> {
        let mut out = Vec::new();
        // SAFETY: Toolhelp snapshot iteration with a correctly sized entry;
        // the handle is closed on every path.
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return out;
            };
            let mut e = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snap, &mut e).is_ok() {
                loop {
                    out.push(Entry {
                        pid: e.th32ProcessID,
                        parent: (e.th32ParentProcessID != 0).then_some(e.th32ParentProcessID),
                        name: wide(&e.szExeFile),
                    });
                    if Process32NextW(snap, &mut e).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    /// The command line, without touching the target's address space:
    /// `ProcessCommandLineInformation` (Windows 8.1+) works with the limited
    /// query right and returns a `UNICODE_STRING` followed by its characters.
    ///
    /// # Safety
    /// `h` must be an open process handle with at least
    /// `PROCESS_QUERY_LIMITED_INFORMATION`.
    unsafe fn command_line(h: HANDLE) -> String {
        let mut len = 0u32;
        // SAFETY: a size probe with a null buffer; the API reports the length.
        let _ = unsafe {
            NtQueryInformationProcess(
                h,
                ProcessCommandLineInformation,
                std::ptr::null_mut(),
                0,
                &mut len,
            )
        };
        if len == 0 || len > 1 << 20 {
            return String::new();
        }
        // A `u64` buffer, not `u8`: the `UNICODE_STRING` header the API
        // writes at the start has a pointer in it (align 8), and only the
        // element type decides the allocation's alignment.
        let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        let cap = (buf.len() * size_of::<u64>()) as u32;
        // SAFETY: the buffer is at least `len` bytes and 8-aligned.
        let status = unsafe {
            NtQueryInformationProcess(
                h,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast(),
                cap,
                &mut len,
            )
        };
        if status.is_err() || (cap as usize) < size_of::<UNICODE_STRING>() {
            return String::new();
        }
        // SAFETY: the API wrote an 8-aligned UNICODE_STRING at the buffer
        // start whose Buffer points inside the same allocation, which lives
        // until the end of this function.
        unsafe {
            let us = &*(buf.as_ptr() as *const UNICODE_STRING);
            if us.Buffer.is_null() || us.Length == 0 {
                return String::new();
            }
            let chars = std::slice::from_raw_parts(us.Buffer.0, (us.Length / 2) as usize);
            String::from_utf16_lossy(chars)
        }
    }

    /// An open process handle, closed on drop.
    pub struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    impl super::KillHandle for Handle {
        fn details(&self) -> Details {
            // SAFETY: an open handle with the query right (see open_for_kill).
            unsafe { query(self.0) }
        }

        fn terminate(&self) -> bool {
            // SAFETY: an open handle with PROCESS_TERMINATE.
            unsafe { TerminateProcess(self.0, 1).is_ok() }
        }
    }

    /// One handle to query *and* terminate `pid` with, so the process that is
    /// classified is the process that is terminated. Only ever called for a
    /// PID whose snapshot name is a telemouse candidate (`kill_entry`).
    pub fn open_for_kill(pid: u32) -> Option<Handle> {
        // SAFETY: OpenProcess takes integers; the handle is owned by `Handle`.
        unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
            .ok()
            .map(Handle)
        }
    }

    pub fn details(pid: u32) -> Option<Details> {
        // SAFETY: OpenProcess takes integers; the handle is owned by `Handle`
        // and closed when it drops, and it has the query right `query` needs.
        unsafe {
            let h = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?);
            Some(query(h.0))
        }
    }

    /// Everything [`Details`] holds, read through `h`.
    ///
    /// # Safety
    /// `h` must be an open process handle with at least
    /// `PROCESS_QUERY_LIMITED_INFORMATION`.
    unsafe fn query(h: HANDLE) -> Details {
        // SAFETY: the caller's handle is open with the query right; the
        // out-parameters are plain stack structs sized as the APIs expect.
        unsafe {
            let mut d = Details::default();

            let mut path = [0u16; 1024];
            let mut n = path.len() as u32;
            if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut n)
                .is_ok()
            {
                d.exe = Some(String::from_utf16_lossy(&path[..n as usize]));
            }

            let (mut created, mut exited, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            if GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user).is_ok() {
                d.cpu_100ns = ft(kernel).saturating_add(ft(user));
                d.started_unix_s = ft(created).saturating_sub(FILETIME_UNIX_DIFF) / 10_000_000;
                // A nonzero exit time means the process is gone even though
                // the handle opened.
                d.exited = ft(exited) != 0;
            }

            let mut mem = PROCESS_MEMORY_COUNTERS {
                cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
            if GetProcessMemoryInfo(h, &mut mem, mem.cb).is_ok() {
                d.mem_bytes = mem.WorkingSetSize as u64;
            }

            d.cmd = command_line(h);
            d
        }
    }

    /// The Win32 error behind a `windows::core::Error`, undoing the
    /// `HRESULT_FROM_WIN32` wrapping the crate applies.
    fn win32_code(e: &windows::core::Error) -> u32 {
        let c = e.code().0 as u32;
        if c & 0xFFFF_0000 == 0x8007_0000 {
            c & 0xFFFF
        } else {
            c
        }
    }

    /// Is there a process with this PID right now? Asked only when
    /// `details` failed, which is the elevated-process case.
    ///
    /// `OpenProcess` already answers this: *access denied* means a process
    /// is there and is not ours to query, *invalid parameter* means the PID
    /// names nothing. Both are a single syscall; the full snapshot walk
    /// (~7 ms, because the kernel walks every thread to build it) is kept
    /// only for an error neither of those covers.
    pub fn exists(pid: u32) -> bool {
        // SAFETY: OpenProcess takes integers and returns a handle we close
        // immediately; nothing is read from the target.
        unsafe {
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(h) => {
                    let _ = CloseHandle(h);
                    true
                }
                Err(e) => {
                    let code = win32_code(&e);
                    if code == ERROR_ACCESS_DENIED.0 {
                        true
                    } else if code == ERROR_INVALID_PARAMETER.0 {
                        false
                    } else {
                        enumerate().iter().any(|e| e.pid == pid)
                    }
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    use super::{Details, Entry};

    fn table(which: ProcessesToUpdate<'_>) -> System {
        let mut s = System::new();
        s.refresh_processes_specifics(
            which,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );
        s
    }

    pub fn enumerate() -> Vec<Entry> {
        table(ProcessesToUpdate::All)
            .processes()
            .iter()
            .map(|(pid, p)| Entry {
                pid: pid.as_u32(),
                parent: p.parent().map(Pid::as_u32),
                name: p.name().to_string_lossy().into_owned(),
            })
            .collect()
    }

    pub fn details(pid: u32) -> Option<Details> {
        let target = Pid::from_u32(pid);
        let s = table(ProcessesToUpdate::Some(&[target]));
        let p = s.process(target)?;
        Some(Details {
            cmd: p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            exe: p.exe().map(|e| e.display().to_string()),
            cpu_100ns: p.accumulated_cpu_time() * 10_000,
            mem_bytes: p.memory(),
            started_unix_s: p.start_time(),
            exited: false,
        })
    }

    pub fn exists(pid: u32) -> bool {
        let target = Pid::from_u32(pid);
        table(ProcessesToUpdate::Some(&[target]))
            .process(target)
            .is_some()
    }

    /// Off Windows there is no handle to hold: the PID is re-read for both
    /// halves. Capture is Windows-only; this keeps the policy testable.
    pub struct Handle {
        pid: u32,
    }

    impl super::KillHandle for Handle {
        fn details(&self) -> Details {
            details(self.pid).unwrap_or_default()
        }

        fn terminate(&self) -> bool {
            let target = Pid::from_u32(self.pid);
            let s = table(ProcessesToUpdate::Some(&[target]));
            s.process(target).is_some_and(|p| p.kill())
        }
    }

    pub fn open_for_kill(pid: u32) -> Option<Handle> {
        exists(pid).then_some(Handle { pid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_workspace_binaries_case_insensitively() {
        assert_eq!(classify("telemouse.exe", ""), Some(ProcKind::Capture));
        assert_eq!(classify("telemouse", ""), Some(ProcKind::Capture));
        assert_eq!(classify("TELEMOUSE-VIZ.EXE", ""), Some(ProcKind::Viz));
        assert_eq!(
            classify("telemouse-analyze.exe", ""),
            Some(ProcKind::Analyze)
        );
        assert_eq!(classify("telemouse-ctl.exe", ""), Some(ProcKind::Ctl));
        // A test binary or a future tool: listed, as "other".
        assert_eq!(
            classify("telemouse_ctl-1a2b3c.exe", ""),
            Some(ProcKind::Other)
        );
    }

    #[test]
    fn cargo_wrappers_count_only_when_they_name_telemouse() {
        assert_eq!(
            classify("cargo.exe", "cargo run --release -p telemouse-viz"),
            Some(ProcKind::Cargo)
        );
        assert_eq!(classify("cargo.exe", "cargo build -p other-project"), None);
        assert_eq!(classify("cargo", ""), None);
    }

    #[test]
    fn unrelated_processes_are_invisible() {
        for (name, cmd) in [
            ("explorer.exe", ""),
            ("cs2.exe", "cs2.exe -novid"),
            (
                "rustrover64.exe",
                "C:\\RustRover\\bin\\rustrover64.exe C:\\telemouse",
            ),
            ("code.exe", "code telemouse"),
            ("notelemouse.exe", ""),
        ] {
            assert_eq!(classify(name, cmd), None, "{name}");
        }
    }

    #[test]
    fn the_name_prefilter_admits_everything_classify_can_accept() {
        // Anything classify() might say yes to must pass the prefilter, or
        // the scan would never query it.
        for name in [
            "telemouse.exe",
            "TELEMOUSE-VIZ.EXE",
            "telemouse_ctl-1a2b.exe",
            "cargo.exe",
            "cargo",
        ] {
            assert!(name_is_candidate(name), "{name}");
        }
        for name in ["explorer.exe", "cs2.exe", "notelemouse.exe", "System"] {
            assert!(!name_is_candidate(name), "{name}");
        }
    }

    #[test]
    fn cpu_pct_is_a_rate_over_the_previous_interval() {
        assert_eq!(cpu_pct(None, 5_000_000), 0.0, "first sighting");
        assert_eq!(
            cpu_pct(Some((Duration::ZERO, 0)), 5_000_000),
            0.0,
            "no interval"
        );
        // 250 ms of CPU over a 1 s interval = 25% of one core.
        let pct = cpu_pct(Some((Duration::from_secs(1), 10_000_000)), 12_500_000);
        assert!((pct - 25.0).abs() < 1e-3, "{pct}");
        // A counter that went backwards (PID recycled) is 0, not negative.
        assert_eq!(
            cpu_pct(Some((Duration::from_secs(1), 9_000_000)), 1_000),
            0.0
        );
    }

    #[test]
    fn scan_lists_related_processes_ordered_and_marks_self() {
        let s = Scanner::new();
        let list = s.scan();
        // Ordering invariant holds whatever happens to be running.
        for w in list.windows(2) {
            assert!((w[0].kind.order(), w[0].pid) <= (w[1].kind.order(), w[1].pid));
        }
        for p in &list {
            assert_eq!(p.is_self, p.pid == std::process::id());
            assert!(classify(&p.name, &p.cmd).is_some());
        }
        // This test binary is itself a telemouse process, so the scan is
        // never empty here — and the details came through for it.
        let me = list
            .iter()
            .find(|p| p.is_self)
            .expect("the test process must list itself");
        assert_eq!(me.kind, ProcKind::Other);
        assert!(me.mem_mb > 0.0, "working set should be known: {me:?}");
        assert!(me.started_unix_s > 1_600_000_000, "{}", me.started_unix_s);
        assert!(me.exe.as_deref().is_some_and(|e| !e.is_empty()));
        assert!(
            me.cmd.to_ascii_lowercase().contains("telemouse"),
            "{:?}",
            me.cmd
        );
        assert_eq!(me.cpu_pct, 0.0, "no rate on the first scan");
        // The second scan reports a rate over the interval, never negative.
        let again = s.scan();
        let me2 = again.iter().find(|p| p.is_self).unwrap();
        assert!(me2.cpu_pct >= 0.0);
    }

    #[test]
    fn cached_scan_is_reused_within_ttl_and_invalidated_by_kill() {
        let s = Scanner::new();
        let a = s.scan_cached(Duration::from_secs(60));
        let b = s.scan_cached(Duration::from_secs(60));
        assert_eq!(a, b, "a cached scan is returned verbatim");
        assert!(s.last.lock().unwrap().is_some());
        assert!(
            s.known.lock().unwrap().is_some(),
            "the enumeration is remembered too"
        );
        // A refused kill still drops both caches: the state was touched.
        let _ = s.kill(u32::MAX - 7);
        assert!(s.last.lock().unwrap().is_none());
        assert!(s.known.lock().unwrap().is_none());
        // A fresh scan re-enumerates; the one after it reuses the enumeration
        // (only the per-process details are refreshed).
        let _ = s.scan();
        let enumerated_at = s.known.lock().unwrap().as_ref().map(|(t, _)| *t).unwrap();
        let _ = s.scan();
        assert_eq!(
            s.known.lock().unwrap().as_ref().map(|(t, _)| *t).unwrap(),
            enumerated_at,
            "a scan inside ENUMERATE_EVERY must not walk the process table again"
        );
        s.invalidate();
        assert!(s.known.lock().unwrap().is_none());
        // A zero TTL always rescans.
        let _ = s.scan_cached(Duration::ZERO);
        let at = s.last.lock().unwrap().as_ref().map(|(t, _)| *t).unwrap();
        let _ = s.scan_cached(Duration::ZERO);
        assert!(s.last.lock().unwrap().as_ref().map(|(t, _)| *t).unwrap() > at);
    }

    #[test]
    fn kill_refuses_self_and_unrelated_targets() {
        let s = Scanner::new();
        assert_eq!(s.kill(std::process::id()).unwrap_err(), KillError::IsSelf);
        // PID 4 is the Windows System process, PID 1 is init elsewhere:
        // present, and never ours.
        let system_pid = if cfg!(windows) { 4 } else { 1 };
        match s.kill(system_pid) {
            Err(KillError::NotRelated) | Err(KillError::NotFound) => {}
            other => panic!("must never terminate an unrelated process: {other:?}"),
        }
        assert_eq!(s.kill(u32::MAX - 7).unwrap_err(), KillError::NotFound);
    }

    #[test]
    fn enumeration_sees_this_process_and_a_scan_is_cheap() {
        let me = std::process::id();
        let entries = sys::enumerate();
        assert!(entries.iter().any(|e| e.pid == me));
        let d = sys::details(me).expect("own details");
        assert!(d.mem_bytes > 0);
        assert!(!d.exited);
        assert!(sys::exists(me));
        assert!(!sys::exists(u32::MAX - 7));
        // A process that exists but refuses the query handle still reads as
        // alive (Windows' System process, PID 4, is the standing example),
        // and the answer comes from OpenProcess alone.
        if cfg!(windows) {
            // 100 calls; a table walk each (~7 ms) would be the better part
            // of a second. Bound generously for a loaded CI box.
            let t0 = Instant::now();
            for _ in 0..50 {
                let _ = sys::exists(me);
                let _ = sys::exists(u32::MAX - 7);
            }
            assert!(
                t0.elapsed() < Duration::from_millis(300),
                "sys::exists must not walk the process table: {:?}",
                t0.elapsed()
            );
        }
        // The whole point of the rewrite: a scan between enumerations is a
        // handful of per-process queries, not a walk of the process table.
        // Asserted as an ordering (a details-only scan is cheaper than one
        // that also enumerates), which holds however slow the box is.
        let s = Scanner::new();
        let _ = s.scan(); // enumerates
        let t0 = Instant::now();
        for _ in 0..20 {
            let _ = s.scan();
        }
        let details_only = t0.elapsed();
        let t0 = Instant::now();
        for _ in 0..20 {
            s.invalidate();
            let _ = s.scan();
        }
        let enumerating = t0.elapsed();
        assert!(
            details_only < enumerating,
            "details-only scans ({details_only:?}) must be cheaper than enumerating ones ({enumerating:?})"
        );
        assert!(
            enumerating / 20 < Duration::from_secs(1),
            "enumeration took {:?}",
            enumerating / 20
        );
    }

    /// A handle that records what was asked of it.
    struct FakeHandle {
        details: Details,
        terminated: std::cell::Cell<bool>,
    }

    impl KillHandle for &FakeHandle {
        fn details(&self) -> Details {
            self.details.clone()
        }
        fn terminate(&self) -> bool {
            self.terminated.set(true);
            true
        }
    }

    fn entry(pid: u32, name: &str) -> Entry {
        Entry {
            pid,
            parent: None,
            name: name.into(),
        }
    }

    /// The anticheat rule: a process whose name is not telemouse's is never
    /// opened, not even to be looked at.
    #[test]
    fn a_non_candidate_name_never_reaches_the_handle() {
        for name in ["cs2.exe", "explorer.exe", "System", "notelemouse.exe"] {
            let r = kill_entry::<&FakeHandle>(entry(1234, name), |_| {
                panic!("{name}: a handle was opened on a process that is not telemouse's")
            });
            assert_eq!(r.unwrap_err(), KillError::NotRelated, "{name}");
        }
    }

    #[test]
    fn kill_classifies_through_the_handle_and_terminates_through_it() {
        // The ordinary case: snapshot and handle agree.
        let h = FakeHandle {
            details: Details {
                exe: Some(r"C:\tm\telemouse-viz.exe".into()),
                cmd: "telemouse-viz serve".into(),
                ..Default::default()
            },
            terminated: false.into(),
        };
        let mut opened = None;
        let info = kill_entry(entry(42, "telemouse-viz.exe"), |pid| {
            opened = Some(pid);
            Some(&h)
        })
        .unwrap();
        assert_eq!(opened, Some(42));
        assert_eq!(info.kind, ProcKind::Viz);
        assert!(h.terminated.get());

        // The PID was recycled since the snapshot: the handle names another
        // program, and nothing is terminated.
        let recycled = FakeHandle {
            details: Details {
                exe: Some(r"C:\Games\game.exe".into()),
                ..Default::default()
            },
            terminated: false.into(),
        };
        let r = kill_entry(entry(42, "telemouse.exe"), |_| Some(&recycled));
        assert_eq!(r.unwrap_err(), KillError::NotRelated);
        assert!(!recycled.terminated.get());

        // A cargo wrapper is decided by the command line the handle reads.
        let cargo = FakeHandle {
            details: Details {
                exe: Some(r"C:\cargo\bin\cargo.exe".into()),
                cmd: "cargo build -p other".into(),
                ..Default::default()
            },
            terminated: false.into(),
        };
        let r = kill_entry(entry(7, "cargo.exe"), |_| Some(&cargo));
        assert_eq!(r.unwrap_err(), KillError::NotRelated);
        assert!(!cargo.terminated.get());

        // Gone between the snapshot and the open; or refused the handle.
        let exited = FakeHandle {
            details: Details {
                exited: true,
                ..Default::default()
            },
            terminated: false.into(),
        };
        let r = kill_entry(entry(9, "telemouse.exe"), |_| Some(&exited));
        assert_eq!(r.unwrap_err(), KillError::NotFound);
        assert!(!exited.terminated.get());
        let r = kill_entry::<&FakeHandle>(entry(9, "telemouse.exe"), |_| None);
        assert_eq!(r.unwrap_err(), KillError::Failed);
    }

    #[test]
    fn a_process_that_exited_leaves_the_table_before_the_next_enumeration() {
        // Spawn a child, let the scanner see it, end it, and check the next
        // scan (inside ENUMERATE_EVERY, so details-only) drops it.
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                ["/C", "ping -n 30 127.0.0.1 > NUL"]
            } else {
                ["-c", "sleep 30"]
            })
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn a child");
        let pid = child.id();
        let s = Scanner::new();
        // The child is not a telemouse process, so it never shows in the
        // rendered list; exercise the mechanism through the entries instead.
        assert!(sys::exists(pid));
        let d = sys::details(pid).expect("details of a live child");
        assert!(!d.exited);
        child.kill().unwrap();
        let _ = child.wait();
        // Gone, whichever way the OS reports it.
        let gone = match sys::details(pid) {
            Some(d) => d.exited,
            None => !sys::exists(pid),
        };
        assert!(gone, "a dead child must read as gone");
        let _ = s.scan();
    }
}
