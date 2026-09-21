//! The panel page inside the window: a WebView2 control filling the client
//! area, navigated to the panel's own URL.
//!
//! WebView2 is COM on a single-threaded apartment: every object is created
//! on the `ctl-gui` thread and every callback arrives there as a posted
//! message, so the existing `GetMessageW` loop in `win.rs` is the only loop
//! involved. Creation is asynchronous in two hops (environment, then
//! controller); each hop's completion handler captures the window handle,
//! never the state pointer, and reaches the state through `GWLP_USERDATA`
//! exactly as `wndproc` does — a callback that fires after the window is
//! gone finds nothing and returns.
//!
//! While the window is in the tray the page is suspended
//! (`ICoreWebView2_3::TrySuspend`, decided by the state machine in
//! `model::power_*`) so that a panel nobody is looking at costs nothing; a
//! runtime too old for that interface falls back to what this did before,
//! `SetIsVisible(false)` alone.
//!
//! Every failure ends in [`fallback`]: the read-only `EDIT` text view is
//! shown with a banner saying why, and the page is opened in the default
//! browser once. Nothing here panics and nothing blocks the thread; a
//! runtime that never answers is caught by a watchdog timer.
//!
//! Anti-cheat posture: WebView2 spawns Microsoft-signed `msedgewebview2.exe`
//! children that render into our ordinary window; this process opens no
//! handle on any other process, hooks nothing, and the window stays a plain
//! `WS_OVERLAPPEDWINDOW`.

use tracing::{debug, info, warn};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_COLOR, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC, COREWEBVIEW2_WEB_ERROR_STATUS,
    CreateCoreWebView2EnvironmentWithOptions, GetAvailableCoreWebView2BrowserVersionString,
    ICoreWebView2, ICoreWebView2_3, ICoreWebView2Controller, ICoreWebView2Controller2,
    ICoreWebView2Environment,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler, NavigationCompletedEventHandler, NewWindowRequestedEventHandler,
    TrySuspendCompletedHandler, take_pwstr,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, KillTimer, SetTimer};
use windows::core::{BOOL, Interface, PCWSTR, PWSTR};

use super::model::{self, PowerAction, PowerState, WebStatus};
use super::win::{self, UiState};

/// `SetTimer` id: the environment or controller never answered.
pub(super) const TIMER_WATCHDOG: usize = 1;
/// `SetTimer` id: navigate again after a refused connection.
pub(super) const TIMER_NAV_RETRY: usize = 2;
const WATCHDOG_MS: u32 = 10_000;
const NAV_RETRY_MS: u32 = 500;
/// Six tries half a second apart: the listener is bound before this window
/// exists, so a refused connection means something else is wrong.
const NAV_RETRIES: u8 = 6;

/// The page's own `--bg`, painted before the first frame so the window
/// never flashes white.
const BACKGROUND: COREWEBVIEW2_COLOR = COREWEBVIEW2_COLOR {
    A: 255,
    R: 0x0b,
    G: 0x0f,
    B: 0x16,
};

/// Told to the page when the window goes to the tray. `SetIsVisible(false)`
/// does not make the document `hidden`, so the page cannot notice by itself
/// that nobody is looking; these two hooks are how it is told. A page that
/// does not have them (an older build behind a newer window) ignores both.
const SLEEP_SCRIPT: &str = "window.telemouseSleep && window.telemouseSleep()";
/// Told to the page when the window comes back: refresh now, then poll as
/// usual. Without it the first thing on screen could be a poll period old,
/// because a suspended timer resumes where it stopped.
const WAKE_SCRIPT: &str = "window.telemouseWake && window.telemouseWake()";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// `begin` has not run.
    Idle,
    /// The environment or the controller is being created.
    Creating,
    /// The page is on screen.
    Hosted,
    /// The text view is the window.
    Fallback(String),
}

/// Everything the window holds about its embedded page.
pub(super) struct WebHost {
    pub(super) phase: Phase,
    controller: Option<ICoreWebView2Controller>,
    webview: Option<ICoreWebView2>,
    /// The same object as `webview`, through the interface that has
    /// `TrySuspend` / `Resume`. `None` on a runtime older than that
    /// (WebView2 1.0.864), which is then never suspended.
    suspend: Option<ICoreWebView2_3>,
    /// What the page costs while the window is hidden.
    power: PowerState,
    nav_retries: u8,
    /// The first successful navigation was logged.
    page_shown: bool,
    /// The default browser was opened as the fallback (at most once).
    browser_opened: bool,
}

