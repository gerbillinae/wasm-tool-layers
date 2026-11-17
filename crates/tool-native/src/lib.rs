//! A tool's native build: its core on an owner thread, every interface
//! talking to it through a [`Backend`] handle, and native implementations of
//! the generic ports. See opinionated/optional/wasm.md (Rule 2).
//!
//! A tool's `main` is then its own ports plus one call:
//!
//! ```ignore
//! fn main() {
//!     let progress = tool_native::ports::RoutedProgress::default();
//!     let p = progress.clone();
//!     let backend = tool_native::start(move |services| MyTool::new(my_ports(p, services)));
//!     tool_native::run(backend, progress, App { name: "my-tool", instructions: None, commands: vec![] });
//! }
//! ```

pub mod ports;

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};
use tool_contract::{Backend, Context, Description, HttpForward, HttpReply, HttpRequest, LocalFuture, ToolError};
use tool_ports::{Listener, Notify, Spawn, Timer};

/// What a tool's core offers its native build.
pub trait NativeTool: Send + Sync + 'static {
    fn describe(&self) -> Description;
    fn configure(&self, settings: Map<String, Value>) -> Result<(), ToolError>;
    fn invoke(self: Arc<Self>, cx: &Context, command: &str, input: Value) -> LocalFuture<Result<Value, ToolError>>;
    /// The shared state (with `revision`) and the changes after `since`.
    fn state(&self, _since: Option<u64>) -> Option<Value> {
        None
    }
    fn shutdown(&self) {}
    /// The tool's HTTP interface (tool-rest), if it has one.
    fn http(self: Arc<Self>) -> Option<Rc<dyn tool_rest::Service>> {
        None
    }
}

type Job<T> = Box<dyn FnOnce(Arc<T>) -> LocalFuture<()> + Send>;
type OnChange = Arc<Mutex<Option<Arc<dyn Fn(u64) + Send + Sync>>>>;

/// The ports the native build provides for the core, besides the tool's
/// own: background work and timers on the owner thread, change
/// notification, and serving the HTTP interface.
pub struct Services {
    pub timer: Box<dyn Timer>,
    pub spawn: Box<dyn Spawn>,
    pub notify: Box<dyn Notify>,
    pub listener: Box<dyn Listener>,
}

/// A handle on the core, which lives on its own thread inside tokio's
/// single-threaded runtime, where its background work runs between calls.
/// Each call is a job queued to that thread, run as its own task, so calls
/// overlap while they wait.
pub struct Native<T: NativeTool> {
    jobs: tokio::sync::mpsc::UnboundedSender<Job<T>>,
    on_change: OnChange,
}

/// Starts the owner thread, making the core there with `make`.
pub fn start<T: NativeTool>(make: impl FnOnce(Services) -> Arc<T> + Send + 'static) -> Native<T> {
    let (jobs, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job<T>>();
    let on_change: OnChange = Arc::default();
    let services = Services {
        timer: Box::new(ports::TokioTimer),
        spawn: Box::new(ports::LocalSpawn),
        notify: Box::new(ports::Subscribers(on_change.clone())),
        listener: Box::new(NativeListener { jobs: jobs.clone(), running: Mutex::default() }),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let core = make(services);
            while let Some(job) = rx.recv().await {
                tokio::task::spawn_local(job(core.clone()));
            }
        });
    });
    Native { jobs, on_change }
}

impl<T: NativeTool> Native<T> {
    /// Runs `f` on the core's thread and waits for its result.
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(Arc<T>) -> LocalFuture<R> + Send + 'static) -> R {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = self.jobs.send(Box::new(move |core| {
            Box::pin(async move {
                let _ = tx.send(f(core).await);
            })
        }));
        rx.recv().expect("the core's thread has stopped")
    }
}

