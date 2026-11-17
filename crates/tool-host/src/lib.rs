//! The generic host for components implementing opinionated:tool (see
//! wit/tool.wit), as a library. `wasm-tool` is [`run`] with nothing added; a
//! tool-specific host passes [`Extension`]s: extra imports for the linker,
//! state for them, grant flags, and custom commands.
//!
//!   HOST [GRANTS]... COMPONENT.wasm COMMAND [ARGS]
//!
//! The component gets nothing it isn't granted: no files, no environment,
//! no sockets, outgoing HTTP only to the --allow-http authorities, and only
//! the --secret names.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tool_contract::{Backend, Context, Description, ToolError};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};

// Hosts bind the core world (the tool interface and host imports); the
// component's wasi:http/handler export is reached through
// wasmtime-wasi-http's own bindings.
wasmtime::component::bindgen!({ world: "tool-core", path: "../../wit" });

/// The optional lifecycle export (suspend and resume), bound on its own so
/// components without it still load.
mod lifecycle {
    wasmtime::component::bindgen!({ world: "tool-lifecycle", path: "../../wit" });
    pub use exports::opinionated::tool::lifecycle::{SuspendReason, SuspendReply};
}

mod typed;

use opinionated::tool::types as wit;

pub use wasmtime;

// ------------------------------------------------------------------ grants

/// What the user granted on the command line.
#[derive(Debug, Default, Clone)]
pub struct Grants {
    pub http: Vec<String>,
    pub secrets: Vec<String>,
    /// Grant flags an extension added (e.g. --audio) that were given.
    pub flags: Vec<String>,
    /// A directory the component may keep files in, mounted at /data.
    pub data_dir: Option<std::path::PathBuf>,
}

/// Where a granted data directory appears inside the component.
pub const DATA_MOUNT: &str = "/data";

/// Outgoing HTTP is checked against the granted authorities before it's
/// sent.
struct Hooks {
    allow: Vec<String>,
}

impl WasiHttpHooks for Hooks {
    fn send_request(
        &mut self,
        request: http::Request<wasmtime_wasi_http::WasiBody>,
        options: Option<wasmtime_wasi_http::RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<wasmtime_wasi_http::WasiBody>,
                        Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        let authority = request.uri().authority().map(|a| a.to_string()).unwrap_or_default();
        let host = authority.split(':').next().unwrap_or("").to_string();
        if !self.allow.iter().any(|a| *a == authority || *a == host) {
            eprintln!("refused HTTP to {authority} (grant it with --allow-http {authority})");
            return Box::new(async { Err(wasmtime_wasi_http::Error::HttpRequestDenied) });
        }
        Box::new(async move {
            use http_body_util::BodyExt;
            let (res, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
            Ok((res.map(BodyExt::boxed_unsync), Box::new(io) as Box<dyn Future<Output = _> + Send>))
        })
    }
}

// ------------------------------------------------------------------ state

type Writer = Arc<Mutex<std::io::Stdout>>;

/// What outlives an instance: when one is suspended and a fresh one
/// started, these carry over (the socket stays open, MCP keeps its output).
#[derive(Default)]
struct Shared {
    /// Set in mcp mode: progress for calls with a progress token goes out
    /// as MCP notifications.
    mcp_out: Option<(Writer, u64)>,
    /// Told new revisions (Backend::subscribe).
    listener: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// The running listener for the component's HTTP export.
    serving: Option<tool_serve::Listener>,
    /// The last settings configure accepted, replayed into a fresh instance.
    settings: Option<String>,
    /// The granted data directory, mounted at /data in each new instance.
    data_dir: Option<std::path::PathBuf>,
    /// The next suspend and resume are the host's own housekeeping (a
    /// re-grant), not worth a log line.
    quiet: bool,
}

/// The store's state. `ext` is the extension's: the state behind the
/// imports it added.
pub struct State<X: 'static> {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    hooks: Hooks,
    secrets: Vec<String>,
    shared: Arc<Mutex<Shared>>,
    /// For serving the component's HTTP export: the queue to its thread
    /// (None if it has no export).
    http_jobs: Option<tokio::sync::mpsc::UnboundedSender<Job>>,
    /// The program asked to be suspended (request-suspend), keeping this.
    suspend_request: Option<Option<Vec<u8>>>,
    suspend_wake: Arc<tokio::sync::Notify>,
    /// The size of the instance's largest linear memory, in bytes.
    memory: MemoryWatch,
    pub ext: X,
}

/// Watches linear memory growth (it never shrinks), for recycling.
#[derive(Default)]
struct MemoryWatch {
    largest: usize,
}