impl WebHost {
    pub(super) fn new() -> Self {
        Self {
            phase: Phase::Idle,
            controller: None,
            webview: None,
            suspend: None,
            power: PowerState::Visible,
            nav_retries: 0,
            page_shown: false,
            browser_opened: false,
        }
    }

    /// What the text view says about this host.
    pub(super) fn status(&self) -> WebStatus {
        match &self.phase {
            Phase::Idle | Phase::Creating => WebStatus::Loading,
            Phase::Hosted => WebStatus::Hosted,
            Phase::Fallback(reason) => WebStatus::Fallback {
                reason: reason.clone(),
                browser_opened: self.browser_opened,
            },
        }
    }

    pub(super) fn hosted(&self) -> bool {
        self.phase == Phase::Hosted
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The page, told it is inside the window (`?hosted=1`): it then shows
/// "Open in browser" and nothing else differs.
fn hosted_url(panel_url: &str) -> String {
    format!("{panel_url}?hosted=1")
}

/// Probe the runtime and start creating the page. Called once, after the
/// window is on screen; everything else follows from callbacks.
pub(super) unsafe fn begin(s: *mut UiState) {
    // SAFETY: `s` is the live state on this thread; every Win32/COM call is
    // made with plain copies, never across a `&mut`.
    unsafe {
        let hwnd = (*s).hwnd;
        if (*s).link.no_webview {
            fallback(s, "disabled with --no-webview", false);
            return;
        }
        let Some(dir) = (*s).link.webview_data_dir.clone() else {
            fallback(
                s,
                "no writable folder for its data (LOCALAPPDATA and TEMP are unset)",
                true,
            );
            return;
        };

        let mut version = PWSTR::null();
        match GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) {
            Ok(()) if !version.is_null() => {
                let v = take_pwstr(version);
                info!(runtime = %v, "webview2 runtime found");
            }
            Ok(()) => {
                fallback(s, "the WebView2 Runtime is not installed", true);
                return;
            }
            Err(e) => {
                debug!(error = %e, "GetAvailableCoreWebView2BrowserVersionString");
                fallback(s, "the WebView2 Runtime is not installed", true);
                return;
            }
        }

        let udf = wide(&dir.display().to_string());
        let handler =
            CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |hr, env| {
                on_environment(hwnd, hr, env);
                Ok(())
            }));
        // Phase and watchdog first: a loader that fails at once invokes the
        // handler before returning, and the handler must find `Creating`.
        (*s).web.phase = Phase::Creating;
        SetTimer(Some(hwnd), TIMER_WATCHDOG, WATCHDOG_MS, None);
        debug!(data = %dir.display(), "webview2 environment requested");
        if let Err(e) = CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            PCWSTR(udf.as_ptr()),
            None,
            &handler,
        ) && (*s).web.phase == Phase::Creating
        {
            fallback(s, format!("the WebView2 environment failed: {e}"), true);
        }
    }
}

/// Hop one: the environment exists (or not); ask it for a controller
/// parented to our window.
unsafe fn on_environment(
    hwnd: HWND,
    hr: windows::core::Result<()>,
    env: Option<ICoreWebView2Environment>,
) {
    // SAFETY: on the UI thread, reached through GWLP_USERDATA like wndproc.
    unsafe {
        debug!(result = ?hr, has_env = env.is_some(), "webview2 environment callback");
        let s = win::state_of(hwnd);
        if s.is_null() || (*s).web.phase != Phase::Creating {
            return; // the window is gone, or the watchdog already gave up
        }
        let env = match (hr, env) {
            (Ok(()), Some(env)) => env,
            (Err(e), _) => {
                fallback(s, format!("the WebView2 environment failed: {e}"), true);
                return;
            }
            (Ok(()), None) => {
                fallback(s, "the WebView2 environment came back empty", true);
                return;
            }
        };
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
            move |hr, controller| {
                on_controller(hwnd, hr, controller);
                Ok(())
            },
        ));
        if let Err(e) = env.CreateCoreWebView2Controller(hwnd, &handler) {
            fallback(s, format!("the WebView2 controller failed: {e}"), true);
        }
    }
}

