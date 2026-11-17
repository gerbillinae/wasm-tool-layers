# wasm-tool-layers

Rust libraries for building a tool once and offering it as a CLI, an MCP server, a REST API with a web UI, and optionally a sandboxed WebAssembly component, all generated from one description of its commands. The native build and the component run the same code the same way, and a conformance suite checks that they do.

The design and its reasoning are in [opinionated/optional/wasm.md](https://github.com/gerbillinae/opinionated/blob/main/optional/wasm.md). A complete example tool is [anki-cards-rs](https://github.com/gerbillinae/anki-cards-rs).

## The shape

```
interfaces   CLI · MCP · REST · TUI           generated from describe(), the same in both runtimes
seam         Backend                          what every interface talks to
adapter      tool-native  |  tool-host + export_tool!
instance     ToolInstance                     implemented once, in the tool's core
core         registry · library · tool-rest   the same source on both targets
ports        native  |  WASI and host imports the only place the runtimes differ
```

A tool implements `ToolInstance` once. `tool-native` runs it in process; `tool_wasi::export_tool!` exports it as a component, which `tool-host` runs. Both give the same extension shape, the same lifecycle, the same data directory and the same answer to a crash.

## The crates

| Crate | For | What it is |
|---|---|---|
| `tool-contract` | every tool | The command registry, `describe()` (schemas, routes, errors, features, declared needs), `invoke`, `ToolError`, `Context`, `ToolInstance`, the `Backend` trait, route adaptors, and `conformance` |
| `tool-ports` | every tool's core | The port traits the core reaches the world through: HTTP out, clock, secrets, questions, progress, timers, background work, change notification, serving, audio, and the data directory |
| `tool-cli` | hosts | A CLI generated at run time from `describe()`, plus custom commands |
| `tool-mcp` | hosts | MCP over stdio, generated from `describe()`, with changes since each session's last call |
| `tool-rest` | a tool's core | Its HTTP interface, generated from the registry: command routes, browser adapters (the page, state, event stream), the token check |
| `tool-serve` | hosts | The listening socket (hyper): localhost only, Origin and Host checks, forwarding with a "client gone" signal; the `serve` command |
| `tool-native` | native builds | A `ToolInstance` on an owner thread behind a `Backend` handle, native ports (checked against the declared needs), and the standard CLI (`run`) |
| `tool-wasi` | components | `export_tool!` (a `ToolInstance` as the component, with its ports), WASI ports, an async HTTP client, and serving `tool-rest` as `wasi:http/handler` |
| `tool-files` | tool cores | Lock files and atomic writes over std::fs, for data a tool keeps in its directory; the same code natively and in a component |
| `tool-host` | WASM hosts | The generic host as a library (wasmtime): grants, the owner thread, suspend and resume, the compilation cache, and extensions for custom capabilities |
| `wasm-tool` | users | `tool-host` with nothing added: runs any tool component |
| `tool-audio` | tools that play sound | A command-line player, as a native port and (feature `host`) as the `local:audio/player` WIT interface; `stub/` is a component that implements it as unavailable |

`wit/` holds the `opinionated:tool` WIT package (the contract between a component and its host), `local:audio`, and the WASI 0.3.0 packages they use.

## Using them

Each crate is its own dependency, from this one repository. Use the same tag for all of them, so they come from one checkout:

```toml
[dependencies]
tool-contract = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.3.0" }
tool-ports    = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.3.0" }
tool-native   = { git = "https://github.com/gerbillinae/wasm-tool-layers", tag = "v0.3.0" }
```

The core implements the instance contract once:

```rust
impl tool_contract::ToolInstance for MyTool {
    fn describe(&self) -> Description { REGISTRY.describe(self) }
    fn configure(&self, settings: Map<String, Value>) -> Result<(), ToolError> { … }
    fn invoke(self: Arc<Self>, cx: &Context, command: &str, input: Value) -> LocalFuture<Result<Value, ToolError>> {
        REGISTRY.invoke(self, cx, command, input)
    }
    // optional: state, shutdown, http, suspend, resume, memory_limit
}
```

The native `main` is its ports plus one call:

```rust
tool_native::run(App { name: "my-tool", instructions: None, commands: Box::new(|_desc| vec![]) }, |services| {
    MyTool::new(Ports { http: Box::new(UreqHttp::checked(services.declared.clone())), data_dir: services.data_dir, … })
})
```

The component is its own WIT world (including `opinionated:tool/tool-component`) plus one macro:

```rust
wit_bindgen::generate!({ world: "my:tool/component", path: "wit", with: { … }, generate_all });
tool_wasi::export_tool!(Component, MyTool, || MyTool::new(host_ports()));
```

### Or skip the native build

A tool can ship as its component alone, in one binary: a build script compiles the component (a second cargo, its own target directory), the bytes are embedded, and `tool_host::run_embedded` runs them with the tool's own command line (`TOOL COMMAND`, no component path). Its default grants (typically `--grant-declared` and its extension's flags) apply unless `--no-default-grants`; `--host-help` lists the host's flags. The core is then only ever built for WASM. anki-cards-rs's [`wasm-only` branch](https://github.com/gerbillinae/anki-cards-rs/tree/wasm-only) does this.

```rust
tool_host::run_embedded(extension, tool_host::Embedded {
    wasm: my_tool_wasm::WASM,                 // include_bytes! of what build.rs built
    instructions: Some(MCP_INSTRUCTIONS.into()),
    default_flags: vec!["--grant-declared".into()],
})
```

## Declared needs

A tool declares what it reaches beyond itself (`Registry::needs`), computed from its config: HTTP authorities, secret names, a data directory. They're in `describe()`. The native build enforces them (`UreqHttp::checked`, `EnvSecrets::checked`), so an undeclared capability fails in development, not only in the sandbox. A host grants them only when asked: `--grant-declared`. Granted secrets come from where the native build reads them (the environment, then `~/keys.env`).

## Suspend and resume

The host can discard an instance and later start a fresh one from a snapshot the program chose to keep. That sheds memory (linear memory never shrinks), stops idle tools, and carries session state to the next run. The snapshot is opaque bytes; postcard is the recommendation (types it encodes must not use `skip_serializing_if`). Either side can start it:

- **The program:** `host.request-suspend(snapshot)`, at any point; `export_tool!` does it once memory passes the tool's `memory_limit`.
- **The host:** `suspend(reason)`, with reason `idle`, `memory` or `exit`. The program accepts with a snapshot, or refuses with a reason, for example while audio plays.

The fresh instance is configured as before, then given the snapshot (`resume`) before any other call. The host's socket, MCP output and subscribers outlive instances.

## Keeping data

A tool that keeps data of its own gets a directory, `Ports::data_dir`: natively `$<NAME>_DATA_DIR` or the platform's data folder (`tool_contract::standard_data_dir`); under a host, a grant, which the component sees as `/data`. The tool reads and writes it with plain std::fs and decides its own format and locking. Several processes may share it. wasi:filesystem has no advisory locks, so `tool-files` builds lock files from exclusive create, with atomic writes by rename; a lock waits by the tool's own timer, so a holder in the same process carries on. Use a self-describing format (JSON with serde defaults) for data that outlives builds.

## Flags

Both builds, before the command:

```
--data-dir DIR       the tool's data directory          (native default: the standard one)
--no-data            native: no data directory
--data               host: grant the standard data directory
--snapshot FILE      resume from FILE, and suspend into FILE on exit
```

The host only:

```
--allow-http HOST[:PORT]   --secret NAME   --grant-declared
--suspend-idle SECS        ask the component to suspend after SECS without calls
--recycle-above-mb N       ask it to suspend past N MiB (and twice a fresh instance's)
--no-cache                 compile the component every run (the compilation cache is on by default)
```

## Conformance

`tool_contract::conformance` has checks any `Backend` must pass (`check`) and runs the same steps through two backends to compare results and final state (`compare`). A tool runs both against its native build and its component; anki-cards' suite (`anki-cards-conformance`) does, against a strict fake AnkiConnect.

## Versions

The WASM side tracks wasmtime closely: wasmtime 50 (Rust 1.97 or later), WASI 0.3.0, and so `wasip3` 0.9 and wit-bindgen 0.62 in components. A component and its host must agree on the WASI version.

## Status

Extracted from the anki-cards port, where every piece is used and tested. The APIs are young and will change.