impl wasmtime::ResourceLimiter for MemoryWatch {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        self.largest = self.largest.max(desired);
        Ok(true)
    }
    fn table_growing(&mut self, _current: usize, _desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

impl<X: Send> WasiView for State<X> {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl<X: Send> WasiHttpView for State<X> {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView { ctx: &mut self.http, table: &mut self.table, hooks: &mut self.hooks }
    }
}

impl<X> wit::Host for State<X> {}

impl<X> opinionated::tool::host::Host for State<X> {
    fn confirm(&mut self, question: String) -> Result<bool, wit::ToolError> {
        let a = self.ask(format!("{question} [y/N] "))?.to_lowercase();
        Ok(a == "y" || a == "yes")
    }

    fn ask(&mut self, question: String) -> Result<String, wit::ToolError> {
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(n) if n > 0 => Ok(line.trim().to_string()),
            _ => Err(wit::ToolError {
                code: "not_confirmed".into(),
                message: "no answer was given. Nothing was changed.".into(),
                details: None,
            }),
        }
    }

    fn progress(&mut self, cx: wit::Context, message: String) {
        let token = cx.request.and_then(|r| serde_json::from_str::<Value>(&r).ok());
        let mut shared = self.shared.lock().unwrap();
        if let (Some((out, n)), Some(token)) = (shared.mcp_out.as_mut(), token) {
            *n += 1;
            let mut o = out.lock().unwrap();
            let _ = writeln!(o, "{}", tool_mcp::progress_notification(&token, *n, &message));
            let _ = o.flush();
            return;
        }
        eprintln!("{message}");
    }

    fn get_secret(&mut self, name: String) -> Option<opinionated::tool::host::Secret> {
        if !self.secrets.contains(&name) {
            return None;
        }
        let value = std::env::var(&name).ok().filter(|v| !v.trim().is_empty())?;
        Some(opinionated::tool::host::Secret {
            value: value.trim().into(),
            source: format!("granted ({name} from the host's environment)"),
        })
    }

    fn secret_hint(&mut self, name: String) -> String {
        if self.secrets.contains(&name) {
            format!("{name} is granted, but the host's environment doesn't set it")
        } else {
            format!("grant it with --secret {name}, with {name} set in the host's environment")
        }
    }

    fn changed(&mut self, revision: u64) {
        let f = self.shared.lock().unwrap().listener.clone();
        if let Some(f) = f {
            f(revision);
        }
    }

    fn listen(&mut self, port: u16, allow_origins: Vec<String>) -> Result<String, String> {
        let jobs = self.http_jobs.clone().ok_or("this component has no HTTP interface")?;
        self.shared.lock().unwrap().serving = None;
        let l = tool_serve::listen(Arc::new(WasmHttp { jobs }), port, allow_origins)?;
        let url = l.url.clone();
        self.shared.lock().unwrap().serving = Some(l);
        Ok(url)
    }

    fn unlisten(&mut self) {
        self.shared.lock().unwrap().serving = None;
    }

    fn request_suspend(&mut self, snapshot: Option<Vec<u8>>) {
        self.suspend_request = Some(snapshot);
        self.suspend_wake.notify_one();
    }

    fn utc_offset(&mut self, at_ms: i64) -> i32 {
        use chrono::{Local, Offset, TimeZone};
        Local.timestamp_millis_opt(at_ms).single().map(|t| t.offset().fix().local_minus_utc()).unwrap_or(0)
    }
}

// ------------------------------------------------------------------ backend

type Reply<R> = std::sync::mpsc::Sender<R>;

/// What the owner thread is asked to do.
enum Job {
    Describe(Reply<String>),
    Configure(String, Reply<Result<(), ToolError>>),
    /// A command; through the component's typed interface when it has one
    /// (the interface's name), else the JSON invoke.
    Invoke(wit::Context, String, Value, Option<String>, Reply<Result<Value, ToolError>>),
    State(Option<u64>, Reply<Option<Value>>),
    /// A request for the component's wasi:http export: the head goes back
    /// first, then the body in chunks, until done or the client is gone.
    Http(
        tool_contract::HttpRequest,
        futures::channel::oneshot::Receiver<()>,
        futures::channel::oneshot::Sender<tool_contract::HttpHead>,
        futures::channel::mpsc::Sender<Vec<u8>>,
    ),
    Shutdown(Reply<()>),
    /// The host is exiting: ask the program to suspend for the next run
    /// (--snapshot), and answer once the snapshot is saved.
    Persist(Reply<()>),
    /// Grant this data directory: the instance is suspended (keeping what
    /// it will) and the next one starts with the grant.
    Regrant(std::path::PathBuf, Reply<()>),
    McpOut(Writer),
    Subscribe(Arc<dyn Fn(u64) + Send + Sync>),
}

/// A loaded component, as every interface's backend. The component lives
/// on its own thread, whose event loop (wasmtime's run_concurrent) keeps
/// running between calls, so work the component started carries on by
/// itself (WASI 0.3). This handle queues calls to that thread.
pub struct Wasm {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
    /// The component's typed commands interface (describe's `wit`).
    typed: Option<String>,
    /// Whether the component exports wasi:http/handler.
    has_http: bool,
}

/// Forwards checked requests to the component's HTTP export.
struct WasmHttp {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

impl tool_contract::HttpForward for WasmHttp {
    fn forward(
        &self,
        req: tool_contract::HttpRequest,
        gone: futures::channel::oneshot::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<tool_contract::HttpReply>> + Send>> {
        let (head_tx, head_rx) = futures::channel::oneshot::channel();
        let (chunk_tx, chunk_rx) = futures::channel::mpsc::channel(16);
        let sent = self.jobs.send(Job::Http(req, gone, head_tx, chunk_tx)).is_ok();
        Box::pin(async move {
            if !sent {
                return None;
            }
            Some((head_rx.await.ok()?, chunk_rx))
        })
    }
}

fn from_wit_err(e: wit::ToolError) -> ToolError {
    ToolError { code: e.code, message: e.message, details: e.details.and_then(|d| serde_json::from_str(&d).ok()) }
}

fn trap(e: wasmtime::Error) -> ToolError {
    ToolError::new("internal", format!("the component failed: {e:#}"))
}

impl Wasm {
    fn ask<R>(&self, job: impl FnOnce(Reply<R>) -> Job) -> R {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = self.jobs.send(job(tx));
        rx.recv().expect("the component's thread has stopped")
    }

