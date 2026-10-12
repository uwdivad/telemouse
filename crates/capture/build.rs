//! Windows version resource, application manifest and icon for
//! `telemouse.exe`. The shared code explains why every binary carries one.

include!("../../build-support/resource.rs");

fn main() {
    embed_resources("telemouse", "telemouse raw mouse capture agent");
}