/// Hop two: the controller exists; size it, tune the settings, wire the
/// two events that matter, and navigate.
unsafe fn on_controller(
    hwnd: HWND,
    hr: windows::core::Result<()>,
    controller: Option<ICoreWebView2Controller>,
) {
    // SAFETY: as `on_environment`.
    unsafe {
        debug!(result = ?hr, has_controller = controller.is_some(), "webview2 controller callback");
        let s = win::state_of(hwnd);
        if s.is_null() || (*s).web.phase != Phase::Creating {
            return;
        }
        let controller = match (hr, controller) {
            (Ok(()), Some(c)) => c,
            (Err(e), _) => {
                fallback(s, format!("the WebView2 controller failed: {e}"), true);
                return;
            }
            (Ok(()), None) => {
                fallback(s, "the WebView2 controller came back empty", true);
                return;
            }
        };
        if let Err(e) = attach(s, hwnd, &controller) {
            let _ = controller.Close();
            fallback(
                s,
                format!("the WebView2 page could not be set up: {e}"),
                true,
            );
        }
    }
}

unsafe fn attach(
    s: *mut UiState,
    hwnd: HWND,
    controller: &ICoreWebView2Controller,
) -> windows::core::Result<()> {
    // SAFETY: COM calls on this thread's objects; the state is read field
    // by field.
    unsafe {
        // Older runtimes lack Controller2; a white first frame is then the
        // only cost.
        if let Ok(c2) = controller.cast::<ICoreWebView2Controller2>() {
            let _ = c2.SetDefaultBackgroundColor(BACKGROUND);
        }
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc)?;
        controller.SetBounds(rc)?;
        controller.SetIsVisible((*s).visible)?;

        let wv = controller.CoreWebView2()?;
        let st = wv.Settings()?;
        // It is a panel, not a browser: no context menu, no zoom, no status
        // bar, no host objects, no messages, and no built-in error page (a
        // refused connection is retried and then reported by us).
        let _ = st.SetAreDefaultContextMenusEnabled(false);
        let _ = st.SetIsStatusBarEnabled(false);
        let _ = st.SetIsZoomControlEnabled(false);
        let _ = st.SetAreHostObjectsAllowed(false);
        let _ = st.SetIsWebMessageEnabled(false);
        let _ = st.SetAreDevToolsEnabled(cfg!(debug_assertions));
        let _ = st.SetIsBuiltInErrorPageEnabled(false);

        // Every link the page opens in a new tab (the dashboard, the docs,
        // the releases) goes to the system browser, not a second WebView2.
        let mut token = 0i64;
        let new_window = NewWindowRequestedEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                let _ = args.SetHandled(true);
                let mut uri = PWSTR::null();
                if args.Uri(&mut uri).is_ok() {
                    let uri = take_pwstr(uri);
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        win::shell_open(&uri);
                    } else {
                        debug!(uri, "new-window request ignored");
                    }
                }
            }
            Ok(())
        }));
        wv.add_NewWindowRequested(&new_window, &mut token)?;

        let navigated = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            on_navigated(hwnd, args);
            Ok(())
        }));
        wv.add_NavigationCompleted(&navigated, &mut token)?;

        let url = wide(&hosted_url(&(*s).link.panel_url));
        wv.Navigate(PCWSTR(url.as_ptr()))?;

        // The interface that can suspend the page while it is in the tray.
        // An older runtime simply does not have it.
        let suspend = wv.cast::<ICoreWebView2_3>().ok();
        if suspend.is_none() {
            info!(
                "this WebView2 runtime cannot suspend a hidden page; it will keep polling in the tray"
            );
        }
        (*s).web.suspend = suspend;
        (*s).web.power = if (*s).visible {
            PowerState::Visible
        } else {
            // Hidden before the page was ready: `on_navigated` offers it to
            // the runtime again once the first navigation succeeds.
            PowerState::HiddenAwake
        };
        (*s).web.controller = Some(controller.clone());
        (*s).web.webview = Some(wv);
        (*s).web.phase = Phase::Hosted;
        let _ = KillTimer(Some(hwnd), TIMER_WATCHDOG);
        win::hide_text_view(s);
        Ok(())
    }
}

