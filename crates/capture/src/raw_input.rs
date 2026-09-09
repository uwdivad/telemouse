//! T1 — the capture hot path.
//!
//! A message-only window (`HWND_MESSAGE` parent) registered for the mouse usage
//! page with `RIDEV_INPUTSINK`, so deltas keep arriving while a game holds the
//! foreground. The thread waits for raw input, timestamps with QPC, decodes,
//! and pushes fixed-size [`RawEvent`]s into the SPSC ring — no allocation on
//! the per-report path (the rare exceptions: a hotkey marker allocates its
//! label `String` and mpsc node, and the first sight of a new device allocates
//! its entry in the device table), no locks, no blocking on the consumer. A
//! full ring bumps a counter and the event is dropped: capture never stalls.
//!
//! ## Why the reads are batched, and why a live stream runs on a timer
//!
//! Two things about the Win32 raw-input path cost CPU out of proportion to
//! the work (measured cycle-exact on Zen 3 / Windows 10, 1kHz synthetic
//! input; see docs/BENCHMARKS.md):
//!
//! * A wake of the thread by the message queue — `MsgWaitForMultipleObjectsEx`
//!   returning because a `WM_INPUT` arrived — costs ~25–30µs of kernel CPU on
//!   this thread. One wake per report was ~2.8% of a core.
//! * The read itself (`GetRawInputData` and `GetRawInputBuffer` alike, whatever
//!   the buffered call returns) plus a timer wake is ~15µs per drain.
//!
//! So T1 only ever waits on the message queue while the mouse is *still*.
//! The first report of a burst wakes it (that arrival is observed exactly);
//! it then lets reports pile up for the coalescing window
//! (`batch.coalesce_ms`) on a high-resolution waitable timer and drains them
//! with one `GetRawInputBuffer`. From then on, for as long as each drain
//! returns something, the loop runs on the timer alone — periodic at the
//! window plus one report interval — and never touches the queue wait: at
//! 1kHz and 2ms that is ~3 reports per drain, 333 drains/s, and ~0.5% of a
//! core (wake-then-window on the queue was 1.4%; one read per report 2.8%).
//! A drain that comes back empty means the hand stopped: the timer is
//! cancelled and the thread goes back to the queue wait, so an idle desk
//! costs nothing.
//!
//! The price is timestamp resolution inside a drain: only a burst's first
//! report has an observed arrival. Later reports are spaced by the report
//! interval estimated from consecutive drains ([`DrainStamper`], pure and
//! tested), never past the drain time, so a 1kHz mouse still reads as ~1ms
//! intervals and velocity math never sees a zero delta. The window is
//! recorded in the session envelope (`coalesce_ms`) so consumers know; set it
//! to 0 for one read per report and exact per-report wake times. Messages
//! posted to the window (quit, hotkey, display change) are handled after the
//! next drain, at most one window late.
//!
//! The decode step is pure ([`decode_mouse`]) and therefore tested everywhere;
//! only the window plumbing is `#[cfg(windows)]`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::thread::Thread;

use telemouse_core::RawEvent;
use telemouse_core::event::buttons;

/// `RAWMOUSE.usFlags`: the frame carries absolute coordinates (tablets, RDP,
/// some virtual devices) rather than HID deltas.
pub const MOUSE_MOVE_ABSOLUTE: u16 = 0x0001;
/// `RAWMOUSE.usButtonFlags`: `usButtonData` holds a signed wheel delta.
pub const RI_MOUSE_WHEEL: u16 = 0x0400;
/// `RAWMOUSE.usButtonFlags`: `usButtonData` holds a signed *horizontal* (tilt)
/// wheel delta. Mutually exclusive with [`RI_MOUSE_WHEEL`] in one frame.
pub const RI_MOUSE_HWHEEL: u16 = 0x0800;

/// Where T1 publishes the handles the rest of the process needs to stop it.
///
/// Both are written before the message loop starts and read only at shutdown.
/// The thread id is the fallback path: if the window never came up, `hwnd`
/// stays 0 and `PostThreadMessageW(WM_QUIT)` still unblocks the wait, so
/// `join()` can never hang on a capture thread that failed to start properly.
#[derive(Debug, Default)]
pub struct CaptureHandles {
    hwnd: AtomicIsize,
    thread_id: AtomicU32,
}

impl CaptureHandles {
    pub fn hwnd(&self) -> isize {
        self.hwnd.load(Ordering::Acquire)
    }

    pub fn thread_id(&self) -> u32 {
        self.thread_id.load(Ordering::Acquire)
    }

    pub fn set_hwnd(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
    }

    pub fn set_thread_id(&self, id: u32) {
        self.thread_id.store(id, Ordering::Release);
    }
}

/// Lets producers wake T2 when it is parked with nothing in flight: T1 on the
/// first event of a batch, and anyone handing over a marker or a shutdown.
///
/// The hot path pays exactly one relaxed load per event. That load can, in
/// principle, observe a stale `false` while T2 is on its way into `park`
/// (x86 allows the store-then-load reordering on T2's side); T2's park timeout
/// is the backstop. T2's two-stage idle descent keeps that first backstop at
/// the batch window (see `shipping::next_park_timeout`), so a lost wake costs
/// at most one window — never the 1s idle park, and never a stall.
#[derive(Debug, Default)]
pub struct RingWaker {
    parked: AtomicBool,
    thread: OnceLock<Thread>,
}

