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
}

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

/// The store's state. `ext` is the extension's: the state behind the
/// imports it added.
pub struct State<X: 'static> {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    hooks: Hooks,
    secrets: Vec<String>,
    /// Set in mcp mode: progress for calls with a progress token goes out
    /// as MCP notifications.
    mcp_out: Option<(Writer, u64)>,
    /// Told new revisions (Backend::subscribe).
    listener: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    /// For serving the component's HTTP export: the queue to its thread
    /// (None if it has no export), and the running listener.
    http_jobs: Option<tokio::sync::mpsc::UnboundedSender<Job>>,
    serving: Option<tool_serve::Listener>,
    pub ext: X,
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
        if let (Some((out, n)), Some(token)) = (self.mcp_out.as_mut(), token) {
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
        if let Some(f) = &self.listener {
            f(revision);
        }
    }

    fn listen(&mut self, port: u16, allow_origins: Vec<String>) -> Result<String, String> {
        let jobs = self.http_jobs.clone().ok_or("this component has no HTTP interface")?;
        self.serving = None;
        let l = tool_serve::listen(Arc::new(WasmHttp { jobs }), port, allow_origins)?;
        let url = l.url.clone();
        self.serving = Some(l);
        Ok(url)
    }

    fn unlisten(&mut self) {
        self.serving = None;
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
        let typed = self.typed.clone();
        self.ask(|r| Job::Invoke(wcx, command.to_string(), input, typed, r))
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

/// The owner thread's loop, inside the store's event loop: each queued
/// call runs concurrently (a call waiting on HTTP doesn't hold up the
/// others), and the component's own tasks run alongside.
async fn serve_jobs<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    tool: &ToolCore,
    instance: &wasmtime::component::Instance,
    http: Option<&wasmtime_wasi_http::p3::bindings::Service>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Job>,
) -> wasmtime::Result<()> {
    use futures::StreamExt;
    let mut running = futures::stream::FuturesUnordered::new();
    loop {
        tokio::select! {
            job = rx.recv() => match job {
                Some(job) => running.push(run_job(a, tool, instance, http, job)),
                None => break,
            },
            Some(()) = running.next(), if !running.is_empty() => {}
        }
    }
    while running.next().await.is_some() {}
    Ok(())
}

async fn run_job<X: Send + 'static>(
    a: &wasmtime::component::Accessor<State<X>>,
    tool: &ToolCore,
    instance: &wasmtime::component::Instance,
    http: Option<&wasmtime_wasi_http::p3::bindings::Service>,
    job: Job,
) {
    let t = tool.opinionated_tool_tool();
    {
        match job {
            Job::Describe(r) => {
                let _ = r.send(t.call_describe(a).await.unwrap_or_default());
            }
            Job::Configure(settings, r) => {
                let _ = r.send(t.call_configure(a, settings).await.map_err(trap).and_then(|x| x.map_err(from_wit_err)));
            }
            Job::Invoke(cx, command, input, Some(iface), r) => {
                let _ = r.send(typed_invoke(a, instance, &iface, cx, &command, input).await);
            }
            Job::Invoke(cx, command, input, None, r) => {
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
                if let Some(service) = http {
                    forward(a, service, req, gone, head, chunks).await;
                }
            }
            Job::Shutdown(r) => {
                let _ = t.call_shutdown(a).await;
                let _ = r.send(());
            }
            Job::McpOut(out) => a.with(|mut s| s.data_mut().mcp_out = Some((out, 0))),
            Job::Subscribe(f) => a.with(|mut s| s.data_mut().listener = Some(f)),
        }
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
    /// Makes the extension's state, knowing what was granted.
    pub state: Box<dyn FnOnce(&Grants) -> X + Send>,
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
    let func = a
        .with(|mut s| {
            let i = instance.get_export_index(&mut s, None, iface)?;
            let f = instance.get_export_index(&mut s, Some(&i), command)?;
            instance.get_func(&mut s, f)
        })
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
/// jobs until the handle is dropped.
fn load<X: Send + 'static>(path: &str, grants: &Grants, state: Box<dyn FnOnce(&Grants) -> X + Send>, imports: AddImports<X>) -> Result<Wasm> {
    let (jobs, rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let http_jobs = jobs.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<bool>>();
    let (path, grants) = (path.to_string(), grants.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
        rt.block_on(async move {
            let w = |e: wasmtime::Error| anyhow!("{e:#}");
            let setup = async {
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
                // No files, no environment, no sockets: only stderr, for
                // the component's own panics.
                let wasi = WasiCtx::builder().inherit_stderr().build();
                let data = State {
                    wasi,
                    http: WasiHttpCtx::new(),
                    table: ResourceTable::new(),
                    hooks: Hooks { allow: grants.http.clone() },
                    secrets: grants.secrets.clone(),
                    mcp_out: None,
                    listener: None,
                    http_jobs: None,
                    serving: None,
                    ext: state(&grants),
                };
                let mut store = Store::new(&engine, data);
                let instance = linker.instantiate_async(&mut store, &component).await.map_err(|e| {
                    anyhow!("{e:#}. If the component imports an interface this host doesn't provide, run it with a host that does, or compose a stub for it in.")
                })?;
                let tool = ToolCore::new(&mut store, &instance).map_err(w)?;
                let instance_handle = instance;
                // The component's HTTP interface, if it exports one.
                let http = wasmtime_wasi_http::p3::bindings::Service::new(&mut store, &instance).ok();
                if http.is_some() {
                    store.data_mut().http_jobs = Some(http_jobs.clone());
                }
                Ok::<_, anyhow::Error>((store, tool, instance_handle, http))
            };
            match setup.await {
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
                Ok((mut store, tool, instance, http)) => {
                    let _ = ready_tx.send(Ok(http.is_some()));
                    let result = store.run_concurrent(async |a| serve_jobs(a, &tool, &instance, http.as_ref(), rx).await).await;
                    if let Err(e) | Ok(Err(e)) = result.map(|r| r.map(|_| ())) {
                        eprintln!("the component stopped: {e:#}");
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
            "usage: {} [GRANTS]... COMPONENT.wasm COMMAND [ARGS]\n\ngrants:\n  --allow-http HOST[:PORT]  let the component make HTTP requests to this host\n  --secret NAME             let the component read NAME from this host's environment{extra}",
            ext.name
        )
    };
    let mut args = std::env::args().skip(1);
    let mut grants = Grants::default();
    let path = loop {
        match args.next().as_deref() {
            Some("--allow-http") => grants.http.push(args.next().context("--allow-http needs HOST or HOST:PORT")?),
            Some("--secret") => grants.secrets.push(args.next().context("--secret needs a name")?),
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
    let mut wasm = load(&path, &grants, ext.state, ext.imports)?;
    if std::env::var_os("TOOL_HOST_TIMING").is_some() {
        eprintln!("loaded {path} in {:?}", started.elapsed());
    }
    let desc = wasm.describe();

    // In mcp mode, progress for calls with a progress token goes out as MCP
    // notifications; that routing lives in the store, on the owner thread.
    let jobs = wasm.jobs.clone();
    let mcp = tool_mcp::mcp_command(&ext.name, None, Box::new(move |out| {
        let _ = jobs.send(Job::McpOut(out));
    }));
    let mut customs = vec![tool_cli::describe_command(), mcp, tool_serve::command(&ext.name)];
    customs.extend((ext.commands)(&desc));
    Ok(tool_cli::run(&desc, rest, &mut wasm, customs))
}