/// A navigation finished. Success is logged once; a refused connection is
/// retried a few times, then the text view takes over.
unsafe fn on_navigated(
    hwnd: HWND,
    args: Option<
        webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2NavigationCompletedEventArgs,
    >,
) {
    // SAFETY: as `on_environment`.
    unsafe {
        let s = win::state_of(hwnd);
        if s.is_null() || (*s).web.phase != Phase::Hosted {
            return;
        }
        let Some(args) = args else { return };
        let mut ok = BOOL(0);
        let _ = args.IsSuccess(&mut ok);
        if ok.as_bool() {
            (*s).web.nav_retries = 0;
            if !(*s).web.page_shown {
                (*s).web.page_shown = true;
                info!(url = %(*s).link.panel_url, "panel page hosted in the window");
            }
            // The window went to the tray while this navigation was still
            // running (or an earlier suspension was refused because of it):
            // a loaded page can be suspended, so ask again.
            if !(*s).visible {
                power(s, model::power_hide((*s).web.power, can_suspend(s)));
            }
            return;
        }
        let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
        let _ = args.WebErrorStatus(&mut status);
        if (*s).web.nav_retries < NAV_RETRIES {
            (*s).web.nav_retries += 1;
            debug!(
                status = status.0,
                attempt = (*s).web.nav_retries,
                "panel navigation failed; retrying"
            );
            SetTimer(Some(hwnd), TIMER_NAV_RETRY, NAV_RETRY_MS, None);
        } else {
            fallback(
                s,
                format!(
                    "the panel's own HTTP server did not answer (web error status {})",
                    status.0
                ),
                true,
            );
        }
    }
}

