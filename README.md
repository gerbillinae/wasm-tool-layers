# wasm-tool-layers

Rust libraries for building a tool once and offering it as a CLI, an MCP server, a REST API with a web UI, and optionally a sandboxed WebAssembly component, all generated from one description of its commands.

The design and its reasoning are in [opinionated/optional/wasm.md](https://github.com/gerbillinae/opinionated/blob/main/optional/wasm.md). A complete example tool is anki-cards (`language-teacher/tools/anki-cards-rs`).

## The crates

| Crate | For | What it is |
|---|---|---|
| `tool-contract` | every tool | The command registry, `describe()` (schemas, routes, error categories), `invoke`, `ToolError`, `Context`, the `Backend` trait, HTTP route adaptors |
| `tool-ports` | every tool's core | The port traits the core reaches the world through: HTTP out, clock, secrets, questions, progress, timers, background work, change notification, serving, audio |
| `tool-cli` | hosts | A CLI generated at run time from `describe()`, plus custom commands |
| `tool-mcp` | hosts | MCP over stdio, generated from `describe()`, with changes since each session's last call |
| `tool-rest` | a tool's core | Its HTTP interface, generated from the registry: command routes, browser adapters (the page, state, event stream), the token check |
| `tool-serve` | hosts | The listening socket (hyper): localhost only, Origin and Host checks, forwarding with a "client gone" signal; the `serve` command |
| `tool-native` | native builds | The core on an owner thread behind a `Backend` handle, native port implementations, and the standard CLI (`run`) |
| `tool-wasi` | components | WASI 0.3 glue: an async HTTP client, and serving `tool-rest` as `wasi:http/handler` |
| `tool-files` | tool cores | Lock files and atomic writes over std::fs, for data a tool keeps in its granted directory; the same code natively and in a component |
| `tool-host` | WASM hosts | The generic host as a library (wasmtime): grants, the owner thread, typed command calls, suspend and resume, and extensions for custom capabilities |
| `wasm-tool` | users | `tool-host` with nothing added: runs any tool component |
| `tool-audio` | tools that play sound | A command-line player, as a native port and (feature `host`) as the `local:audio/player` WIT interface; `stub/` is a component that implements it as unavailable |

`wit/` holds the `opinionated:tool` WIT package (the contract between a component and its host), `local:audio`, and the WASI 0.3.0 packages they use.

## Using them

Each crate is its own dependency, from this one repository. Use the same tag for all of them, so they come from one checkout:

```toml
[dependencies]
tool-contract = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.1.0" }
tool-ports    = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.1.0" }
tool-rest     = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.1.0" }
tool-native   = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.1.0" }
```

A native tool's `main` is its ports plus one call:

```rust
let progress = tool_native::ports::RoutedProgress::default();
let p = progress.clone();
let backend = tool_native::start(move |services| Arc::new(MyTool::new(my_ports(p, services))));
tool_native::run(backend, progress, tool_native::App { name: "my-tool", instructions: None, commands: vec![] });
```

## Suspend and resume

A component that exports `opinionated:tool/lifecycle` can be suspended: the host discards the instance and later starts a fresh one from a snapshot the program chose to keep. The snapshot is opaque bytes; the program picks the format. postcard is the recommendation: compact binary serde, several times faster and smaller than JSON. Its types must write every field, so no `skip_serializing_if`. Either side can start it:

- **The program:** `host.request-suspend(snapshot)`, at any point.
- **The host:** `suspend(reason)`, with reason `idle`, `memory` or `exit`. The program accepts with a snapshot, or refuses with a reason, for example while audio plays.

The fresh instance is configured as before, then given the snapshot (`resume`) before any other call. The host's socket, MCP output and subscribers outlive instances. Host flags:

```
--suspend-idle SECS       ask the component to suspend after SECS without calls
--recycle-above-mb N      ask it to suspend when its memory passes N MiB (and twice a fresh instance's)
--snapshot FILE           resume from FILE, and ask it to suspend into FILE on exit
```

## Keeping data

A tool that keeps data of its own gets a directory: `Ports::data_dir`. A native tool picks one (`tool_native::ports::default_data_dir`), and a WASM host grants one with `--data-dir DIR`, which the component sees as `/data`. The tool reads and writes it with plain std::fs and decides its own format and locking. Several processes may share the directory: two MCP servers, a CLI run, or the native build next to the component. wasi:filesystem has no advisory locks, so `tool-files` builds lock files from exclusive create, with atomic writes by rename. A lock waits by the tool's own timer, so a holder in the same process carries on while others wait.

Use a self-describing format for data that outlives builds, such as JSON with serde defaults, and keep postcard for snapshots.

## Versions

The WASM side tracks wasmtime closely: wasmtime 50 (Rust 1.97 or later), WASI 0.3.0, and so `wasip3` 0.9 and wit-bindgen 0.62 in components. A component and its host must agree on the WASI version.

## Status

Extracted from the anki-cards port, where every piece is used and tested. The APIs are young and will change.