    /// Calls a command through the component's typed export (describe's
    /// `wit`), for programs that want it; the generated interfaces use the
    /// JSON invoke. Records come back with every field, as WIT has them.
    /// Commands the typed interface doesn't export go through invoke.
    pub fn invoke_typed(&mut self, cx: &Context, command: &str, input: Value) -> Result<Value, ToolError> {
        let wcx = wit::Context {
            by: cx.by.clone(),
            session: cx.session.clone(),
            interactive: cx.interactive,
            request: cx.request.as_ref().map(|r| r.to_string()),
        };
        let typed = self.typed.clone();
        self.ask(|r| Job::Invoke(wcx, command.to_string(), input, typed, r))
    }

    /// Loads a component as a library would (no command line): for tests
    /// and programs embedding a tool.
    pub fn load(path: &str, grants: &Grants, lifecycle: Lifecycle) -> Result<Wasm> {
        let ext = Extension::none("tool-host");
        load(path, grants, ext.state, ext.imports, lifecycle)
    }

}

impl Backend for Wasm {
    fn describe(&mut self) -> Description {
        let text = self.ask(Job::Describe);
        let desc: Description = serde_json::from_str(&text).expect("the component's describe() isn't a description");
        self.typed = desc.wit.clone();
        desc
    }

    fn configure(&mut self, config: &Map<String, Value>) -> Result<(), ToolError> {
        let settings = Value::Object(config.clone()).to_string();
        self.ask(|r| Job::Configure(settings, r))
    }

    fn invoke(&mut self, cx: &Context, command: &str, input: Value) -> Result<Value, ToolError> {
        let wcx = wit::Context {
            by: cx.by.clone(),
            session: cx.session.clone(),
            interactive: cx.interactive,
            request: cx.request.as_ref().map(|r| r.to_string()),
        };
        // The generated interfaces call the JSON invoke, as the native
        // build does, so their output is the same in both runtimes (typed
        // WIT always writes every field). See invoke_typed.
        self.ask(|r| Job::Invoke(wcx, command.to_string(), input, None, r))
    }

    fn shutdown(&mut self) {
        self.ask(Job::Shutdown)
    }

    fn state(&mut self, since: Option<u64>) -> Option<Value> {
        self.ask(|r| Job::State(since, r))
    }



    fn subscribe(&mut self, on_change: Arc<dyn Fn(u64) + Send + Sync>) {
        let _ = self.jobs.send(Job::Subscribe(on_change));
    }

