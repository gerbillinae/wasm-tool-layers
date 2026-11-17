//! The contract every interface shares (see opinionated/optional/wasm.md):
//!
//! - [`Description`], what `describe()` returns: commands with JSON Schemas
//!   for their input and output, error codes with categories, and the hints
//!   the CLI generator needs;
//! - [`Registry`], where a tool registers its commands once, and which
//!   produces `describe` and `invoke` from them;
//! - [`ToolError`] and [`Context`].
//!
//! It knows nothing about clap, MCP or HTTP, and compiles for wasm32-wasip2.

use std::collections::BTreeMap;
use std::sync::Arc;

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The version of the description format.
pub const CONTRACT: &str = "opinionated.tool/0.1";

// ---------------------------------------------------------------- errors

/// An error with a stable code.
///
/// The message may name arguments as `{arg:create_missing}`. Each interface
/// renders those its own way (`--create-missing` on the CLI,
/// `create_missing` for MCP and REST), because only the interface knows how
/// it spells an argument. See [`ToolError::render`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ToolError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        ToolError { code: code.to_string(), message: message.into(), details: None }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The message with each `{arg:name}` replaced by `spell(name)`, and
    /// `{cmd:name}` by the command's name.
    pub fn render(&self, spell: impl Fn(&str) -> String) -> String {
        render_commands(&render_args(&self.message, spell), |c| c.to_string())
    }

    /// The error as MCP and REST show it: arguments by their JSON names.
    pub fn rendered_plain(&self) -> ToolError {
        ToolError { message: self.render(|n| n.to_string()), ..self.clone() }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.render(|n| n.to_string()))
    }
}

impl std::error::Error for ToolError {}

/// Replaces `{arg:name}` in text with `spell(name)`.
pub fn render_args(text: &str, spell: impl Fn(&str) -> String) -> String {
    render_refs(text, "{arg:", spell)
}

/// Replaces `{cmd:name}` (a reference to another command, in help and
/// descriptions) with `spell(name)`: `list-cards` on the CLI, `list_cards`
/// for MCP.
pub fn render_commands(text: &str, spell: impl Fn(&str) -> String) -> String {
    render_refs(text, "{cmd:", spell)
}

