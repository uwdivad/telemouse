//! Windows version resource, application manifest and icon for
//! `telemouse-ctl.exe`; see `build-support/resource.rs`. The manifest's
//! `asInvoker` matters most here: the panel launches every other component,
//! so an elevated panel would mean an elevated capture agent.

include!("../../build-support/resource.rs");

fn main() {
    embed_resources("telemouse-ctl", "telemouse control panel");
}
