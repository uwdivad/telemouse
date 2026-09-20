//! Windows version resource and application manifest for `telemouse.exe`.
//!
//! An executable with no file description, product name or version is the
//! antivirus heuristic profile for an unsigned program that reads raw input
//! (docs/ANTICHEAT-2026-09-14.md, F3); with the resource, Task Manager and
//! the file's Properties name it. The manifest (`telemouse.manifest` at the
//! workspace root) declares `asInvoker` — the agent never asks for elevation
//! — and per-monitor DPI awareness. A resource that fails to compile (no
//! Windows SDK `rc.exe`) is a warning, not a failed build: the binary works
//! without it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../telemouse.manifest");
    println!("cargo:rerun-if-changed=../../assets/telemouse.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "telemouse")
            .set("FileDescription", "telemouse raw mouse capture agent")
            .set("InternalName", "telemouse")
            .set("OriginalFilename", "telemouse.exe")
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
            println!("cargo:warning=telemouse.exe gets no version resource: {e}");
        }
    }
}