    fn clone_handle(&self) -> Option<Box<dyn Backend + Send>> {
        Some(Box::new(Wasm { jobs: self.jobs.clone(), typed: self.typed.clone(), has_http: self.has_http }))
    }
}

/// When the host suspends instances, besides when the program asks.
#[derive(Debug, Default, Clone)]
pub struct Lifecycle {
    /// Ask the program to suspend after this long without calls.
    pub idle: Option<std::time::Duration>,
    /// Ask the program to suspend when its linear memory passes this many
    /// bytes, so a fresh instance starts small. Only when it's at least
    /// twice what the instance had once resumed: below that, a fresh one
    /// wouldn't be much smaller, and recycling would thrash.
    pub recycle_above: Option<usize>,
    /// Keep the snapshot here, so the next run resumes from it.
    pub snapshot_file: Option<std::path::PathBuf>,
}

/// How long to wait before asking again after a refusal for memory.
const MEMORY_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// Why an instance's event loop ended.
enum Outcome {
    /// Every handle is gone: the host is exiting.
    Closed,
    /// Suspended, keeping this; answer the reply once it's saved.
    Suspended(Option<Vec<u8>>, Option<Reply<()>>),
}

/// The instance's exports.
struct Exports {
    tool: ToolCore,
    instance: wasmtime::component::Instance,
    http: Option<wasmtime_wasi_http::p3::bindings::Service>,
    lifecycle: Option<lifecycle::ToolLifecycle>,
}

fn log(message: impl std::fmt::Display) {
    eprintln!("tool-host: {message}");
}

/// Asks the program to suspend; Some(what it keeps) if it agreed.
async fn ask_suspend<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    ex: &Exports,
    reason: lifecycle::SuspendReason,
) -> Option<Option<Vec<u8>>> {
    let l = ex.lifecycle.as_ref()?;
    match l.opinionated_tool_lifecycle().call_suspend(a, reason).await {
        Ok(lifecycle::SuspendReply::Accept(keep)) => Some(keep),
        Ok(lifecycle::SuspendReply::Refuse(why)) => {
            let why_asked = match reason {
                lifecycle::SuspendReason::Idle => "idle",
                lifecycle::SuspendReason::Memory => "memory",
                lifecycle::SuspendReason::Exit => "exit",
            };
            log(format!("asked to suspend ({why_asked}), the program refused: {why}"));
            None
        }
        Err(e) => {
            log(format!("suspend failed: {e:#}"));
            None
        }
    }
}

/// The owner thread's loop, inside the store's event loop: each queued
/// call runs concurrently (a call waiting on HTTP doesn't hold up the
/// others), and the component's own tasks run alongside. It ends when the
/// handles are gone, or the instance is suspended: then calls in progress
/// finish, open HTTP streams are dropped, and later jobs wait in the queue
/// for the next instance.
async fn serve_jobs<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    ex: &Exports,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Job>,
    first: Option<Job>,
    opts: &Lifecycle,
    wake: Arc<tokio::sync::Notify>,
) -> wasmtime::Result<Outcome> {
    use futures::StreamExt;
    use lifecycle::SuspendReason;
    use std::time::Instant;
    let mut running = futures::stream::FuturesUnordered::new();
    // HTTP requests, apart: an event stream never ends by itself.
    let mut streams = futures::stream::FuturesUnordered::new();
    let mut suspending: Option<(Option<Vec<u8>>, Option<Reply<()>>)> = None;
    let mut last_call = Instant::now();
    let mut memory_ask = Instant::now();
    let suspendable = ex.lifecycle.is_some();
    // What a fresh instance takes, once resumed and warmed up: the floor
    // for recycling.
    let fresh = a.with(|mut s| s.data_mut().memory.largest);
    let push = |job: Job, running: &mut futures::stream::FuturesUnordered<_>, streams: &mut futures::stream::FuturesUnordered<_>| {
        if matches!(job, Job::Http(..)) { streams.push(run_job(a, ex, job)) } else { running.push(run_job(a, ex, job)) }
    };
    if let Some(job) = first {
        push(job, &mut running, &mut streams);
    }
    loop {
        if suspending.is_some() && running.is_empty() {
            let (keep, reply) = suspending.take().unwrap();
            return Ok(Outcome::Suspended(keep, reply));
        }
        let idle_at = opts.idle.map(|d| tokio::time::Instant::from_std(last_call + d));
        let watch_idle = suspendable && idle_at.is_some() && running.is_empty() && suspending.is_none();
        tokio::select! {
            job = rx.recv(), if suspending.is_none() => {
                last_call = Instant::now();
                match job {
                    None => break,
                    Some(Job::Persist(r)) => match ask_suspend(a, ex, SuspendReason::Exit).await {
                        Some(keep) => suspending = Some((keep, Some(r))),
                        None => { let _ = r.send(()); }
                    },
                    Some(Job::Regrant(dir, r)) => {
                        a.with(|mut s| {
                            let mut sh = s.data_mut().shared.lock().unwrap();
                            sh.data_dir = Some(dir);
                            sh.quiet = true;
                        });
                        // Keep what it will; without lifecycle, it has only
                        // just started, so there's nothing to lose.
                        let keep = if suspendable { ask_suspend(a, ex, SuspendReason::Exit).await.flatten() } else { None };
                        suspending = Some((keep, Some(r)));
                    }
                    Some(job) => push(job, &mut running, &mut streams),
                }
            }
            Some(()) = running.next(), if !running.is_empty() => {
                last_call = Instant::now();
                let size = a.with(|mut s| s.data_mut().memory.largest);
                let over = opts.recycle_above.is_some_and(|limit| size > limit && size >= 2 * fresh);
                if over && suspendable && running.is_empty() && suspending.is_none() && Instant::now() >= memory_ask {
                    match ask_suspend(a, ex, SuspendReason::Memory).await {
                        Some(keep) => {
                            log(format!("recycling: linear memory is {} MiB", size >> 20));
                            suspending = Some((keep, None));
                        }
                        None => memory_ask = Instant::now() + MEMORY_RETRY,
                    }
                }
            }
            Some(()) = streams.next(), if !streams.is_empty() => {}
            () = wake.notified() => {
                if let Some(keep) = a.with(|mut s| s.data_mut().suspend_request.take()) {
                    if suspendable && suspending.is_none() {
                        log("the program asked to be suspended");
                        suspending = Some((keep, None));
                    }
                }
            }
            () = tokio::time::sleep_until(idle_at.unwrap_or_else(tokio::time::Instant::now)), if watch_idle => {
                match ask_suspend(a, ex, SuspendReason::Idle).await {
                    Some(keep) => suspending = Some((keep, None)),
                    None => last_call = Instant::now(),
                }
            }
        }
    }
    while running.next().await.is_some() {}
    Ok(Outcome::Closed)
}