impl RingWaker {
    /// Called once by T2 with its own handle.
    pub fn register(&self, thread: Thread) {
        let _ = self.thread.set(thread);
    }

    /// T2: about to park. Callers **must** re-check the ring afterwards.
    pub fn begin_park(&self) {
        self.parked.store(true, Ordering::SeqCst);
    }

    /// T2: awake again.
    pub fn end_park(&self) {
        self.parked.store(false, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn is_parked(&self) -> bool {
        self.parked.load(Ordering::Relaxed)
    }

    /// Hot path: wake T2 if (and only if) it is parked.
    #[inline]
    pub fn wake(&self) {
        if self.parked.load(Ordering::Relaxed)
            && let Some(t) = self.thread.get()
        {
            t.unpark();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded {
    /// A usable relative-motion frame.
    Relative(RawEvent),
    /// Absolute-coordinate frame: not a HID delta, so it is skipped (counted).
    Absolute,
}

/// Turn the raw `RAWMOUSE` fields into a [`RawEvent`].
pub fn decode_mouse(
    us_flags: u16,
    button_flags: u16,
    button_data: u16,
    last_x: i32,
    last_y: i32,
    ts_qpc: u64,
    device_ix: u8,
) -> Decoded {
    if us_flags & MOUSE_MOVE_ABSOLUTE != 0 {
        return Decoded::Absolute;
    }
    // `usButtonData` carries one wheel value; the flag says which axis it is.
    let (wheel, wheel_h) = if button_flags & RI_MOUSE_WHEEL != 0 {
        (button_data as i16, 0)
    } else if button_flags & RI_MOUSE_HWHEEL != 0 {
        (0, button_data as i16)
    } else {
        (0, 0)
    };
    Decoded::Relative(RawEvent {
        ts_qpc,
        dx: last_x,
        dy: last_y,
        // The core constants are numerically identical to the RI_MOUSE_* bits,
        // so this masks straight through with no translation table.
        buttons: button_flags & buttons::MASK,
        wheel,
        wheel_h,
        device_ix,
    })
}

/// Assigns timestamps to reports drained together.
///
/// The first report of a drain is stamped with the wake time (its arrival was
/// observed). Later ones are spaced by the current estimate of the device's
/// report interval, clamped so no stamp lands after the drain itself — which
/// also keeps stamps monotonic across drains, since the next wake is later
/// than this drain. The estimate is refined from consecutive drains: if the
/// previous drain had `n` reports and the next wake came `gap` later, the
/// device reported roughly every `gap / n`. A pause longer than `max_gap`
/// (the hand stopped) is not an observation.
///
/// All arithmetic is in QPC ticks; nothing here touches a clock, so the
/// policy is unit-testable with synthetic drains.
#[derive(Debug, Clone)]
pub struct DrainStamper {
    step: u64,
    min_step: u64,
    max_step: u64,
    max_gap: u64,
    prev_wake: Option<u64>,
    prev_n: usize,
}

impl DrainStamper {
    /// `initial_step`: spacing to assume before anything was observed (1ms is
    /// right for the usual 1kHz mouse). `max_step`: the coalescing window —
    /// reports drained together cannot be further apart than that.
    pub fn new(initial_step: u64, max_step: u64, max_gap: u64) -> Self {
        let max_step = max_step.max(1);
        Self {
            step: initial_step.clamp(1, max_step),
            min_step: 1,
            max_step,
            max_gap,
            prev_wake: None,
            prev_n: 0,
        }
    }

    /// Current report-interval estimate, in ticks.
    #[cfg(test)]
    pub fn step(&self) -> u64 {
        self.step
    }

    /// Timestamp for the `i`th report (0-based) of a drain that woke at `wake`
    /// and is being read at `now`.
    #[inline]
    pub fn stamp(&self, wake: u64, now: u64, i: usize) -> u64 {
        wake.saturating_add(self.step.saturating_mul(i as u64))
            .min(now.max(wake))
    }

    /// Timestamp for the `i`th of `n` reports drained on the cadence timer,
    /// where nothing observed any arrival: they came in somewhere in
    /// `(prev, now]`, the period since the previous drain. Spreading them
    /// evenly over that period is the choice that bounds the *interval*
    /// error — a period one report short or long changes every spacing by
    /// a fraction instead of leaving one double-length gap at the boundary —
    /// which is what velocity math cares about. Monotonic across drains: the
    /// last stamp is `now`, and the next drain starts after it.
    #[inline]
    pub fn spread(prev: u64, now: u64, n: usize, i: usize) -> u64 {
        let n = n.max(1) as u128;
        let span = now.saturating_sub(prev) as u128;
        prev.saturating_add((span * (i as u128 + 1) / n) as u64)
    }

    /// Record that a drain woke at `wake` and found `n` reports; refines the
    /// interval estimate from the previous drain.
    pub fn finish(&mut self, wake: u64, n: usize) {
        if let Some(prev) = self.prev_wake
            && self.prev_n > 0
            && n > 0
        {
            let gap = wake.saturating_sub(prev);
            if gap > 0 && gap <= self.max_gap {
                let observed = (gap / self.prev_n as u64).clamp(self.min_step, self.max_step);
                // Light smoothing: one odd drain should not swing the estimate.
                self.step = ((self.step * 3 + observed) / 4).clamp(self.min_step, self.max_step);
            }
        }
        if n > 0 {
            self.prev_wake = Some(wake);
            self.prev_n = n;
        }
    }
}

/// Round `addr` up to a multiple of `align` (a power of two). What the
/// `NEXTRAWINPUTBLOCK` macro does between blocks of a `GetRawInputBuffer`
/// result: each block starts on a pointer-sized boundary. Pure, so the walk's
/// arithmetic is testable off Windows.
pub fn align_up(addr: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (addr + align - 1) & !(align - 1)
}

#[cfg(windows)]
pub use win::{CaptureDeps, Hotkey, post_quit, post_thread_quit, run};

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use telemouse_core::RawEvent;
    use windows::Win32::Foundation::{
        CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW,
        GetCurrentThreadId, INFINITE, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForSingleObject,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_F9,
    };
    use windows::Win32::UI::Input::{
        GetRawInputBuffer, GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
        RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
        GetWindowLongPtrW, HWND_MESSAGE, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
        PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, PostThreadMessageW, QS_ALLINPUT,
        RegisterClassW, SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DESTROY,
        WM_DISPLAYCHANGE, WM_HOTKEY, WM_INPUT, WM_QUIT, WNDCLASSW,
    };
    use windows::core::{PCWSTR, w};

    use super::{CaptureHandles, Decoded, DrainStamper, RingWaker, decode_mouse};
    use crate::context::SharedContext;
    use crate::devices::{self, DeviceTable};
    use crate::platform;
    use crate::shipping::MarkerSignal;
    use crate::stats::Stats;

    /// HID usage page 0x01 (generic desktop), usage 0x02 (mouse).
    const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
    const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
    const HOTKEY_ID: i32 = 1;
    /// Private message asking the capture thread to leave its message loop.
    const WM_TELEMOUSE_QUIT: u32 = WM_APP + 1;
    const CLASS_NAME: PCWSTR = w!("TelemouseRawInputClass");
    /// Reports read per `GetRawInputBuffer` call. Mouse reports are 48 bytes;
    /// 256 of them cover a 25ms stall at 8kHz before a second call is needed.
    const RAW_BUFFER_REPORTS: usize = 256;
    /// A wake more than this after the previous drain means the mouse was
    /// still; the interval estimator ignores it.
    const STAMPER_MAX_GAP: Duration = Duration::from_millis(20);
    /// Added to the coalescing window to make the cadence period of a live
    /// stream: one report interval of the usual 1kHz mouse, so the first
    /// report of the next drain has arrived by the time the timer fires and
    /// drains stay the same size as with wake-then-window.
    const CADENCE_SLACK: Duration = Duration::from_millis(1);

    /// Which hotkey to register for markers.
    #[derive(Debug, Clone, Copy)]
    pub struct Hotkey {
        pub vk: u32,
        pub label: &'static str,
    }

    impl Default for Hotkey {
        fn default() -> Self {
            Self {
                vk: VK_F9.0 as u32,
                label: "hotkey",
            }
        }
    }

    /// Everything T1 needs, assembled by `main` before the thread is spawned.
    pub struct CaptureDeps {
        pub producer: rtrb::Producer<RawEvent>,
        pub marker_tx: Sender<MarkerSignal>,
        pub stats: Arc<Stats>,
        pub handles: Arc<CaptureHandles>,
        pub waker: Arc<RingWaker>,
        pub ctx: Arc<SharedContext>,
        /// The startup enumeration, whose `names()` are exactly
        /// `SessionConfig.devices`.
        pub devices: DeviceTable,
        pub hotkey: Hotkey,
        /// How long to let reports pile up after a wake before reading them
        /// all at once. Zero reads immediately (one report per wake).
        pub coalesce: Duration,
        pub qpc_freq: u64,
    }

    /// Everything the window procedure touches. Lives on the capture thread's
    /// stack for the lifetime of the message loop and is reachable from the
    /// window procedure through `GWLP_USERDATA`.
    struct CaptureState {
        producer: rtrb::Producer<RawEvent>,
        stats: Arc<Stats>,
        marker_tx: Sender<MarkerSignal>,
        hotkey_label: &'static str,
        waker: Arc<RingWaker>,
        ctx: Arc<SharedContext>,
        devices: DeviceTable,
        /// T1-private running totals. Published with plain relaxed *stores*, so
        /// the hot path never does a read-modify-write on a shared cache line.
        events: u64,
        ring_drops: u32,
        abs_frames: u32,
    }

    /// Ask the capture thread (identified by the HWND it published) to stop.
    /// Safe to call from any thread — `PostMessageW` is thread-safe.
    pub fn post_quit(hwnd: isize) {
        if hwnd == 0 {
            return;
        }
        let hwnd = HWND(hwnd as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_TELEMOUSE_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    /// Fallback stop path: post `WM_QUIT` straight to the thread's message
    /// queue. Works even if the window was never created, which is exactly the
    /// case where `post_quit` would silently do nothing and `join` would hang.
    pub fn post_thread_quit(thread_id: u32) {
        if thread_id == 0 {
            return;
        }
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_INPUT => {
                // Normally never reached: the loop drains raw input itself
                // and only dispatches the rest. Kept so a WM_INPUT that does
                // get dispatched is still captured rather than lost.
                let ts_qpc = platform::qpc();
                let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut CaptureState;
                if !state.is_null() {
                    unsafe { handle_input(&mut *state, lparam, ts_qpc) };
                }
                // WM_INPUT must reach DefWindowProc so the system can clean up.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_HOTKEY => {
                let ts_qpc = platform::qpc();
                let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut CaptureState;
                if !state.is_null() {
                    let state = unsafe { &mut *state };
                    let _ = state.marker_tx.send(MarkerSignal {
                        ts_qpc,
                        label: state.hotkey_label.to_string(),
                    });
                    // T2 may be parked indefinitely on an idle desk.
                    state.waker.wake();
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                // The desktop geometry changed; T3 owns the context snapshot,
                // so just flag it and let the next tick re-read the metrics.
                let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut CaptureState;
                if !state.is_null() {
                    unsafe { (*state).ctx.mark_display_changed() };
                }
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
            WM_TELEMOUSE_QUIT | WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// Single-report read for a `WM_INPUT` that reached the message loop
    /// (a report that arrived between the buffered drain and the peek).
    unsafe fn handle_input(state: &mut CaptureState, lparam: LPARAM, ts_qpc: u64) {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        let read = unsafe {
            GetRawInputData(
                HRAWINPUT(lparam.0 as *mut c_void),
                RID_INPUT,
                Some(&mut raw as *mut RAWINPUT as *mut c_void),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if read == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE.0 {
            return;
        }
        unsafe { process_mouse(state, &raw, ts_qpc) };
    }

    /// How a drain's reports get their timestamps.
    #[derive(Debug, Clone, Copy)]
    enum Stamping {
        /// The first report's arrival was observed (`wake`): it is stamped
        /// exactly and the rest follow at the estimated interval.
        Anchored { wake: u64 },
        /// A cadence-timer drain: nothing observed any arrival, so the
        /// reports are spread over the period since the previous drain.
        Spread { prev: u64 },
    }

    /// Read every report waiting in the queue with as few calls as possible.
    /// Returns how many were read. Stamps come from `stamper` per `stamping`,
    /// with `now` the read time.
    unsafe fn drain_buffer(
        state: &mut CaptureState,
        buf: &mut [RAWINPUT],
        stamper: &DrainStamper,
        stamping: Stamping,
        now: u64,
    ) -> usize {
        let mut total = 0usize;
        // For a spread drain that needs more than one call (only above ~28kHz
        // at the default window), each call spreads over what is left.
        let mut spread_from = match stamping {
            Stamping::Spread { prev } => prev,
            Stamping::Anchored { .. } => 0,
        };
        loop {
            let mut size = size_of_val(buf) as u32;
            let n = unsafe {
                GetRawInputBuffer(
                    Some(buf.as_mut_ptr()),
                    &mut size,
                    size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if n == 0 || n == u32::MAX {
                break;
            }
            // Walk by `dwSize`: mouse reports are fixed-size, but the buffer
            // format is variable-length by contract, and the next block
            // starts at the pointer-aligned boundary after this one
            // (`NEXTRAWINPUTBLOCK`: QWORD on 64-bit, DWORD on 32-bit). Both
            // the header read and the block itself are bounded by the
            // buffer, so a count the API over-reports can never walk past
            // the allocation.
            let start = buf.as_ptr() as usize;
            let end = start + size_of_val(buf);
            let mut p = start;
            for i in 0..n as usize {
                let Some(block) = (unsafe { next_block_bounds(p, end) }) else {
                    break;
                };
                let raw = unsafe { &*(p as *const RAWINPUT) };
                if raw.header.dwType == RIM_TYPEMOUSE.0 {
                    let ts = match stamping {
                        Stamping::Anchored { wake } => stamper.stamp(wake, now, total),
                        Stamping::Spread { .. } => {
                            DrainStamper::spread(spread_from, now, n as usize, i)
                        }
                    };
                    unsafe { process_mouse(state, raw, ts) };
                }
                total += 1;
                p = block;
            }
            if (n as usize) < buf.len() {
                break;
            }
            spread_from = now;
        }
        total
    }

    /// Where the block after the one at `p` starts, or `None` if the block
    /// at `p` cannot be trusted: its header would not fit before `end`, or
    /// its `dwSize` runs past `end`. Reads only the header.
    ///
    /// # Safety
    /// `p` must point at a readable `RAWINPUTHEADER` when `p + header <= end`.
    unsafe fn next_block_bounds(p: usize, end: usize) -> Option<usize> {
        let header = size_of::<RAWINPUTHEADER>();
        if p.checked_add(header)? > end {
            return None;
        }
        let size = unsafe { (*(p as *const RAWINPUTHEADER)).dwSize } as usize;
        let block_end = p.checked_add(size.max(header))?;
        if block_end > end {
            return None;
        }
        Some(super::align_up(block_end, size_of::<usize>()))
    }

    /// The decode+push half of the hot path. No allocation on the per-report
    /// path, no locking (a never-before-seen device is queried and added to
    /// the table once — the rare exception).
    unsafe fn process_mouse(state: &mut CaptureState, raw: &RAWINPUT, ts_qpc: u64) {
        let mouse = unsafe { raw.data.mouse };
        let buttons = unsafe { mouse.Anonymous.Anonymous };
        // Linear scan over a handful of handles; only a device we have never
        // seen costs an OS query, and only once.
        let device_ix = devices::index_for(&mut state.devices, raw.header.hDevice.0 as isize);
        match decode_mouse(
            mouse.usFlags.0,
            buttons.usButtonFlags,
            buttons.usButtonData,
            mouse.lLastX,
            mouse.lLastY,
            ts_qpc,
            device_ix,
        ) {
            Decoded::Absolute => {
                state.abs_frames = state.abs_frames.wrapping_add(1);
                state
                    .stats
                    .t1
                    .abs_frames
                    .store(state.abs_frames, Ordering::Relaxed);
            }
            Decoded::Relative(ev) => {
                state
                    .stats
                    .t1
                    .last_event_qpc
                    .store(ts_qpc, Ordering::Relaxed);
                match state.producer.push(ev) {
                    Ok(()) => {
                        state.events = state.events.wrapping_add(1);
                        state.stats.t1.events.store(state.events, Ordering::Relaxed);
                        state.waker.wake();
                    }
                    // Ring full: drop, count, keep going. Never block here.
                    Err(_) => {
                        state.ring_drops = state.ring_drops.wrapping_add(1);
                        state
                            .stats
                            .t1
                            .ring_drops
                            .store(state.ring_drops, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Create the process-lifetime coalescing timer, preferring the
    /// high-resolution kind (std's `thread::sleep` overshoots by ~0.5–0.8ms
    /// per window; the high-resolution timer fires on time). The flag is
    /// rejected on older Windows 10 builds, where a plain waitable timer is
    /// still better than the sleep. `None` means no waitable timer at all —
    /// the caller falls back to `thread::sleep`.
    fn create_coalesce_timer() -> Option<HANDLE> {
        unsafe {
            match CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            ) {
                Ok(t) => {
                    tracing::info!(resolution = "high", "coalesce timer ready");
                    Some(t)
                }
                Err(high_res_err) => {
                    match CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0) {
                        Ok(t) => {
                            tracing::info!(
                                resolution = "standard",
                                high_res_error = %high_res_err,
                                "high-resolution timer unavailable; coalesce timer ready"
                            );
                            Some(t)
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "no waitable timer; coalescing falls back to thread::sleep"
                            );
                            None
                        }
                    }
                }
            }
        }
    }

    /// Run the capture thread: create the window, register raw input and the
    /// marker hotkey, then wait for input until asked to quit.
    ///
    /// The thread id and window handle are published through
    /// [`CaptureHandles`] as soon as they exist, so shutdown always has a way
    /// to break the loop.
    pub fn run(deps: CaptureDeps) -> Result<()> {
        let CaptureDeps {
            producer,
            marker_tx,
            stats,
            handles,
            waker,
            ctx,
            devices,
            hotkey,
            coalesce,
            qpc_freq,
        } = deps;
        handles.set_thread_id(unsafe { GetCurrentThreadId() });
        platform::raise_capture_thread_priority();

        let ticks = |d: Duration| (d.as_nanos() * qpc_freq as u128 / 1_000_000_000) as u64;
        // Reports drained together never span more than the window plus the
        // sleep's overshoot; 1ms is the right prior for a 1kHz mouse.
        let mut stamper = DrainStamper::new(
            ticks(Duration::from_millis(1)),
            ticks(coalesce + Duration::from_millis(1)).max(ticks(Duration::from_millis(1))),
            ticks(STAMPER_MAX_GAP),
        );

        unsafe {
            let hinstance = GetModuleHandleW(None).context("GetModuleHandleW")?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            // A zero return usually means "already registered", which is fine.
            let _ = RegisterClassW(&class);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                w!("telemouse"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinstance.into()),
                None,
            )
            .context("create message-only window")?;
            handles.set_hwnd(hwnd.0 as isize);

            let mut state = CaptureState {
                producer,
                stats,
                marker_tx,
                hotkey_label: hotkey.label,
                waker,
                ctx,
                devices,
                events: 0,
                ring_drops: 0,
                abs_frames: 0,
            };
            let state_ptr: *mut CaptureState = &mut state;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

            let rid = [RAWINPUTDEVICE {
                usUsagePage: HID_USAGE_PAGE_GENERIC,
                usUsage: HID_USAGE_GENERIC_MOUSE,
                // INPUTSINK: deliver input even when we are not foreground.
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            }];
            let registered = RegisterRawInputDevices(&rid, size_of::<RAWINPUTDEVICE>() as u32);
            if let Err(e) = registered {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = DestroyWindow(hwnd);
                return Err(e).context("RegisterRawInputDevices for the mouse usage page");
            }

            match RegisterHotKey(Some(hwnd), HOTKEY_ID, MOD_NOREPEAT, hotkey.vk) {
                Ok(()) => tracing::info!(vk = hotkey.vk, "marker hotkey registered"),
                // Another app owns the key: markers are a nice-to-have.
                Err(e) => tracing::warn!(error = %e, vk = hotkey.vk, "marker hotkey unavailable"),
            }

            tracing::info!(
                coalesce_ms = coalesce.as_millis() as u64,
                "raw input capture running"
            );
            // One process-lifetime timer for the coalescing window; the whole
            // drain path stays allocation-free.
            let coalesce_timer = if coalesce.is_zero() {
                None
            } else {
                create_coalesce_timer()
            };
            // While a stream is live the loop runs on the timer alone, one
            // drain per cadence period: the window plus one report interval,
            // so a 1kHz mouse yields the same drains per second (three
            // reports each at 2ms) as wake-then-window did.
            let cadence = coalesce + CADENCE_SLACK;
            let cadence_ms = cadence.as_millis().clamp(1, i32::MAX as u128) as i32;
            let mut raw_buf: Vec<RAWINPUT> = vec![RAWINPUT::default(); RAW_BUFFER_REPORTS];
            let mut msg = MSG::default();
            // True after a drain that returned reports: the stream is live
            // and the next drain is taken on the cadence timer rather than
            // on the queue wait (see the module docs for why that matters).
            let mut live = false;
            let mut prev_now: u64 = 0;
            'pump: loop {
                let stamping: Stamping;
                if live {
                    let mut waited = false;
                    if let Some(timer) = coalesce_timer {
                        // Armed periodic on the first drain of this stream;
                        // every wait here is one cadence period.
                        waited = WaitForSingleObject(timer, INFINITE) == WAIT_OBJECT_0;
                        if !waited {
                            let e = windows::core::Error::from_thread();
                            tracing::warn!(error = %e, "cadence wait failed; sleeping the period");
                        }
                    }
                    if !waited {
                        std::thread::sleep(cadence);
                    }
                    // Nothing observed any arrival this period: the reports
                    // are spread over it (see `DrainStamper::spread`).
                    stamping = Stamping::Spread { prev: prev_now };
                } else {
                    // Block until anything is queued. MWMO_INPUTAVAILABLE
                    // makes this return for messages that were already
                    // waiting, not only ones posted after the previous peek.
                    let r = MsgWaitForMultipleObjectsEx(
                        None,
                        INFINITE,
                        QS_ALLINPUT,
                        MWMO_INPUTAVAILABLE,
                    );
                    if r == WAIT_FAILED {
                        let e = windows::core::Error::from_thread();
                        tracing::error!(error = %e, "MsgWaitForMultipleObjectsEx failed; stopping capture");
                        break;
                    }
                    // The first report's arrival is now; let the next few
                    // land before paying for the read.
                    let wake = platform::qpc();
                    stamping = Stamping::Anchored { wake };
                    if !coalesce.is_zero() {
                        let mut waited = false;
                        if let Some(timer) = coalesce_timer {
                            // Negative due time = relative, in 100ns units:
                            // first due after one window, then periodic at
                            // the cadence for as long as the stream is live.
                            // A message posted meanwhile (quit, hotkey,
                            // display change) is handled after the drain, at
                            // most one window late.
                            let due = -((coalesce.as_nanos() / 100) as i64);
                            if SetWaitableTimer(timer, &due, cadence_ms, None, None, false).is_ok()
                            {
                                waited = WaitForSingleObject(timer, INFINITE) == WAIT_OBJECT_0;
                                if !waited {
                                    let e = windows::core::Error::from_thread();
                                    tracing::warn!(error = %e, "coalesce wait failed; sleeping the window");
                                }
                            }
                        }
                        if !waited {
                            std::thread::sleep(coalesce);
                        }
                    }
                }
                let now = platform::qpc();
                let n = drain_buffer(&mut *state_ptr, &mut raw_buf, &stamper, stamping, now);
                // The interval estimate only learns from observed wakes; a
                // spread drain has none (its spacing is the period over its
                // count, which needs no estimate).
                if let Stamping::Anchored { wake } = stamping {
                    stamper.finish(wake.min(now), n);
                }
                prev_now = now;
                let was_live = live;
                live = n > 0 && !coalesce.is_zero();
                if was_live
                    && !live
                    && let Some(timer) = coalesce_timer
                {
                    // The stream paused: stop the periodic timer so an idle
                    // desk costs nothing, and go back to waiting on the queue.
                    let _ = CancelWaitableTimer(timer);
                }

                // Everything that is not raw input — hotkey, display change,
                // quit — plus any WM_INPUT that slipped in after the drain.
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    match msg.message {
                        WM_QUIT => break 'pump,
                        WM_INPUT => {
                            handle_input(&mut *state_ptr, msg.lParam, platform::qpc());
                            let _ = DefWindowProcW(msg.hwnd, msg.message, msg.wParam, msg.lParam);
                        }
                        _ => {
                            let _ = DispatchMessageW(&msg);
                        }
                    }
                }
            }

            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            if let Some(timer) = coalesce_timer {
                let _ = CloseHandle(timer);
            }
            // Detach the state before it goes out of scope.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(hwnd);
            tracing::debug!("capture thread finished");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_motion_becomes_a_raw_event() {
        let d = decode_mouse(0, 0, 0, -7, 3, 42, 0);
        assert_eq!(
            d,
            Decoded::Relative(RawEvent {
                ts_qpc: 42,
                dx: -7,
                dy: 3,
                buttons: 0,
                wheel: 0,
                wheel_h: 0,
                device_ix: 0,
            })
        );
    }

    #[test]
    fn absolute_frames_are_rejected() {
        assert_eq!(
            decode_mouse(MOUSE_MOVE_ABSOLUTE, 0, 0, 100, 100, 1, 0),
            Decoded::Absolute
        );
        // Even with buttons attached: the deltas are not HID counts.
        assert_eq!(
            decode_mouse(
                MOUSE_MOVE_ABSOLUTE | 0x02,
                buttons::LEFT_DOWN,
                0,
                1,
                1,
                1,
                2
            ),
            Decoded::Absolute
        );
    }

    #[test]
    fn button_flags_are_masked_to_the_wire_bits() {
        // 0x0400 (wheel) and 0x0800 (hwheel) must not leak into `buttons`.
        let Decoded::Relative(ev) = decode_mouse(
            0,
            buttons::LEFT_DOWN | buttons::X2_UP | RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL,
            0,
            0,
            0,
            9,
            0,
        ) else {
            panic!("expected a relative event");
        };
        assert_eq!(ev.buttons, buttons::LEFT_DOWN | buttons::X2_UP);
        assert!(ev.is_click_down());
        assert!(ev.is_click_up());
    }

    #[test]
    fn wheel_data_is_signed_and_only_read_when_flagged() {
        // 0xFF88 as i16 = -120: one detent backwards.
        let Decoded::Relative(back) = decode_mouse(0, RI_MOUSE_WHEEL, 0xFF88, 0, 0, 1, 0) else {
            panic!("expected a relative event");
        };
        assert_eq!((back.wheel, back.wheel_h), (-120, 0));

        let Decoded::Relative(fwd) = decode_mouse(0, RI_MOUSE_WHEEL, 120, 0, 0, 1, 0) else {
            panic!("expected a relative event");
        };
        assert_eq!((fwd.wheel, fwd.wheel_h), (120, 0));

        // Same button data without the wheel bit is not a wheel event.
        let Decoded::Relative(none) = decode_mouse(0, buttons::MIDDLE_DOWN, 0xFF88, 0, 0, 1, 0)
        else {
            panic!("expected a relative event");
        };
        assert_eq!((none.wheel, none.wheel_h), (0, 0));
        assert_eq!(none.buttons, buttons::MIDDLE_DOWN);
    }

    #[test]
    fn horizontal_wheel_lands_in_its_own_field() {
        // Tilt right.
        let Decoded::Relative(right) = decode_mouse(0, RI_MOUSE_HWHEEL, 120, 0, 0, 7, 0) else {
            panic!("expected a relative event");
        };
        assert_eq!((right.wheel, right.wheel_h), (0, 120));
        // Tilt left: 0xFF88 as i16 = -120.
        let Decoded::Relative(left) = decode_mouse(0, RI_MOUSE_HWHEEL, 0xFF88, 0, 0, 7, 0) else {
            panic!("expected a relative event");
        };
        assert_eq!((left.wheel, left.wheel_h), (0, -120));
        // The tilt flag never leaks into the button bitfield.
        assert_eq!(left.buttons, 0);

        // A hwheel alongside a click keeps both.
        let Decoded::Relative(both) =
            decode_mouse(0, RI_MOUSE_HWHEEL | buttons::LEFT_DOWN, 120, 2, -2, 7, 0)
        else {
            panic!("expected a relative event");
        };
        assert_eq!(both.wheel_h, 120);
        assert_eq!(both.buttons, buttons::LEFT_DOWN);
        assert!(both.has_motion());
    }

    #[test]
    fn vertical_wheel_wins_when_both_bits_are_somehow_set() {
        // The two are mutually exclusive per frame; if a device lies, the
        // value belongs to exactly one axis and we must not double-count it.
        let Decoded::Relative(ev) =
            decode_mouse(0, RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL, 120, 0, 0, 1, 0)
        else {
            panic!("expected a relative event");
        };
        assert_eq!((ev.wheel, ev.wheel_h), (120, 0));
    }

    #[test]
    fn device_index_rides_along_untouched() {
        let Decoded::Relative(ev) = decode_mouse(0, 0, 0, 1, 1, 5, 3) else {
            panic!("expected a relative event");
        };
        assert_eq!(ev.device_ix, 3);
    }

    #[test]
    fn timestamp_passes_through_untouched() {
        let Decoded::Relative(ev) = decode_mouse(0, 0, 0, 1, 1, u64::MAX, 0) else {
            panic!("expected a relative event");
        };
        assert_eq!(ev.ts_qpc, u64::MAX);
    }

    #[test]
    fn block_boundaries_round_up_to_the_pointer_size() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(48, 8), 48);
        assert_eq!(align_up(49, 8), 56);
        assert_eq!(align_up(63, 8), 64);
        assert_eq!(align_up(4, 4), 4);
        assert_eq!(align_up(5, 4), 8);
    }

    #[test]
    fn handles_start_empty_and_publish_once_written() {
        let h = CaptureHandles::default();
        assert_eq!((h.hwnd(), h.thread_id()), (0, 0));
        h.set_thread_id(4242);
        h.set_hwnd(0x1234);
        assert_eq!((h.hwnd(), h.thread_id()), (0x1234, 4242));
    }

    #[test]
    fn waking_an_unparked_consumer_is_a_no_op() {
        let w = RingWaker::default();
        assert!(!w.is_parked());
        w.wake(); // no registered thread, no park: must not panic
        w.register(std::thread::current());
        w.wake();
        w.begin_park();
        assert!(w.is_parked());
        // Unparks *this* thread, so the next park returns immediately.
        w.wake();
        std::thread::park_timeout(std::time::Duration::from_secs(30));
        w.end_park();
        assert!(!w.is_parked());
    }

    // 10MHz QPC: 1ms = 10_000 ticks.
    const MS: u64 = 10_000;

    #[test]
    fn a_single_report_drain_is_stamped_at_the_wake() {
        let s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        assert_eq!(s.stamp(1_000_000, 1_000_000 + 2 * MS, 0), 1_000_000);
    }

    #[test]
    fn later_reports_are_spaced_by_the_estimate_and_never_pass_the_read_time() {
        let s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        let wake = 1_000_000;
        let now = wake + 25 * MS / 10; // read 2.5ms after the wake
        assert_eq!(s.stamp(wake, now, 1), wake + MS);
        assert_eq!(s.stamp(wake, now, 2), wake + 2 * MS);
        // The 4th report would land at +3ms, after the read: clamped.
        assert_eq!(s.stamp(wake, now, 3), now);
        assert_eq!(s.stamp(wake, now, 9), now);
        // A read time before the wake (clock oddity) still yields the wake.
        assert_eq!(s.stamp(wake, wake - 5, 2), wake);
    }

    #[test]
    fn spread_stamps_partition_the_period_evenly_and_end_at_the_read() {
        // Three reports over a 3ms period: 1ms apart, the last one at `now`.
        let (prev, now) = (1_000_000, 1_000_000 + 3 * MS);
        assert_eq!(DrainStamper::spread(prev, now, 3, 0), prev + MS);
        assert_eq!(DrainStamper::spread(prev, now, 3, 1), prev + 2 * MS);
        assert_eq!(DrainStamper::spread(prev, now, 3, 2), now);
        // A period one report short spreads the slack over both intervals
        // (1.5ms each) instead of leaving a 2ms gap at the boundary.
        assert_eq!(DrainStamper::spread(prev, now, 2, 0), prev + 15 * MS / 10);
        assert_eq!(DrainStamper::spread(prev, now, 2, 1), now);
        // One report: stamped at the read. Zero is treated as one.
        assert_eq!(DrainStamper::spread(prev, now, 1, 0), now);
        assert_eq!(DrainStamper::spread(prev, now, 0, 0), now);
        // Never before the previous drain, never after this one; a clock
        // oddity (now < prev) collapses onto prev rather than wrapping.
        assert_eq!(DrainStamper::spread(now, prev, 3, 1), now);
    }

    #[test]
    fn spread_drains_stay_monotonic_across_periods() {
        let mut last = 0u64;
        let mut prev = 5_000_000u64;
        for k in 0..50usize {
            let n = 2 + k % 3; // 2, 3, 4 reports per period
            let now = prev + 3 * MS;
            for i in 0..n {
                let t = DrainStamper::spread(prev, now, n, i);
                assert!(t > last, "stamp not increasing: {t} <= {last}");
                last = t;
            }
            prev = now;
        }
    }

    #[test]
    fn stamps_stay_monotonic_across_consecutive_drains() {
        let mut s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        let mut last = 0u64;
        let mut wake = 5_000_000u64;
        for _ in 0..50 {
            let now = wake + 29 * MS / 10;
            for i in 0..3 {
                let t = s.stamp(wake, now, i);
                assert!(t >= last, "stamp went backwards: {t} < {last}");
                last = t;
            }
            s.finish(wake, 3);
            wake += 3 * MS; // next report arrives 1ms after the last drained one
        }
    }

    #[test]
    fn the_interval_estimate_converges_on_a_periodic_stream() {
        // 1kHz mouse, 2ms coalescing that overshoots to ~3 reports per drain:
        // wakes are 3ms apart, drains hold 3 reports → 1ms spacing.
        let mut s = DrainStamper::new(2 * MS, 3 * MS, 20 * MS); // wrong prior
        let mut wake = 0u64;
        for _ in 0..40 {
            s.finish(wake, 3);
            wake += 3 * MS;
        }
        assert!(
            (s.step() as i64 - MS as i64).abs() <= MS as i64 / 20,
            "step {}",
            s.step()
        );

        // An 8kHz mouse: 24 reports per 3ms drain → 125µs spacing.
        let mut s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        let mut wake = 0u64;
        for _ in 0..40 {
            s.finish(wake, 24);
            wake += 3 * MS;
        }
        assert!((s.step() as i64 - 1250).abs() <= 100, "step {}", s.step());
    }

    #[test]
    fn a_pause_does_not_poison_the_estimate() {
        let mut s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        let mut wake = 0u64;
        for _ in 0..20 {
            s.finish(wake, 3);
            wake += 3 * MS;
        }
        let before = s.step();
        // The hand stopped for two seconds, then one report arrived.
        wake += 2_000 * MS;
        s.finish(wake, 1);
        assert_eq!(s.step(), before);
        // Empty drains (a hotkey wake) are not observations either.
        s.finish(wake + 5 * MS, 0);
        assert_eq!(s.step(), before);
    }

    #[test]
    fn the_estimate_is_bounded_by_the_coalescing_window() {
        let mut s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        // Two reports 15ms apart look like a 7.5ms interval; that cannot be
        // right for reports drained *together*, so it clamps to the window.
        s.finish(0, 2);
        s.finish(15 * MS, 2);
        assert!(s.step() <= 3 * MS);
        // And a burst of 200 reports in 1ms clamps at the floor, never 0.
        let mut s = DrainStamper::new(MS, 3 * MS, 20 * MS);
        for _ in 0..30 {
            s.finish(0, 200);
            s.finish(MS, 200);
        }
        assert!(s.step() >= 1);
    }
}
