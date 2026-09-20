//! Windows version resource and application manifest for
//! `telemouse-ctl.exe`; see `crates/capture/build.rs` for why. The manifest's
//! `asInvoker` matters most here: the panel launches every other component,
//! so an elevated panel would mean an elevated capture agent.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../telemouse.manifest");
    println!("cargo:rerun-if-changed=../../assets/telemouse.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "telemouse")
            .set("FileDescription", "telemouse control panel")
            .set("InternalName", "telemouse-ctl")
            .set("OriginalFilename", "telemouse-ctl.exe")
            .set("CompanyName", "github.com/uwdivad/telemouse")
            .set("LegalCopyright", "MIT License")
            .set_manifest(include_str!("../../telemouse.manifest"));
        // The application icon (assets/make-icon.ps1 draws it). A checkout
        // without it still gets the version resource and the manifest.
        let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/telemouse.ico");
        if std::path::Path::new(icon).exists() {
            res.set_icon(icon);
        }
        if let Err(e) = res.compile() {
            println!("cargo:warning=telemouse-ctl.exe gets no version resource: {e}");
        }
    }
}