async fn run_job<X: Send + 'static>(a: &wasmtime::component::Accessor<State<X>>, ex: &Exports, job: Job) {
    let t = ex.tool.opinionated_tool_tool();
    match job {
        Job::Describe(r) => {
            let _ = r.send(t.call_describe(a).await.unwrap_or_default());
        }
        Job::Configure(settings, r) => {
            let out = t.call_configure(a, settings.clone()).await.map_err(trap).and_then(|x| x.map_err(from_wit_err));
            if out.is_ok() {
                a.with(|mut s| s.data_mut().shared.lock().unwrap().settings = Some(settings));
            }
            let _ = r.send(out);
        }
        // Commands the typed interface doesn't export (their types don't
        // fit WIT well, e.g. maps) go through the JSON invoke.
        Job::Invoke(cx, command, input, Some(iface), r) if typed_func(a, &ex.instance, &iface, &command).is_some() => {
            let _ = r.send(typed_invoke(a, &ex.instance, &iface, cx, &command, input).await);
        }
        Job::Invoke(cx, command, input, _, r) => {
            let out = t.call_invoke(a, cx, command, input.to_string()).await.map_err(trap).and_then(|x| x.map_err(from_wit_err));
            let _ = r.send(out.and_then(|s| {
                serde_json::from_str(&s).map_err(|e| ToolError::new("internal", format!("the component's result isn't JSON: {e}")))
            }));
        }
        Job::State(since, r) => {
            let text = t.call_state(a, since).await.ok().flatten();
            let _ = r.send(text.and_then(|t| serde_json::from_str(&t).ok()));
        }
        Job::Http(req, gone, head, chunks) => {
            if let Some(service) = &ex.http {
                forward(a, service, req, gone, head, chunks).await;
            }
        }
        Job::Shutdown(r) => {
            let _ = t.call_shutdown(a).await;
            let _ = r.send(());
        }
        // Handled by serve_jobs.
        Job::Persist(r) | Job::Regrant(_, r) => {
            let _ = r.send(());
        }
        Job::McpOut(out) => a.with(|mut s| s.data_mut().shared.lock().unwrap().mcp_out = Some((out, 0))),
        Job::Subscribe(f) => a.with(|mut s| s.data_mut().shared.lock().unwrap().listener = Some(f)),
    }
}

// ------------------------------------------------------------------ extension

type AddImports<X> = Box<dyn FnOnce(&mut Linker<State<X>>) -> wasmtime::Result<()> + Send>;
type Customs = Box<dyn FnOnce(&Description) -> Vec<tool_cli::Custom<'static>>>;

/// What a tool-specific host adds to the generic one.
pub struct Extension<X: 'static> {
    /// The host's name, for usage and messages.
    pub name: String,
    /// Grant flags it accepts, with their help (e.g. ("--audio", "let the
    /// tool play sound")). Given ones arrive in [`Grants::flags`].
    pub grant_flags: Vec<(String, String)>,
    /// Makes the extension's state, knowing what was granted: once per
    /// instance (a suspended one is replaced by a fresh one).
    pub state: Box<dyn Fn(&Grants) -> X + Send>,
    /// Adds the extension's imports to the linker.
    pub imports: AddImports<X>,
    /// Custom commands, given the component's description.
    pub commands: Customs,
}

impl Extension<()> {
    /// Nothing added: wasm-tool.
    pub fn none(name: &str) -> Self {
        Extension {
            name: name.into(),
            grant_flags: vec![],
            state: Box::new(|_| ()),
            imports: Box::new(|_| Ok(())),
            commands: Box::new(|_| vec![]),
        }
    }
}

/// The typed export for a command, if the interface has one.
fn typed_func<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    instance: &wasmtime::component::Instance,
    iface: &str,
    command: &str,
) -> Option<wasmtime::component::Func> {
    a.with(|mut s| {
        let i = instance.get_export_index(&mut s, None, iface)?;
        let f = instance.get_export_index(&mut s, Some(&i), command)?;
        instance.get_func(&mut s, f)
    })
}

/// Calls a command through the component's typed interface: JSON in,
/// converted to WIT values by the function's own types; WIT out, converted
/// back. Commands without input take only the context.
async fn typed_invoke<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    instance: &wasmtime::component::Instance,
    iface: &str,
    cx: wit::Context,
    command: &str,
    input: Value,
) -> Result<Value, ToolError> {
    use wasmtime::component::Val;
    let func = typed_func(a, instance, iface, command)
        .ok_or_else(|| ToolError::new(tool_contract::codes::UNKNOWN_COMMAND, format!("{iface} has no function {command}")))?;
    let (params, results): (Vec<(String, wasmtime::component::Type)>, Vec<wasmtime::component::Type>) = a.with(|s| {
        let ty = func.ty(&s);
        (ty.params().map(|(n, t)| (n.to_string(), t)).collect(), ty.results().collect())
    });
    let mut args = vec![];
    for (name, ty) in &params {
        let v = if name == "cx" {
            let cx = json!({"by": cx.by, "session": cx.session, "interactive": cx.interactive, "request": cx.request});
            typed::to_val(ty, Some(&cx), "cx")
        } else {
            typed::to_val(ty, Some(&input), "input")
        };
        args.push(v.map_err(|e| ToolError::new(tool_contract::codes::INVALID_ARGUMENT, format!("invalid arguments: {e}")))?);
    }
    if params.len() == 1 && input.as_object().is_some_and(|o| !o.is_empty()) {
        return Err(ToolError::new(tool_contract::codes::INVALID_ARGUMENT, format!("{{cmd:{command}}} takes no arguments")));
    }
    let mut out = vec![Val::Bool(false); results.len()];
    func.call_concurrent(a, &args, &mut out).await.map_err(trap)?;
    match (results.first(), out.first()) {
        (Some(wasmtime::component::Type::Result(rt)), Some(Val::Result(Ok(v)))) => {
            Ok(match (rt.ok(), v) {
                (Some(t), Some(v)) => typed::to_json(&t, v),
                _ => Value::Null,
            })
        }
        (_, Some(Val::Result(Err(Some(e))))) => Err(typed::to_tool_error(e)),
        _ => Err(ToolError::new("internal", format!("{command} returned something other than result<_, tool-error>"))),
    }
}

