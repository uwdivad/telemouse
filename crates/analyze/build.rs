//! Windows version resource and application manifest for
//! `telemouse-analyze.exe`; see `crates/capture/build.rs` for why.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../telemouse.manifest");
    println!("cargo:rerun-if-changed=../../assets/telemouse.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "telemouse")
            .set("FileDescription", "telemouse offline aim-metrics analyzer")
            .set("InternalName", "telemouse-analyze")
            .set("OriginalFilename", "telemouse-analyze.exe")
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
            println!("cargo:warning=telemouse-analyze.exe gets no version resource: {e}");
        }
    }
}
