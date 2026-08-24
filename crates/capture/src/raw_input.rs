//! T1 — the capture hot path.
//!
//! A message-only window (`HWND_MESSAGE` parent) registered for the mouse usage
//! page with `RIDEV_INPUTSINK`, so deltas keep arriving while a game holds the
//! foreground. On `WM_INPUT` we timestamp with QPC, decode, and push a
//! fixed-size [`RawEvent`] into the SPSC ring — no allocation, no locks, no
//! blocking. A full ring bumps a counter and the event is dropped: capture
//! never stalls.
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
/// stays 0 and `PostThreadMessageW(WM_QUIT)` still unblocks `GetMessageW`, so
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

/// Lets T1 wake T2 when the ring goes empty → non-empty, so T2 can park
/// instead of polling every few milliseconds.
///
/// The hot path pays exactly one relaxed load per event. That load can, in
/// principle, observe a stale `false` while T2 is on its way into `park`
/// (x86 allows the store-then-load reordering on T2's side); T2's park timeout
/// is the backstop, so the worst case is a slightly late flush, never a stall.
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

    /// T1 hot path: wake T2 if (and only if) it is parked.
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

#[cfg(windows)]
pub use win::{CaptureDeps, Hotkey, post_quit, post_thread_quit, run};

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::Sender;

    use anyhow::{Context, Result};
    use telemouse_core::RawEvent;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_F9,
    };
    use windows::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
        RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
        GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, MSG, PostMessageW, PostQuitMessage,
        PostThreadMessageW, RegisterClassW, SetWindowLongPtrW, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_APP, WM_DESTROY, WM_DISPLAYCHANGE, WM_HOTKEY, WM_INPUT, WM_QUIT, WNDCLASSW,
    };
    use windows::core::{PCWSTR, w};

    use super::{CaptureHandles, Decoded, RingWaker, decode_mouse};
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
                // Timestamp first: everything after this is decode overhead.
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

    /// The hot path. No allocation, no locking, no fallible I/O.
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

    /// Run the capture thread: create the window, register raw input and the
    /// marker hotkey, then pump messages until asked to quit.
    ///
    /// The thread id and window handle are published through
    /// [`CaptureHandles`] as soon as they exist, so shutdown always has a way
    /// to break the message loop.
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
        } = deps;
        handles.set_thread_id(unsafe { GetCurrentThreadId() });
        platform::raise_capture_thread_priority();

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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut CaptureState as isize);

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

            tracing::info!("raw input capture running");
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 == 0 {
                    break; // WM_QUIT
                }
                if r.0 == -1 {
                    let e = windows::core::Error::from_thread();
                    tracing::error!(error = %e, "GetMessage failed; stopping capture");
                    break;
                }
                let _ = DispatchMessageW(&msg);
            }

            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
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
            decode_mouse(MOUSE_MOVE_ABSOLUTE | 0x02, buttons::LEFT_DOWN, 0, 1, 1, 1, 2),
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
}