/// Calls the component's HTTP export with a checked request, sends the
/// head back, then the body chunk by chunk. An event stream's body never
/// ends; when the client goes away, sending fails, the body is dropped,
/// and the component sees its stream closed.
async fn forward<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    service: &wasmtime_wasi_http::p3::bindings::Service,
    req: tool_contract::HttpRequest,
    mut gone: futures::channel::oneshot::Receiver<()>,
    head: futures::channel::oneshot::Sender<tool_contract::HttpHead>,
    mut chunks: futures::channel::mpsc::Sender<Vec<u8>>,
) {
    use futures::SinkExt;
    use http_body_util::BodyExt;
    let host = req.header("Host").unwrap_or("localhost").to_string();
    let mut target = req.path.clone();
    if !req.query.is_empty() {
        let q: Vec<String> = req.query.iter().map(|(k, v)| format!("{}={}", encode(k), encode(v))).collect();
        target = format!("{target}?{}", q.join("&"));
    }
    let mut builder = http::Request::builder().method(req.method.as_str()).uri(format!("http://{host}{target}"));
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    let Ok(request) = builder.body(http_body_util::Full::new(bytes::Bytes::from(req.body)).map_err(|e| match e {})) else {
        return;
    };
    let (request, _io) = a.with(|mut s| {
        let h = WasiHttpView::http(s.data_mut());
        wasmtime_wasi_http::p3::Request::from_http(h.hooks, request)
    });
    let response = match service.handle(a, request).await {
        Ok(Ok(r)) => r,
        other => {
            eprintln!("the component's HTTP handler failed: {:?}", other.err());
            return;
        }
    };
    let response = match a.with(|mut s| response.into_http(&mut s, async { Ok(()) })) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("the component's response couldn't be read: {e:#}");
            return;
        }
    };
    let (parts, mut body) = response.into_parts();
    let headers = parts
        .headers
        .iter()
        .filter(|(k, _)| *k != http::header::CONTENT_LENGTH && *k != http::header::TRANSFER_ENCODING)
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string()))
        .collect();
    if head.send(tool_contract::HttpHead { status: parts.status.as_u16(), headers }).is_err() {
        return;
    }
    // Until the body ends or the client goes away; then dropping the body
    // closes the component's stream, so it hears the page has gone.
    loop {
        let frame = futures::future::select(Box::pin(body.frame()), &mut gone).await;
        let futures::future::Either::Left((Some(Ok(frame)), _)) = frame else { break };
        if let Ok(data) = frame.into_data() {
            if chunks.send(data.to_vec()).await.is_err() {
                break;
            }
        }
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Starts the owner thread, which loads the component and then serves
/// jobs until the handle is dropped. Engine, linker and component are made
/// once; instances come and go: a suspended one is dropped, and the next
/// job that needs the component starts a fresh one, configured as the last
/// was and resumed from the snapshot.
fn load<X: Send + 'static>(
    path: &str,
    grants: &Grants,
    state: Box<dyn Fn(&Grants) -> X + Send>,
    imports: AddImports<X>,
    opts: Lifecycle,
) -> Result<Wasm> {
    let (jobs, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let http_jobs = jobs.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<bool>>();
    let (path, grants) = (path.to_string(), grants.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
        rt.block_on(async move {
            let w = |e: wasmtime::Error| anyhow!("{e:#}");
            let setup = || -> Result<_> {
                let mut config = wasmtime::Config::new();
                config.wasm_component_model_async(true);
                let engine = Engine::new(&config).map_err(w)?;
                let component = Component::from_file(&engine, &path).map_err(|e| anyhow!("loading {path}: {e:#}"))?;
                let mut linker: Linker<State<X>> = Linker::new(&engine);
                wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(w)?;
                wasmtime_wasi::p3::add_to_linker(&mut linker).map_err(w)?;
                wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker).map_err(w)?;
                wasmtime_wasi_http::p3::add_to_linker(&mut linker).map_err(w)?;
                ToolCore::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s).map_err(w)?;
                imports(&mut linker).map_err(w)?;
                Ok((engine, component, linker))
            };
            let (engine, component, linker) = match setup() {
                Ok(x) => x,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let shared = Arc::new(Mutex::new(Shared { data_dir: grants.data_dir.clone(), ..Default::default() }));
            let mut kept: Option<Vec<u8>> = opts.snapshot_file.as_ref().and_then(|f| std::fs::read(f).ok());
            let mut ready = Some(ready_tx);
            loop {
                // After the first instance, start one only when a job needs
                // the component.
                let mut first = None;
                if ready.is_none() {
                    while first.is_none() {
                        match rx.recv().await {
                            None => return,
                            Some(Job::McpOut(o)) => shared.lock().unwrap().mcp_out = Some((o, 0)),
                            Some(Job::Subscribe(f)) => shared.lock().unwrap().listener = Some(f),
                            // Suspended: nothing running, the snapshot saved.
                            Some(Job::Shutdown(r) | Job::Persist(r)) => {
                                let _ = r.send(());
                            }
                            Some(Job::Regrant(dir, r)) => {
                                shared.lock().unwrap().data_dir = Some(dir);
                                let _ = r.send(());
                            }
                            Some(job) => first = Some(job),
                        }
                    }
                }
                let started = std::time::Instant::now();
                let instantiate = async {
                    // No environment, no sockets, no files but the granted
                    // data directory: only stderr, for the component's own
                    // panics.
                    let mut wasi = WasiCtx::builder();
                    wasi.inherit_stderr();
                    let data_dir = shared.lock().unwrap().data_dir.clone();
                    if let Some(dir) = &data_dir {
                        wasi.preopened_dir(dir, DATA_MOUNT, wasmtime_wasi::FsPerms::ReadWrite)
                            .map_err(|e| anyhow!("granting {}: {e:#}", dir.display()))?;
                    }
                    let wasi = wasi.build();
                    let wake = Arc::new(tokio::sync::Notify::new());
                    let data = State {
                        wasi,
                        http: WasiHttpCtx::new(),
                        table: ResourceTable::new(),
                        hooks: Hooks { allow: grants.http.clone() },
                        secrets: grants.secrets.clone(),
                        shared: shared.clone(),
                        http_jobs: None,
                        suspend_request: None,
                        suspend_wake: wake.clone(),
                        memory: MemoryWatch::default(),
                        ext: state(&grants),
                    };
                    let mut store = Store::new(&engine, data);
                    store.limiter(|s| &mut s.memory);
                    let instance = linker.instantiate_async(&mut store, &component).await.map_err(|e| {
                        anyhow!("{e:#}. If the component imports an interface this host doesn't provide, run it with a host that does, or compose a stub for it in.")
                    })?;
                    let tool = ToolCore::new(&mut store, &instance).map_err(w)?;
                    // The component's HTTP interface and lifecycle, if it
                    // exports them.
                    let http = wasmtime_wasi_http::p3::bindings::Service::new(&mut store, &instance).ok();
                    if http.is_some() {
                        store.data_mut().http_jobs = Some(http_jobs.clone());
                    }
                    let lifecycle = lifecycle::ToolLifecycle::new(&mut store, &instance).ok();
                    Ok::<_, anyhow::Error>((store, Exports { tool, instance, http, lifecycle }, wake))
                };
                let (mut store, ex, wake) = match instantiate.await {
                    Ok(x) => x,
                    Err(e) => {
                        match ready.take() {
                            Some(r) => {
                                let _ = r.send(Err(e));
                            }
                            None => log(format!("couldn't start a fresh instance: {e:#}")),
                        }
                        return;
                    }
                };
                if let Some(r) = ready.take() {
                    let _ = r.send(Ok(ex.http.is_some()));
                }
                let resuming = kept.take();
                let result = store
                    .run_concurrent(async |a| {
                        let settings = shared.lock().unwrap().settings.clone();
                        if let Some(settings) = settings {
                            if let Err(e) = ex.tool.opinionated_tool_tool().call_configure(a, settings).await {
                                log(format!("replaying configure failed: {e:#}"));
                            }
                        }
                        let listening = shared.lock().unwrap().serving.as_ref().map(|l| l.url.clone());
                        let resumed = match (resuming, &ex.lifecycle) {
                            (Some(snapshot), Some(l)) => {
                                let n = snapshot.len();
                                match l.opinionated_tool_lifecycle().call_resume(a, snapshot, listening).await {
                                    Ok(Ok(())) => {
                                        if !std::mem::take(&mut shared.lock().unwrap().quiet) {
                                            log(format!("resumed in {:?} ({n} bytes kept)", started.elapsed()));
                                        }
                                        true
                                    }
                                    Ok(Err(e)) => {
                                        log(format!("the program couldn't resume, so it starts empty: {e}"));
                                        false
                                    }
                                    Err(e) => {
                                        log(format!("resume failed, so the instance starts empty: {e:#}"));
                                        false
                                    }
                                }
                            }
                            _ => false,
                        };
                        // When recycling by memory, warm up first (describe
                        // builds the description, which any first call does),
                        // so the fresh size it's measured against is real.
                        if opts.recycle_above.is_some() {
                            let _ = ex.tool.opinionated_tool_tool().call_describe(a).await;
                        }
                        // A fresh, empty instance isn't serving: whatever the
                        // socket was for is gone.
                        if !resumed {
                            shared.lock().unwrap().serving = None;
                        }
                        serve_jobs(a, &ex, &mut rx, first, &opts, wake).await
                    })
                    .await;
                match result {
                    Ok(Ok(Outcome::Closed)) => return,
                    Ok(Ok(Outcome::Suspended(keep, reply))) => {
                        let size = store.data().memory.largest;
                        drop(store);
                        let quiet = shared.lock().unwrap().quiet;
                        if !quiet { log(format!(
                            "suspended; {} MiB of linear memory released, {} kept",
                            size >> 20,
                            keep.as_ref().map_or("nothing".to_string(), |k| format!("{} bytes", k.len()))
                        )); }
                        if keep.is_none() {
                            shared.lock().unwrap().quiet = false;
                        }
                        if let Some(f) = &opts.snapshot_file {
                            let saved = match &keep {
                                Some(k) => std::fs::write(f, k),
                                None => std::fs::remove_file(f).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }),
                            };
                            if let Err(e) = saved {
                                log(format!("couldn't save the snapshot to {}: {e}", f.display()));
                            }
                        }
                        kept = keep;
                        if let Some(r) = reply {
                            let _ = r.send(());
                        }
                    }
                    Ok(Err(e)) | Err(e) => {
                        // A trap: the instance is unusable and its state is
                        // gone. The next job starts a fresh one.
                        log(format!("the component stopped: {e:#}"));
                        kept = None;
                        shared.lock().unwrap().serving = None;
                    }
                }
            }
        });
    });
    let has_http = ready_rx.recv().map_err(|_| anyhow!("the component's thread stopped while loading"))??;
    Ok(Wasm { jobs, typed: None, has_http })
}

