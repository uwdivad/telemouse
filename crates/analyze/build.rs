//! Windows version resource, application manifest and icon for
//! `telemouse-analyze.exe`; see `build-support/resource.rs`.

include!("../../build-support/resource.rs");

fn main() {
    embed_resources(
        "telemouse-analyze",
        "telemouse offline aim-metrics analyzer",
    );
}