impl<T: NativeTool> Backend for Native<T> {
    fn describe(&mut self) -> Description {
        self.call(|c| Box::pin(async move { c.describe() }))
    }
    fn configure(&mut self, config: &Map<String, Value>) -> Result<(), ToolError> {
        let config = config.clone();
        self.call(move |c| Box::pin(async move { c.configure(config) }))
    }
    fn invoke(&mut self, cx: &Context, command: &str, input: Value) -> Result<Value, ToolError> {
        let (cx, command) = (cx.clone(), command.to_string());
        self.call(move |c| c.invoke(&cx, &command, input))
    }
    fn shutdown(&mut self) {
        self.call(|c| Box::pin(async move { c.shutdown() }));
    }
    fn state(&mut self, since: Option<u64>) -> Option<Value> {
        self.call(move |c| Box::pin(async move { c.state(since) }))
    }
    fn subscribe(&mut self, on_change: Arc<dyn Fn(u64) + Send + Sync>) {
        *self.on_change.lock().unwrap() = Some(on_change);
    }
    fn clone_handle(&self) -> Option<Box<dyn Backend + Send>> {
        Some(Box::new(Native { jobs: self.jobs.clone(), on_change: self.on_change.clone() }))
    }
}

/// The HTTP interface natively: tool-rest on the core's thread, the body
/// streamed back to the listener in chunks, stopping when the client goes.
struct NativeHttp<T: NativeTool> {
    jobs: tokio::sync::mpsc::UnboundedSender<Job<T>>,
}

impl<T: NativeTool> HttpForward for NativeHttp<T> {
    fn forward(
        &self,
        req: HttpRequest,
        gone: futures::channel::oneshot::Receiver<()>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<HttpReply>> + Send>> {
        let (head_tx, head_rx) = futures::channel::oneshot::channel();
        let (chunk_tx, chunk_rx) = futures::channel::mpsc::channel(16);
        let _ = self.jobs.send(Box::new(move |core: Arc<T>| {
            Box::pin(async move {
                let desc = core.describe();
                if let Some(svc) = core.http() {
                    tool_rest::serve_into(svc, &desc, req, head_tx, chunk_tx, gone).await;
                }
            })
        }));
        Box::pin(async move { Some((head_rx.await.ok()?, chunk_rx)) })
    }
}

/// Serving natively: tool-serve's listener, forwarding to the core's HTTP
/// interface.
struct NativeListener<T: NativeTool> {
    jobs: tokio::sync::mpsc::UnboundedSender<Job<T>>,
    running: Mutex<Option<tool_serve::Listener>>,
}

impl<T: NativeTool> Listener for NativeListener<T> {
    fn listen(&self, port: u16, allow_origins: Vec<String>) -> LocalFuture<Result<String, String>> {
        let fwd = Arc::new(NativeHttp { jobs: self.jobs.clone() });
        let r = tool_serve::listen(fwd, port, allow_origins).map(|l| {
            let url = l.url.clone();
            *self.running.lock().unwrap() = Some(l);
            url
        });
        Box::pin(async move { r })
    }
    fn unlisten(&self) {
        self.running.lock().unwrap().take();
    }
}

/// What a tool adds to the standard CLI.
pub struct App {
    pub name: &'static str,
    /// Instructions for MCP clients.
    pub instructions: Option<String>,
    /// Hand-written commands (e.g. an interactive one), added to the
    /// generated ones or replacing one of the same name.
    pub commands: Vec<tool_cli::Custom<'static>>,
}

/// Runs the CLI: the commands generated from describe(), plus `describe`,
/// `mcp` (progress for MCP calls with a progress token becomes MCP
/// notifications), `serve`, and the tool's own. Exits with its status.
pub fn run<T: NativeTool>(mut backend: Native<T>, progress: ports::RoutedProgress, app: App) -> ! {
    let desc = backend.describe();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut customs = vec![
        tool_cli::describe_command(),
        tool_mcp::mcp_command(app.name, app.instructions, Box::new(move |out| progress.route_mcp(out))),
        tool_serve::command(app.name),
    ];
    customs.extend(app.commands);
    std::process::exit(tool_cli::run(&desc, args, &mut backend, customs));
}
