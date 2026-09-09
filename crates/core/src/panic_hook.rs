//! A panic hook that routes through `tracing`, so a panic on any thread of
//! any telemouse binary lands in the same log the operator is already
//! reading — the control panel's `<component>.log`, `ctl.log`, or the
//! console — instead of only on stderr, where a process started from
//! Explorer or the tray has nobody watching.
//!
//! The previous hook (the default one) still runs afterwards, so the usual
//! stderr message and `RUST_BACKTRACE` behaviour are unchanged.

use std::panic::PanicHookInfo;
use std::sync::Once;

static INSTALLED: Once = Once::new();

/// The message a panic carried, if it was a string. Pure, so the extraction
/// is testable without panicking.
pub fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

fn describe(info: &PanicHookInfo<'_>) -> (String, String, String) {
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    let location = info
        .location()
        .map(|l| format!("{}:{}", l.file(), l.line()))
        .unwrap_or_else(|| "<unknown>".to_string());
    (thread, location, payload_message(info.payload()))
}

/// Install the hook once per process. `component` names the binary in the
/// log line (`capture`, `viz`, `ctl`, `analyze`). Safe to call more than
/// once; only the first call installs.
pub fn install(component: &'static str) {
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let (thread, location, message) = describe(info);
            tracing::error!(
                component,
                thread = %thread,
                location = %location,
                panic = %message,
                "panic"
            );
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_messages_are_extracted_for_both_string_kinds() {
        let s: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(payload_message(&*s), "boom");
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("owned boom"));
        assert_eq!(payload_message(&*owned), "owned boom");
        let other: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(payload_message(&*other), "<non-string panic payload>");
    }

    #[test]
    fn installing_twice_is_harmless_and_the_default_hook_still_runs() {
        install("test");
        install("test");
        // The hook chains to the previous one, so a caught panic still
        // unwinds normally and the payload survives.
        let r = std::panic::catch_unwind(|| panic!("hook test"));
        let payload = r.unwrap_err();
        assert_eq!(payload_message(&*payload), "hook test");
    }
}
