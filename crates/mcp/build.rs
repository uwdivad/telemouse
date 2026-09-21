//! Windows version resource and application manifest for
//! `telemouse-mcp.exe`; see `crates/capture/build.rs` for why. `asInvoker`
//! matters here too: an MCP client launches this server as a child, and a
//! tool server that asked for elevation would be the wrong thing entirely.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../telemouse.manifest");
    println!("cargo:rerun-if-changed=../../assets/telemouse.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "telemouse")
            .set("FileDescription", "telemouse MCP server")
            .set("InternalName", "telemouse-mcp")
            .set("OriginalFilename", "telemouse-mcp.exe")
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
            println!("cargo:warning=telemouse-mcp.exe gets no version resource: {e}");
        }
    }
}
