//! Windows version resource, application manifest and icon for
//! `telemouse-mcp.exe`; see `build-support/resource.rs`. `asInvoker`
//! matters here too: an MCP client launches this server as a child, and a
//! tool server that asked for elevation would be the wrong thing entirely.

include!("../../build-support/resource.rs");

fn main() {
    embed_resources("telemouse-mcp", "telemouse MCP server");
}