/// Runs the host: parses grants, loads the component named on the command
/// line, and hands the rest of the arguments to the generated CLI.
pub fn run<X: Send + 'static>(ext: Extension<X>) -> ! {
    match run_inner(ext) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            println!("{}", json!({"error": {"code": "host_error", "message": format!("{e:#}")}}));
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    }
}

fn run_inner<X: Send + 'static>(ext: Extension<X>) -> Result<i32> {
    let usage = {
        let extra: String = ext.grant_flags.iter().map(|(f, h)| format!("\n  {f:<24} {h}")).collect();
        format!(
            "usage: {} [GRANTS]... [LIFECYCLE]... COMPONENT.wasm COMMAND [ARGS]\n\ngrants:\n  --allow-http HOST[:PORT]  let the component make HTTP requests to this host\n  --secret NAME             let the component read NAME from this host's environment\n  --data-dir DIR            let the component keep files in DIR (it sees it as /data)\n  --data                    the same, with the tool's standard data directory (as its native build uses){extra}\n\nlifecycle (for components exporting opinionated:tool/lifecycle):\n  --suspend-idle SECS       ask the component to suspend after SECS without calls\n  --recycle-above-mb N      ask it to suspend when its memory passes N MiB; a fresh instance starts small\n  --snapshot FILE           resume from FILE, and ask it to suspend into FILE on exit",
            ext.name
        )
    };
    let mut args = std::env::args().skip(1);
    let mut grants = Grants::default();
    let mut life = Lifecycle::default();
    let mut data_standard = false;
    let path = loop {
        match args.next().as_deref() {
            Some("--allow-http") => grants.http.push(args.next().context("--allow-http needs HOST or HOST:PORT")?),
            Some("--secret") => grants.secrets.push(args.next().context("--secret needs a name")?),
            Some("--data") => data_standard = true,
            Some("--data-dir") => {
                let dir = std::path::PathBuf::from(args.next().context("--data-dir needs a directory")?);
                std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
                grants.data_dir = Some(dir.canonicalize()?);
            }
            Some("--suspend-idle") => {
                let secs: f64 = args.next().context("--suspend-idle needs seconds")?.parse().context("--suspend-idle needs seconds")?;
                life.idle = Some(std::time::Duration::from_secs_f64(secs));
            }
            Some("--recycle-above-mb") => {
                let mb: usize = args.next().context("--recycle-above-mb needs a number")?.parse().context("--recycle-above-mb needs a number")?;
                life.recycle_above = Some(mb << 20);
            }
            Some("--snapshot") => life.snapshot_file = Some(args.next().context("--snapshot needs a file")?.into()),
            Some("-h" | "--help") | None => {
                eprintln!("{usage}");
                return Ok(0);
            }
            Some(f) if ext.grant_flags.iter().any(|(g, _)| g == f) => grants.flags.push(f.to_string()),
            Some(p) if p.starts_with('-') => bail!("unknown option {p}\n\n{usage}"),
            Some(p) => break p.to_string(),
        }
    };
    let rest: Vec<String> = args.collect();
    let started = std::time::Instant::now();
    let persist = life.snapshot_file.is_some();
    let mut wasm = load(&path, &grants, ext.state, ext.imports, life)?;
    if std::env::var_os("TOOL_HOST_TIMING").is_some() {
        eprintln!("loaded {path} in {:?}", started.elapsed());
    }
    let desc = wasm.describe();
    if data_standard && grants.data_dir.is_none() {
        let dir = tool_contract::standard_data_dir(&desc.name).context("no standard data directory here (no HOME); use --data-dir")?;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        wasm.ask(|r| Job::Regrant(dir.canonicalize().unwrap_or(dir), r));
    }
    // Described again: what's available can depend on the grants.
    let desc = if data_standard { wasm.describe() } else { desc };

    // In mcp mode, progress for calls with a progress token goes out as MCP
    // notifications; that routing lives in the store, on the owner thread.
    let jobs = wasm.jobs.clone();
    let mcp = tool_mcp::mcp_command(&ext.name, None, Box::new(move |out| {
        let _ = jobs.send(Job::McpOut(out));
    }));
    let mut customs = vec![tool_cli::describe_command(), mcp, tool_serve::command(&ext.name)];
    customs.extend((ext.commands)(&desc));
    let code = tool_cli::run(&desc, rest, &mut wasm, customs);
    if persist {
        wasm.ask(Job::Persist);
    }
    Ok(code)
}
