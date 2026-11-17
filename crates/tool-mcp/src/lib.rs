//! MCP, as message handling over a [`Backend`]: one tool per command
//! offered to MCP, with its schemas and annotations from describe(). No
//! transport: the host reads messages from stdio or HTTP and calls
//! [`Server::handle`]. It builds for wasm32-wasip2.

use serde_json::{Map, Value, json};
use tool_contract::{Backend, CommandInfo, Context, Description, Effects, Interface, ToolError, mcp_name, render_args, render_commands};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A tool the *host* adds alongside the component's commands, such as
/// web_start and web_stop (serving is the host's job, not the tool's).
pub struct HostTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
    pub run: Box<dyn FnMut(&mut dyn Backend, Value) -> Result<Value, ToolError>>,
}

/// One MCP session (one client connection).
pub struct Server {
    desc: Description,
    session: String,
    instructions: Option<String>,
    /// The revision this session's last result reported.
    reported: Option<u64>,
    host_tools: Vec<HostTool>,
}

impl Server {
    pub fn new(desc: Description, session: &str, instructions: Option<String>) -> Self {
        Server { desc, session: session.to_string(), instructions, reported: None, host_tools: vec![] }
    }

    /// Adds the host's own tools.
    pub fn with_host_tools(mut self, tools: Vec<HostTool>) -> Self {
        self.host_tools.extend(tools);
        self
    }

    /// Handles one JSON-RPC message, returning the reply (None for a
    /// notification).
    pub fn handle(&mut self, backend: &mut dyn Backend, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = id?; // notifications (initialized, cancelled) need no reply
        let result = match method {
            "initialize" => {
                // Changes are reported from here on, including those made
                // through the host's own tools (web_start).
                self.reported = Some(backend.state(None).and_then(|s| s["revision"].as_u64()).unwrap_or(0));
                Ok(self.initialize(&params))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools()})),
            "tools/call" => Ok(self.call(backend, &params)),
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        // Answer with the client's version when it asks for an older one
        // we can speak; this server uses nothing newer than 2025-06-18.
        let asked = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION);
        let version = if asked < PROTOCOL_VERSION { asked } else { PROTOCOL_VERSION };
        let mut r = json!({
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": self.desc.name, "version": self.desc.version},
        });
        if let Some(i) = &self.instructions {
            r["instructions"] = json!(i);
        }
        r
    }

    fn offered(&self) -> impl Iterator<Item = &CommandInfo> {
        self.desc.commands.iter().filter(|c| c.offered_to(Interface::Mcp) && self.desc.available(c))
    }

    /// The tools: the component's commands, from describe(), then the
    /// host's own.
    pub fn tools(&self) -> Vec<Value> {
        let host = self.host_tools.iter().map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
                "annotations": {"readOnlyHint": t.read_only, "destructiveHint": false, "idempotentHint": true},
            })
        });
        self.offered()
            .map(|c| {
                let mut text = if c.description.is_empty() { c.summary.clone() } else { format!("{} {}", c.summary, c.description) };
                if !c.long.is_empty() {
                    text = format!("{text}\n\n{}", c.long);
                }
                let text = render_args(&render_commands(&text, mcp_name), |a| a.to_string());
                let mut tool = json!({
                    "name": mcp_name(&c.name),
                    "description": text,
                    "inputSchema": for_mcp(&c.input),
                    "annotations": {
                        "readOnlyHint": c.effects == Effects::Read,
                        "destructiveHint": c.effects == Effects::Destructive,
                        "idempotentHint": c.idempotent,
                    },
                });
                if let Some(o) = &c.output {
                    if o.get("type") == Some(&json!("object")) {
                        tool["outputSchema"] = for_mcp(o);
                    }
                }
                tool
            })
            .chain(host)
            .collect()
    }

    fn call(&mut self, backend: &mut dyn Backend, params: &Value) -> Value {
        let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if let Some(t) = self.host_tools.iter_mut().find(|t| t.name == name) {
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            return match (t.run)(backend, args) {
                Ok(v) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
                    "structuredContent": v,
                }),
                Err(e) => tool_error(&e),
            };
        }
        let Some(cmd) = self.offered().find(|c| mcp_name(&c.name) == name).map(|c| c.name.clone()) else {
            return tool_error(&ToolError::new(tool_contract::codes::UNKNOWN_COMMAND, format!("no tool named {name:?}")));
        };
        let input = params.get("arguments").cloned().unwrap_or(json!({}));
        let cx = Context {
            session: Some(self.session.clone()),
            request: params.pointer("/_meta/progressToken").cloned(),
            ..Context::new("mcp")
        };
        let result = backend.invoke(&cx, &cmd, input);
        // Every result says what changed since this session's last one
        // (the user may act through the web page or terminal meanwhile).
        let shared = backend.state(Some(self.reported.unwrap_or(0)));
        let shared = shared.map(|s| {
            let first = self.reported.is_none();
            self.reported = s["revision"].as_u64();
            (s["revision"].clone(), if first { json!([]) } else { s["changes"].clone() }, s["changes_truncated"].clone())
        });
        let with_changes = |mut v: Value| {
            if let (Some(o), Some((rev, changes, truncated))) = (v.as_object_mut(), &shared) {
                o.entry("revision").or_insert(rev.clone());
                o.insert("changes_since_last_call".into(), changes.clone());
                if truncated == &json!(true) {
                    o.insert("changes_truncated".into(), json!(true));
                }
            }
            v
        };
        match result.map(with_changes) {
            Ok(v) => json!({
                "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
                "structuredContent": v,
            }),
            Err(e) => tool_error(&e),
        }
    }
}

