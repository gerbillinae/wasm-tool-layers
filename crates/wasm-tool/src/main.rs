//! wasm-tool: the generic host with nothing added. It runs any component
//! implementing opinionated:tool (wit/tool.wit) with the generated CLI, or
//! as an MCP server over stdio.
//!
//!   wasm-tool [--allow-http HOST[:PORT]]... [--secret NAME]... COMPONENT.wasm COMMAND [ARGS]

fn main() {
    tool_host::run(tool_host::Extension::none("wasm-tool"))
}