/// `WM_TIMER` for one of our ids.
pub(super) unsafe fn timer(s: *mut UiState, id: usize) {
    // SAFETY: `s` is live.
    unsafe {
        let hwnd = (*s).hwnd;
        match id {
            TIMER_WATCHDOG => {
                let _ = KillTimer(Some(hwnd), TIMER_WATCHDOG);
                if (*s).web.phase == Phase::Creating {
                    fallback(s, "the WebView2 Runtime did not answer in 10 s", true);
                }
            }
            TIMER_NAV_RETRY => {
                let _ = KillTimer(Some(hwnd), TIMER_NAV_RETRY);
                if let Some(wv) = (*s).web.webview.clone() {
                    let url = wide(&hosted_url(&(*s).link.panel_url));
                    if let Err(e) = wv.Navigate(PCWSTR(url.as_ptr())) {
                        fallback(s, format!("navigation failed: {e}"), true);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The client area changed: the page fills it.
pub(super) unsafe fn resize(s: *mut UiState, width: i32, height: i32) {
    // SAFETY: `s` is live; the controller is a cloned COM handle.
    unsafe {
        if let Some(c) = (*s).web.controller.clone() {
            let _ = c.SetBounds(RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            });
        }
    }
}

/// The window moved: popups and accelerators follow it.
pub(super) unsafe fn position_changed(s: *mut UiState) {
    // SAFETY: as `resize`.
    unsafe {
        if let Some(c) = (*s).web.controller.clone() {
            let _ = c.NotifyParentWindowPositionChanged();
        }
    }
}

/// Hidden to the tray: the page stops rendering, stops polling and — on a
/// runtime that can — is suspended, so the window costs nothing while a game
/// is in front. Shown: resumed, focused, and refreshed at once so nothing on
/// screen is a poll period old. Every show/hide path (tray click, tray menu,
/// the close button, minimise) comes through `win::set_visible` into here.
pub(super) unsafe fn set_visible(s: *mut UiState, show: bool) {
    // SAFETY: as `resize`.
    unsafe {
        let next = if show {
            model::power_show((*s).web.power)
        } else {
            model::power_hide((*s).web.power, can_suspend(s))
        };
        power(s, next);
    }
}

/// Whether the page may be suspended: a runtime that has `ICoreWebView2_3`
/// and a page that has actually loaded (suspending one mid-navigation is
/// refused, and would freeze the retry).
unsafe fn can_suspend(s: *mut UiState) -> bool {
    // SAFETY: plain field reads on the live state.
    unsafe { (*s).web.suspend.is_some() && (*s).web.page_shown }
}

/// Record a decision from `model` and carry it out.
unsafe fn power(s: *mut UiState, next: (PowerState, PowerAction)) {
    // SAFETY: `s` is live; COM objects are cloned handles.
    unsafe {
        let (state, action) = next;
        if state != (*s).web.power {
            debug!(?state, "webview2 power state");
        }
        (*s).web.power = state;
        let Some(c) = (*s).web.controller.clone() else {
            return; // the text view is the window; nothing to suspend
        };
        match action {
            PowerAction::Nothing => {}
            PowerAction::Hide | PowerAction::HideAndSuspend => {
                run_script(s, SLEEP_SCRIPT);
                let _ = c.SetIsVisible(false);
                // TrySuspend is refused while the controller is visible,
                // which is why it comes after.
                if action == PowerAction::HideAndSuspend {
                    try_suspend(s);
                }
            }
            PowerAction::Show { resume } => {
                if resume {
                    resume_page(s);
                }
                let _ = c.SetIsVisible(true);
                let _ = c.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                run_script(s, WAKE_SCRIPT);
            }
            PowerAction::Resume => resume_page(s),
        }
    }
}

/// Ask the runtime to suspend the page. The answer arrives on this thread's
/// message loop like every other WebView2 completion; nothing waits for it.
unsafe fn try_suspend(s: *mut UiState) {
    // SAFETY: `s` is live; the handler captures only the window handle.
    unsafe {
        let hwnd = (*s).hwnd;
        let Some(w3) = (*s).web.suspend.clone() else {
            return;
        };
        let handler = TrySuspendCompletedHandler::create(Box::new(move |hr, ok| {
            on_suspended(hwnd, hr, ok);
            Ok(())
        }));
        if let Err(e) = w3.TrySuspend(&handler) {
            debug!(error = %e, "TrySuspend refused");
            suspend_answered(s, false);
        }
    }
}

/// `TrySuspend` finished: `ok` is false when the runtime decided the page
/// was busy, which is a normal answer and not an error.
unsafe fn on_suspended(hwnd: HWND, hr: windows::core::Result<()>, ok: bool) {
    // SAFETY: as `on_environment`.
    unsafe {
        let s = win::state_of(hwnd);
        if s.is_null() {
            return;
        }
        if let Err(e) = &hr {
            debug!(error = %e, "TrySuspend failed");
        }
        suspend_answered(s, hr.is_ok() && ok);
    }
}

unsafe fn suspend_answered(s: *mut UiState, ok: bool) {
    // SAFETY: `s` is live.
    unsafe {
        power(s, model::power_suspended((*s).web.power, ok));
    }
}

unsafe fn resume_page(s: *mut UiState) {
    // SAFETY: `s` is live; `Resume` on a page that is not suspended is a
    // no-op the runtime reports, and reporting it is all we do.
    unsafe {
        if let Some(w3) = (*s).web.suspend.clone()
            && let Err(e) = w3.Resume()
        {
            debug!(error = %e, "webview2 resume");
        }
    }
}

/// Run one of the two hook scripts in the page. Fire and forget: the result
/// is only ever a log line, and a page without the hook evaluates to
/// `undefined`.
unsafe fn run_script(s: *mut UiState, script: &str) {
    // SAFETY: `s` is live; the wide buffer outlives the call, which copies
    // the script before returning.
    unsafe {
        let Some(wv) = (*s).web.webview.clone() else {
            return;
        };
        if !(*s).web.page_shown {
            return; // nothing has loaded that could have the hooks
        }
        let js = wide(script);
        let handler = ExecuteScriptCompletedHandler::create(Box::new(|hr, _result| {
            if let Err(e) = hr {
                debug!(error = %e, "page hook script");
            }
            Ok(())
        }));
        if let Err(e) = wv.ExecuteScript(PCWSTR(js.as_ptr()), &handler) {
            debug!(error = %e, "ExecuteScript refused");
        }
    }
}

/// Release the page before the window is destroyed.
pub(super) unsafe fn close(s: *mut UiState) {
    // SAFETY: `s` is live; this is the last use of the COM objects.
    unsafe {
        let hwnd = (*s).hwnd;
        let _ = KillTimer(Some(hwnd), TIMER_WATCHDOG);
        let _ = KillTimer(Some(hwnd), TIMER_NAV_RETRY);
        (*s).web.webview = None;
        (*s).web.suspend = None;
        (*s).web.power = PowerState::Visible;
        if let Some(c) = (*s).web.controller.take() {
            let _ = c.Close();
        }
    }
}

/// Give up on the page: text view with a banner, the browser opened once
/// (unless the user asked for no page, `--no-webview`).
pub(super) unsafe fn fallback(s: *mut UiState, reason: impl Into<String>, open_browser: bool) {
    // SAFETY: `s` is live; COM objects are released before any Win32 call
    // that could re-enter wndproc.
    unsafe {
        let reason = reason.into();
        close(s);
        (*s).web.phase = Phase::Fallback(reason.clone());
        if open_browser && !(*s).web.browser_opened {
            (*s).web.browser_opened = true;
            let url = (*s).link.panel_url.clone();
            if !win::shell_open(&url) {
                (*s).web.browser_opened = false;
            }
        }
        if open_browser {
            warn!(%reason, "panel page not hosted in the window; the text view and the browser stand in");
        } else {
            info!(%reason, "panel page not hosted in the window; text view only");
        }
        win::show_text_view(s);
    }
}