fn render_refs(text: &str, open: &str, spell: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        match after.find('}') {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push_str(&spell(&after[..end]));
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Whether an input property is offered to an interface (its
/// `x-interfaces` hint, if any, names it).
pub fn property_offered(property: &Value, i: Interface) -> bool {
    let name = serde_json::to_value(i).ok();
    property
        .get("x-interfaces")
        .and_then(|v| v.as_array())
        .is_none_or(|only| only.iter().any(|x| Some(x) == name.as_ref()))
}

/// A command's MCP tool name: list-cards becomes list_cards.
pub fn mcp_name(command: &str) -> String {
    command.replace('-', "_")
}

/// Codes the contract itself uses; tools add their own.
pub mod conformance;

pub mod codes {
    pub const UNKNOWN_COMMAND: &str = "unknown_command";
    pub const NOT_AVAILABLE_HERE: &str = "not_available_here";
    pub const FEATURE_UNAVAILABLE: &str = "feature_unavailable";
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    pub const MISSING_ARGUMENT: &str = "missing_argument";
    pub const INTERNAL: &str = "internal";
}

/// What kind of failure a code is. Interfaces map categories, not codes:
/// REST to HTTP statuses, the CLI to exit statuses.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    InvalidInput,
    NotFound,
    Conflict,
    Unavailable,
    Refused,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub description: String,
    pub category: Category,
}

// ---------------------------------------------------------------- context

/// Who is calling, and from where.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Context {
    /// "terminal", "tui", "mcp" or "web". It becomes the change log's `by`.
    pub by: String,
    /// An MCP session, so results can carry changes since that session's
    /// last call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Whether someone is there to answer questions for this call. The
    /// host's asker is only used when this is set: one instance serves
    /// callers who can answer (a terminal) and callers who can't (MCP, REST).
    #[serde(default)]
    pub interactive: bool,
    /// An interface's own handle on this call, e.g. an MCP progress token,
    /// so the host can route progress for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
}

impl Context {
    pub fn new(by: &str) -> Self {
        Context { by: by.to_string(), session: None, interactive: false, request: None }
    }

    /// The interface this caller uses, for `interfaces` checks.
    pub fn interface(&self) -> Option<Interface> {
        match self.by.as_str() {
            "terminal" => Some(Interface::Cli),
            "tui" => Some(Interface::Tui),
            "mcp" => Some(Interface::Mcp),
            "web" => Some(Interface::Rest),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Interface {
    Cli,
    Tui,
    Mcp,
    Rest,
}

pub const ALL_INTERFACES: &[Interface] = &[Interface::Cli, Interface::Tui, Interface::Mcp, Interface::Rest];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Effects {
    Read,
    Write,
    Destructive,
}

// ---------------------------------------------------------------- backend

/// What an interface runs commands against: a native core, or a component
/// through a WASM host. The CLI generator, the MCP server and the REST
/// handler all take one.
pub trait Backend {
    fn describe(&mut self) -> Description;
    /// Called once, before any command, with the tool-wide settings.
    fn configure(&mut self, config: &serde_json::Map<String, Value>) -> Result<(), ToolError>;
    fn invoke(&mut self, cx: &Context, command: &str, input: Value) -> Result<Value, ToolError>;
    /// Asks to be told the shared state's new revision whenever it
    /// changes, including changes made by background work between calls
    /// (playback moving on), so event streams can wake.
    fn subscribe(&mut self, _on_change: std::sync::Arc<dyn Fn(u64) + Send + Sync>) {}
    /// Another handle on the same instance, for calling from another
    /// thread at the same time (a REST server answering requests in
    /// parallel). None if calls must all come through this one.
    fn clone_handle(&self) -> Option<Box<dyn Backend + Send>> {
        None
    }
    /// The host is going away: stop work in progress.
    fn shutdown(&mut self) {}
    /// The shared state (JSON, with at least `revision`), with the changes
    /// after `since`. None if the tool has no shared state.
    fn state(&mut self, _since: Option<u64>) -> Option<Value> {
        None
    }
}

/// A file the tool serves (its web UI).
#[derive(Debug, Clone)]
pub struct Asset {
    pub content_type: String,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------- http

/// How an outgoing HTTP error starts when the host refused the request
/// (a WASM host without the grant), as opposed to the server not answering.
pub const HTTP_DENIED: &str = "denied by host";

/// An HTTP request, as a host's listener reads it.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    /// Without the query string.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A response's status and headers; the body follows as chunks.
#[derive(Debug, Clone)]
pub struct HttpHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

/// A response as a host's listener receives it: the head, then the body as
/// a stream of chunks, which ends when the response is complete (an event
/// stream stays open until the client goes away).
pub type HttpReply = (HttpHead, futures::channel::mpsc::Receiver<Vec<u8>>);

/// Forwards a request (which has passed the host's Origin and Host checks)
/// to the tool's own HTTP handler. `gone` resolves (cancelled) when the
/// client disconnects, so the tool can stop at once rather than at its next
/// write.
pub trait HttpForward: Send + Sync {
    fn forward(
        &self,
        req: HttpRequest,
        gone: futures::channel::oneshot::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<HttpReply>> + Send>>;
}

/// Bytes as base64 in JSON (invoke, MCP); an HTTP route can send them raw
/// (see [`Route::bytes`]).
pub mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------- description

/// What `describe()` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Description {
    pub contract: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub long: String,
    #[serde(default)]
    pub config: Vec<ConfigItem>,
    #[serde(default)]
    pub features: BTreeMap<String, bool>,
    pub commands: Vec<CommandInfo>,
    pub errors: BTreeMap<String, ErrorInfo>,
    /// The tool's HTTP interface, if it has one: every route, those
    /// generated from commands and the fixed browser adapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpSection>,
    /// The component's typed commands interface (e.g.
    /// "anki:cards/commands@0.1.0"): one typed function per command, named
    /// as the command. Hosts call those instead of the JSON invoke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpSection {
    pub routes: Vec<Route>,
}

/// One HTTP route. A route either adapts a command (`command` names it;
/// the route only changes the transport) or is one of the browser adapters
/// (the page, its files, the state and the event stream), fed by the tool's
/// state and assets. No route has logic of its own: anything a TUI or an
/// agent might need is a command. The component checks the token itself
/// (it made it), so hosts need no knowledge of routes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    pub method: String,
    /// May name input properties as {name}, e.g. /api/media/{file}.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub body: BodyKind,
    /// For `bytes`: which result fields hold the data and its content type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<BytesFields>,
    /// Served without the token (the page and its files hold none).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub public: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BodyKind {
    /// The result as JSON, with the state's revision added.
    Json,
    /// One result field as the raw body, another as its Content-Type.
    Bytes,
    /// Server-sent events: the state, then at every change.
    EventStream,
    /// A file of the web UI.
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BytesFields {
    pub data: String,
    pub content_type: String,
}

impl Route {
    pub fn get(path: &str) -> Route {
        Route::new("GET", path)
    }
    pub fn post(path: &str) -> Route {
        Route::new("POST", path)
    }
    pub fn patch(path: &str) -> Route {
        Route::new("PATCH", path)
    }
    pub fn delete(path: &str) -> Route {
        Route::new("DELETE", path)
    }
    pub fn new(method: &str, path: &str) -> Route {
        Route {
            method: method.into(),
            path: path.into(),
            command: None,
            body: BodyKind::Json,
            bytes: None,
            public: false,
            description: String::new(),
        }
    }
    /// The command's result field `data` (bytes) is sent as the body, with
    /// the field `content_type` as its Content-Type, instead of JSON.
    pub fn bytes(mut self, data: &str, content_type: &str) -> Route {
        self.body = BodyKind::Bytes;
        self.bytes = Some(BytesFields { data: data.into(), content_type: content_type.into() });
        self
    }
    fn adapter(method: &str, path: &str, body: BodyKind, public: bool, description: &str) -> Route {
        Route { body, public, description: description.into(), ..Route::new(method, path) }
    }
}

impl Description {
    pub fn command(&self, name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// Whether every feature the command requires is available here.
    pub fn available(&self, cmd: &CommandInfo) -> bool {
        cmd.requires.iter().all(|f| self.features.get(f).copied().unwrap_or(false))
    }

    pub fn category(&self, code: &str) -> Category {
        self.errors.get(code).map(|e| e.category).unwrap_or(Category::Internal)
    }
}

/// A tool-wide setting, such as AnkiConnect's address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigItem {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub summary: String,
    /// Longer help for people (CLI --help).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub long: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    /// How and when an agent should use it; MCP's description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub effects: Effects,
    pub idempotent: bool,
    pub interfaces: Vec<Interface>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub progress: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl CommandInfo {
    pub fn offered_to(&self, i: Interface) -> bool {
        self.interfaces.contains(&i)
    }
}

/// A question the core may ask, and the argument that answers it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub answered_by: String,
    pub text: String,
}


// ---------------------------------------------------------------- registry

/// A future that runs on the tool's one thread (not Send).
pub type LocalFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T>>>;

// ------------------------------------------------------------ the instance

/// Why a host wants an instance gone (see [`ToolInstance::suspend`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuspendReason {
    /// Nothing has called for a while.
    Idle,
    /// Its memory has grown; a fresh instance starts small.
    Memory,
    /// The host is exiting; the snapshot is kept for the next run.
    Exit,
}

/// What tool-rest needs from the tool, in process: the tool's HTTP
/// interface runs inside it, natively and in a component alike.
pub trait HttpService {
    fn invoke(&self, cx: &Context, command: &str, input: Value) -> LocalFuture<Result<Value, ToolError>>;
    /// The shared state (with `revision`), with the changes after `since`.
    fn state(&self, since: Option<u64>) -> Option<Value>;
    /// Resolves once the state's revision is past `after`.
    fn changed(&self, after: u64) -> LocalFuture<()>;
    fn sleep(&self, ms: u64) -> LocalFuture<()>;
    fn asset(&self, path: &str) -> Option<Asset>;
    /// A page opened (true) or closed (false) the event stream.
    fn page(&self, connected: bool);
    /// Whether a request's token is the one the tool issued when it asked
    /// to be served (false while it isn't served).
    fn authorized(&self, token: &str) -> bool;
}

/// One instance of a tool, as every runtime drives it: the native build
/// (tool-native) calls it in process, and a component exports it
/// (tool-wasi's `export_tool!`) for a host (tool-host) to call. A tool
/// implements it once, in its core.
///
/// Calls arrive on one thread, the instance's owner, and may interleave
/// while they wait; background work the tool spawns runs between them.
pub trait ToolInstance: 'static {
    fn describe(&self) -> Description;
    /// Settings, named as describe's `config` names them.
    fn configure(&self, settings: serde_json::Map<String, Value>) -> Result<(), ToolError>;
    fn invoke(self: Arc<Self>, cx: &Context, command: &str, input: Value) -> LocalFuture<Result<Value, ToolError>>;
    /// The shared state (with `revision`) and the changes after `since`.
    fn state(&self, _since: Option<u64>) -> Option<Value> {
        None
    }
    /// The host is going away: stop work in progress.
    fn shutdown(&self) {}
    /// The tool's HTTP interface (tool-rest), if it has one.
    fn http(self: Arc<Self>) -> Option<std::rc::Rc<dyn HttpService>> {
        None
    }
    /// The host wants this instance gone. Ok accepts, keeping a snapshot
    /// (None: nothing); Err refuses, with the reason. The snapshot is the
    /// tool's own bytes: plain data, with work in progress described so
    /// `resume` can start it again. The default refuses.
    fn suspend(&self, _reason: SuspendReason) -> Result<Option<Vec<u8>>, String> {
        Err("this tool doesn't support suspending".into())
    }
    /// A fresh instance takes up where a suspended one left off, before
    /// any other call. `listening` is the address the host still serves
    /// this tool's HTTP interface on, if it does.
    fn resume(&self, _snapshot: &[u8], _listening: Option<String>) -> Result<(), String> {
        Err("this tool doesn't support resuming".into())
    }
    /// In a component: ask to be suspended (keeping a snapshot) once linear
    /// memory passes this many bytes, since it never shrinks. Natively the
    /// allocator returns memory, so it's unused there.
    fn memory_limit(&self) -> Option<u64> {
        None
    }
}

/// Where a tool keeps its data unless told otherwise: `$<NAME>_DATA_DIR`
/// (e.g. ANKI_CARDS_DATA_DIR), else the platform's usual place for an app's
/// data (~/Library/Application Support/NAME on macOS, %APPDATA%\NAME on
/// Windows, $XDG_DATA_HOME/NAME or ~/.local/share/NAME elsewhere). The
/// native build uses it by default; a WASM host grants it with `--data`.
/// Not created here.
pub fn standard_data_dir(name: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let var = format!("{}_DATA_DIR", name.to_uppercase().replace('-', "_"));
    if let Some(d) = std::env::var_os(&var).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|x| !x.is_empty()) {
        return Some(PathBuf::from(x).join(name));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(name));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(if cfg!(target_os = "macos") { home.join("Library/Application Support").join(name) } else { home.join(".local/share").join(name) })
}

type Handler<T> = Arc<dyn Fn(Arc<T>, Context, Value) -> LocalFuture<Result<Value, ToolError>> + Send + Sync>;

/// One command: its description and its handler.
pub struct Command<T> {
    pub info: CommandInfo,
    handler: Handler<T>,
    route: Option<Route>,
}

/// The JSON Schema for a type, with subschemas inlined so a generator can
/// read properties directly.
pub fn schema_for<S: JsonSchema>() -> Value {
    let mut settings = SchemaSettings::draft07();
    settings.inline_subschemas = true;
    let schema = settings.into_generator().into_root_schema_for::<S>();
    let mut v = serde_json::to_value(schema).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("$schema");
    }
    v
}

impl<T: 'static> Command<T> {
    /// A command taking `I` and returning `O`, as an async function, so a
    /// command waiting on I/O doesn't hold up others. It defaults to a
    /// read-only, idempotent command offered to every interface.
    pub fn new<I, O, F, Fut>(name: &str, summary: &str, f: F) -> Self
    where
        I: DeserializeOwned + JsonSchema + 'static,
        O: Serialize + JsonSchema + 'static,
        F: Fn(Arc<T>, Context, I) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<O, ToolError>> + 'static,
    {
        let f = Arc::new(f);
        let handler: Handler<T> = Arc::new(move |t, cx, input| {
            let input = if input.is_null() { Value::Object(Default::default()) } else { input };
            let parsed: Result<I, ToolError> = serde_json::from_value(input)
                .map_err(|e| ToolError::new(codes::INVALID_ARGUMENT, format!("invalid arguments: {e}")));
            let f = f.clone();
            Box::pin(async move {
                let out = f(t, cx, parsed?).await?;
                serde_json::to_value(out).map_err(|e| ToolError::new(codes::INTERNAL, e.to_string()))
            })
        });
        Command {
            info: CommandInfo {
                name: name.to_string(),
                summary: summary.to_string(),
                long: String::new(),
                examples: vec![],
                description: String::new(),
                effects: Effects::Read,
                idempotent: true,
                interfaces: ALL_INTERFACES.to_vec(),
                requires: vec![],
                input: schema_for::<I>(),
                output: Some(schema_for::<O>()),
                questions: vec![],
                progress: false,
                errors: vec![],
            },
            handler,
            route: None,
        }
    }

    pub fn long(mut self, s: &str) -> Self {
        self.info.long = s.trim().to_string();
        self
    }
    pub fn example(mut self, s: &str) -> Self {
        self.info.examples.push(s.to_string());
        self
    }
    pub fn description(mut self, s: &str) -> Self {
        self.info.description = s.trim().to_string();
        self
    }
    pub fn effects(mut self, e: Effects) -> Self {
        self.info.effects = e;
        if e != Effects::Read {
            self.info.idempotent = false;
        }
        self
    }
    pub fn idempotent(mut self, yes: bool) -> Self {
        self.info.idempotent = yes;
        self
    }
    pub fn interfaces(mut self, i: &[Interface]) -> Self {
        self.info.interfaces = i.to_vec();
        self
    }
    pub fn requires(mut self, feature: &str) -> Self {
        self.info.requires.push(feature.to_string());
        self
    }
    pub fn question(mut self, id: &str, answered_by: &str, text: &str) -> Self {
        self.info.questions.push(Question { id: id.into(), answered_by: answered_by.into(), text: text.into() });
        self
    }
    pub fn progress(mut self) -> Self {
        self.info.progress = true;
        self
    }
    pub fn errors(mut self, codes: &[&str]) -> Self {
        self.info.errors.extend(codes.iter().map(|c| c.to_string()));
        self
    }
    /// The command's HTTP route, when the default (`GET /api/<name>` for a
    /// read, `POST /api/<name>` otherwise, JSON in and out) doesn't fit. The
    /// route only adapts the transport: it's the same command, which every
    /// other interface calls through invoke.
    pub fn http(mut self, route: Route) -> Self {
        self.route = Some(route);
        self
    }
    /// Changes one property of the input schema, e.g. to add CLI hints that
    /// can't be written on the struct because the struct is shared.
    pub fn input_hint(mut self, property: &str, key: &str, value: Value) -> Self {
        if let Some(p) = self.info.input.pointer_mut(&format!("/properties/{property}")) {
            if let Some(o) = p.as_object_mut() {
                o.insert(key.to_string(), value);
            }
        }
        self
    }
}