fn tool_error(e: &ToolError) -> Value {
    let e = ToolError { message: render_commands(&render_args(&e.message, |a| a.to_string()), mcp_name), ..e.clone() };
    json!({
        "content": [{"type": "text", "text": serde_json::to_string_pretty(&json!({"error": e})).unwrap_or_default()}],
        "isError": true,
    })
}

fn offered_to_mcp(property: &Value) -> bool {
    tool_contract::property_offered(property, Interface::Mcp)
}

/// A schema as MCP clients see it: properties not offered to MCP removed,
/// and the CLI's x- hints dropped.
fn for_mcp(schema: &Value) -> Value {
    let mut s = schema.clone();
    let mut removed = vec![];
    if let Some(props) = s.get_mut("properties").and_then(|p| p.as_object_mut()) {
        props.retain(|name, p| {
            let keep = offered_to_mcp(p);
            if !keep {
                removed.push(name.clone());
            }
            keep
        });
        for p in props.values_mut() {
            strip_hints(p);
        }
    }
    if let Some(req) = s.get_mut("required").and_then(|r| r.as_array_mut()) {
        req.retain(|r| !removed.iter().any(|x| r == x));
    }
    strip_hints(&mut s);
    s
}

fn strip_hints(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        let keys: Vec<String> = o.keys().filter(|k| k.starts_with("x-")).cloned().collect();
        for k in keys {
            o.remove(&k);
        }
    }
}

/// Serves MCP over newline-delimited JSON on stdin and the given writer.
/// It returns when stdin ends, after letting the tool stop work in
/// progress.
pub fn serve_stdio(server: &mut Server, backend: &mut dyn Backend, mut write: impl FnMut(&Value)) {
    use std::io::BufRead;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => server.handle(backend, &msg),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = reply {
            write(&r);
        }
    }
    // The client has gone: let the tool stop anything in progress, so no
    // sound outlives the server.
    backend.shutdown();
}

/// An MCP progress notification for a call's progress token.
pub fn progress_notification(token: &Value, progress: u64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": {"progressToken": token, "progress": progress, "message": message},
    })
}

/// Settings for an MCP server come from the environment (`.mcp.json`'s
/// `env`), using the names describe() gives.
pub fn config_from_env(desc: &Description) -> Map<String, Value> {
    desc.config
        .iter()
        .filter_map(|c| {
            let v = c.env.as_ref().and_then(|e| std::env::var(e).ok()).or_else(|| c.default.clone())?;
            Some((c.name.clone(), json!(v)))
        })
        .collect()
}

/// Where MCP messages go: stdout, shared with progress notifications.
pub type Writer = std::sync::Arc<std::sync::Mutex<std::io::Stdout>>;

/// The `mcp` command: serves the tool's commands as MCP tools over stdio,
/// for an MCP client to start (e.g. from `.mcp.json`). `on_start` is given
/// the writer before serving, so the build can route progress for calls
/// with a progress token there as notifications.
pub fn mcp_command(name: &str, instructions: Option<String>, on_start: Box<dyn FnOnce(Writer)>) -> tool_cli::Custom<'static> {
    let name = name.to_string();
    tool_cli::Custom {
        command: clap::Command::new("mcp").about("Serve the commands as MCP tools over stdio, for an MCP client to start."),
        run: Box::new(move |b: &mut dyn Backend, _| {
            use std::io::{IsTerminal, Write};
            if std::io::stdin().is_terminal() {
                eprintln!("{name} mcp is an MCP server for a client to start (e.g. from .mcp.json); it's waiting for messages on stdin.");
            }
            let out: Writer = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
            on_start(out.clone());
            eprintln!("{name} MCP server running on stdio");
            let mut server = Server::new(b.describe(), "stdio", instructions);
            serve_stdio(&mut server, b, move |v: &Value| {
                let mut o = out.lock().unwrap();
                let _ = writeln!(o, "{v}");
                let _ = o.flush();
            });
            0
        }),
    }
}
