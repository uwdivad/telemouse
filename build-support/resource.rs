// Windows version resource, application manifest and icon for every
// telemouse executable. Each binary crate's build.rs pulls this file in with
// `include!("../../build-support/resource.rs")` and calls `embed_resources`
// with its own name and description; it is not a crate of its own, so it
// carries `//` comments only (an included file cannot hold `//!`).
//
// An executable with no file description, product name or version is the
// antivirus heuristic profile for an unsigned program that reads raw input
// (docs/ANTICHEAT-2026-09-14.md, F3); with the resource, Task Manager and
// the file's Properties name it. The manifest (`telemouse.manifest` at the
// workspace root) declares `asInvoker` (no telemouse binary ever asks for
// elevation) and per-monitor DPI awareness.
//
// Nothing here fails the build: a missing manifest or icon, a non-Windows
// target, or a resource that does not compile (no Windows SDK `rc.exe`) is a
// `cargo:warning` at most, and the binary works without the resource.

/// Embed the version resource, manifest and icon into the executable
/// `<internal_name>.exe` of the crate whose build script calls this.
fn embed_resources(internal_name: &str, description: &str) {
    let crate_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let root = crate_dir.join("..").join("..");
    let shared = root.join("build-support").join("resource.rs");
    let manifest = root.join("telemouse.manifest");
    let icon = root.join("assets").join("telemouse.ico");
    println!("cargo:rerun-if-changed=build.rs");
    for path in [&shared, &manifest, &icon] {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    // `#[cfg(windows)]` in a build script tests the HOST; what matters is the
    // target the executable is built for.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // winresource is a host-Windows build-dependency (it drives `rc.exe`).
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "telemouse")
            .set("FileDescription", description)
            .set("InternalName", internal_name)
            .set("OriginalFilename", &format!("{internal_name}.exe"))
            .set("CompanyName", "github.com/uwdivad/telemouse")
            .set(
                "LegalCopyright",
                "Copyright (c) telemouse contributors. MIT License",
            );
        match std::fs::read_to_string(&manifest) {
            Ok(text) => {
                res.set_manifest(&text);
            }
            Err(e) => println!(
                "cargo:warning={internal_name}.exe gets no application manifest ({}: {e})",
                manifest.display()
            ),
        }
        // The application icon (assets/make-icon.ps1 draws it). A checkout
        // without it still gets the version resource and the manifest.
        if icon.exists() {
            res.set_icon(&icon.to_string_lossy());
        }
        if let Err(e) = res.compile() {
            println!("cargo:warning={internal_name}.exe gets no version resource: {e}");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = description;
        println!(
            "cargo:warning={internal_name}.exe gets no version resource: \
             building for Windows from a non-Windows host"
        );
    }
}