/// A tool's commands, registered once. `describe` and `invoke` come from it.
pub struct Registry<T> {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub long: String,
    pub config: Vec<ConfigItem>,
    pub errors: BTreeMap<String, ErrorInfo>,
    commands: Vec<Command<T>>,
    features: Option<Box<dyn Fn(&T) -> BTreeMap<String, bool> + Send + Sync>>,
    web_ui: bool,
    wit: Option<String>,
}

impl<T: 'static> Registry<T> {
    pub fn new(name: &str, version: &str, summary: &str) -> Self {
        let mut errors = BTreeMap::new();
        for (code, description, category) in [
            (codes::UNKNOWN_COMMAND, "No command by that name.", Category::NotFound),
            (codes::NOT_AVAILABLE_HERE, "The command isn't offered to this interface.", Category::Refused),
            (codes::FEATURE_UNAVAILABLE, "The command needs something this host doesn't provide.", Category::Unavailable),
            (codes::INVALID_ARGUMENT, "An argument is malformed.", Category::InvalidInput),
            (codes::MISSING_ARGUMENT, "A needed argument wasn't given; the message names it.", Category::InvalidInput),
            (codes::INTERNAL, "A bug, or something unexpected.", Category::Internal),
        ] {
            errors.insert(code.to_string(), ErrorInfo { description: description.into(), category });
        }
        Registry {
            name: name.into(),
            version: version.into(),
            summary: summary.into(),
            long: String::new(),
            config: vec![],
            errors,
            commands: vec![],
            features: None,
            web_ui: false,
            wit: None,
        }
    }

    /// The component's typed commands interface (see Description::wit).
    pub fn wit(mut self, interface: &str) -> Self {
        self.wit = Some(interface.into());
        self
    }

    /// The tool has an HTTP interface with a web UI: its commands' routes
    /// plus the browser adapters (the page and its files, the state, the
    /// event stream).
    pub fn web_ui(mut self) -> Self {
        self.web_ui = true;
        self
    }

    /// Every HTTP route: one per command offered to REST, then the browser
    /// adapters.
    pub fn routes(&self) -> Vec<Route> {
        let mut routes: Vec<Route> = self
            .commands
            .iter()
            .filter(|c| c.info.offered_to(Interface::Rest))
            .map(|c| {
                let mut r = c.route.clone().unwrap_or_else(|| {
                    let method = if c.info.effects == Effects::Read { "GET" } else { "POST" };
                    Route::new(method, &format!("/api/{}", c.info.name))
                });
                r.command = Some(c.info.name.clone());
                r
            })
            .collect();
        routes.extend([
            Route::adapter("GET", "/api/state", BodyKind::Json, false, "The shared state; ?since_revision= adds the changes since then."),
            Route::adapter("GET", "/api/events", BodyKind::EventStream, false, "The shared state now, then at every change, as server-sent events."),
            Route::adapter("GET", "/", BodyKind::File, true, "The web UI."),
            Route::adapter("GET", "/{file*}", BodyKind::File, true, "The web UI's files."),
        ]);
        routes
    }

    pub fn long(mut self, s: &str) -> Self {
        self.long = s.trim().to_string();
        self
    }
    pub fn config(mut self, name: &str, description: &str, default: Option<&str>, env: Option<&str>) -> Self {
        self.config.push(ConfigItem {
            name: name.into(),
            description: description.into(),
            default: default.map(Into::into),
            env: env.map(Into::into),
        });
        self
    }
    pub fn error(mut self, code: &str, category: Category, description: &str) -> Self {
        self.errors.insert(code.into(), ErrorInfo { description: description.into(), category });
        self
    }
    pub fn features(mut self, f: impl Fn(&T) -> BTreeMap<String, bool> + Send + Sync + 'static) -> Self {
        self.features = Some(Box::new(f));
        self
    }
    pub fn command(mut self, c: Command<T>) -> Self {
        self.commands.push(c);
        self
    }

    pub fn describe(&self, tool: &T) -> Description {
        Description {
            contract: CONTRACT.into(),
            name: self.name.clone(),
            version: self.version.clone(),
            summary: self.summary.clone(),
            long: self.long.clone(),
            config: self.config.clone(),
            features: self.features.as_ref().map(|f| f(tool)).unwrap_or_default(),
            commands: self.commands.iter().map(|c| c.info.clone()).collect(),
            errors: self.errors.clone(),
            http: self.web_ui.then(|| HttpSection { routes: self.routes() }),
            wit: self.wit.clone(),
        }
    }

    /// Runs a command. It refuses commands not offered to the caller's
    /// interface, and commands whose features this host lacks.
    pub fn invoke(&self, tool: Arc<T>, cx: &Context, name: &str, input: Value) -> LocalFuture<Result<Value, ToolError>> {
        match self.check(&tool, cx, name, &input) {
            Ok(cmd) => (cmd.handler)(tool, cx.clone(), input),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }

    fn check(&self, tool: &T, cx: &Context, name: &str, input: &Value) -> Result<&Command<T>, ToolError> {
        let Some(cmd) = self.commands.iter().find(|c| c.info.name == name) else {
            return Err(ToolError::new(codes::UNKNOWN_COMMAND, format!("{} has no command {name:?}", self.name)));
        };
        if let Some(i) = cx.interface() {
            if !cmd.info.offered_to(i) {
                return Err(ToolError::new(
                    codes::NOT_AVAILABLE_HERE,
                    format!("{{cmd:{name}}} isn't offered to {} callers", format!("{i:?}").to_lowercase()),
                ));
            }
            // Unknown arguments are refused, not ignored, whatever the input
            // type does (serde can't deny unknown fields through flatten),
            // and so are arguments not offered to this interface.
            if let (Some(args), Some(props)) =
                (input.as_object(), cmd.info.input.get("properties").and_then(|p| p.as_object()))
            {
                if let Some(bad) = args.keys().find(|k| props.get(*k).is_none_or(|p| !property_offered(p, i))) {
                    return Err(ToolError::new(codes::INVALID_ARGUMENT, format!("{{cmd:{name}}} has no argument {bad:?}")));
                }
            }
        }
        if !cmd.info.requires.is_empty() {
            let features = self.features.as_ref().map(|f| f(tool)).unwrap_or_default();
            if let Some(missing) = cmd.info.requires.iter().find(|f| !features.get(*f).copied().unwrap_or(false)) {
                return Err(ToolError::new(
                    codes::FEATURE_UNAVAILABLE,
                    format!("{{cmd:{name}}} needs {missing}, which this host doesn't provide"),
                ));
            }
        }
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_argument_names() {
        let e = ToolError::new("x", "pass {arg:create_missing} or {arg:deck}, {arg:oops");
        assert_eq!(e.render(|n| format!("--{}", n.replace('_', "-"))), "pass --create-missing or --deck, {arg:oops");
    }
}
