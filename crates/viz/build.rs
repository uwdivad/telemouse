//! Windows version resource and application manifest for
//! `telemouse-viz.exe`; see `crates/capture/build.rs` for why.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../telemouse.manifest");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "telemouse")
            .set(
                "FileDescription",
                "telemouse live visualization and replay server",
            )
            .set("InternalName", "telemouse-viz")
            .set("OriginalFilename", "telemouse-viz.exe")
            .set("CompanyName", "github.com/uwdivad/telemouse")
            .set("LegalCopyright", "MIT License")
            .set_manifest(include_str!("../../telemouse.manifest"));
        if let Err(e) = res.compile() {
            println!("cargo:warning=telemouse-viz.exe gets no version resource: {e}");
        }
    }
}
