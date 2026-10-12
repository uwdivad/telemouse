//! Windows version resource, application manifest and icon for
//! `telemouse-viz.exe`; see `build-support/resource.rs`.

include!("../../build-support/resource.rs");

fn main() {
    embed_resources(
        "telemouse-viz",
        "telemouse live visualization and replay server",
    );
}
