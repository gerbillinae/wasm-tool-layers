//! A tool's native build: its [`ToolInstance`] on an owner thread, every
//! interface talking to it through a [`Backend`] handle, and native
//! implementations of the generic ports. See opinionated/optional/wasm.md
//! (Rule 2).
//!
//! It mirrors tool-host, which runs the same instance as a component: the
//! same extension shape ([`App`]), the same lifecycle (`--snapshot`), the
//! same data directory flags, and the same answer to a crash (a fresh
//! instance, configured as before). A tool's `main` is its ports plus one
//! call:
//!
//! ```ignore
//! fn main() {
//!     tool_native::run(App { name: "my-tool", instructions: None, commands: Box::new(|_| vec![]) }, |services| {
//!         MyTool::new(my_ports(services))
//!     })
//! }
//! ```

pub mod ports;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use serde_json::{Map, Value, json};
use tool_contract::{Backend, Context, Description, HttpForward, HttpReply, HttpRequest, LocalFuture, SuspendReason, ToolError, ToolInstance};
use tool_ports::{Listener, Notify, Spawn, Timer};

type Job<T> = Box<dyn FnOnce(Arc<T>) -> LocalFuture<()> + Send>;
type OnChange = Arc<Mutex<Option<Arc<dyn Fn(u64) + Send + Sync>>>>;

/// The ports the native build provides for the core, besides the tool's
/// own: background work and timers on the owner thread, change
/// notification, serving the HTTP interface, progress routed per call, and
/// the data directory.
pub struct Services {
    pub timer: Box<dyn Timer>,
    pub spawn: Box<dyn Spawn>,
    pub notify: Box<dyn Notify>,
    pub listener: Box<dyn Listener>,
    /// Progress: stderr, or MCP notifications for calls with a progress token.
    pub progress: ports::RoutedProgress,
    /// For Ports::data_dir.
    pub data_dir: Option<PathBuf>,
}

/// How the native build was asked to run (the options before the command).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub name: String,
    /// The tool's data directory; None with --no-data.
    pub data_dir: Option<PathBuf>,
    /// Resume from this file, and suspend into it on exit.
    pub snapshot: Option<PathBuf>,
}

impl Options {
    /// The defaults for a tool: the standard data directory, no snapshot.
    pub fn new(name: &str) -> Options {
        Options { name: name.into(), data_dir: tool_contract::standard_data_dir(name), snapshot: None }
    }

    /// Takes the options before the command off `args`.
    pub fn parse(name: &str, args: &mut Vec<String>) -> Result<Options, String> {
        let mut o = Options::new(name);
        while let Some(a) = args.first().cloned() {
            let mut value = || {
                args.remove(0);
                if args.is_empty() { Err(format!("{a} needs a value")) } else { Ok(args.remove(0)) }
            };
            match a.as_str() {
                "--data-dir" => o.data_dir = Some(value()?.into()),
                "--no-data" => {
                    args.remove(0);
                    o.data_dir = None;
                }
                "--snapshot" => o.snapshot = Some(value()?.into()),
                _ => break,
            }
        }
        Ok(o)
    }
}

/// A handle on the instance, which lives on its own thread inside tokio's
/// single-threaded runtime, where its background work runs between calls.
/// Each call is a job queued to that thread, run as its own task, so calls
/// overlap while they wait.
pub struct Native<T: ToolInstance> {
    jobs: tokio::sync::mpsc::UnboundedSender<Job<T>>,
    on_change: OnChange,
    /// The last settings configure accepted, replayed into a fresh instance.
    settings: Arc<Mutex<Option<Map<String, Value>>>>,
}

/// The call failed because the instance panicked; a fresh one replaced it.
#[derive(Debug)]
pub struct Crashed;

/// Starts the owner thread, making the instance there with `make`. If a
/// call panics, the instance is dropped (its state lost, its socket
/// closed) and a fresh one made and configured as before: what a host
/// does when a component traps.
pub fn start<T: ToolInstance>(opts: &Options, make: impl Fn(Services) -> Arc<T> + Send + 'static) -> Native<T> {
    let (jobs, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job<T>>();
    let on_change: OnChange = Arc::default();
    let settings: Arc<Mutex<Option<Map<String, Value>>>> = Arc::default();
    let (listen_jobs, notify, saved, data_dir) = (jobs.clone(), on_change.clone(), settings.clone(), opts.data_dir.clone());
    let progress = ports::RoutedProgress::default();
    let services = move || Services {
        timer: Box::new(ports::TokioTimer),
        spawn: Box::new(ports::LocalSpawn),
        notify: Box::new(ports::Subscribers(notify.clone())),
        listener: Box::new(NativeListener { jobs: listen_jobs.clone(), running: Mutex::default() }),
        progress: progress.clone(),
        data_dir: data_dir.clone(),
    };
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let crashed = Arc::new(tokio::sync::Notify::new());
            let mut core = make(services());
            loop {
                // A crash first: calls after it go to the fresh instance.
                tokio::select! {
                    biased;
                    () = crashed.notified() => {
                        eprintln!("tool-native: the instance panicked; starting a fresh one (its state is lost)");
                        drop(core);
                        core = make(services());
                        if let Some(s) = saved.lock().unwrap().clone() {
                            let _ = core.configure(s);
                        }
                    }
                    job = rx.recv() => {
                        let Some(job) = job else { break };
                        let (crashed, core) = (crashed.clone(), core.clone());
                        tokio::task::spawn_local(async move {
                            if std::panic::AssertUnwindSafe(job(core)).catch_unwind().await.is_err() {
                                crashed.notify_one();
                            }
                        });
                    }
                }
            }
        });
    });
    Native { jobs, on_change, settings }
}

impl<T: ToolInstance> Native<T> {
    /// Runs `f` on the instance's thread and waits for its result; Err if
    /// the instance panicked meanwhile.
    pub fn call<R: Send + 'static>(&self, f: impl FnOnce(Arc<T>) -> LocalFuture<R> + Send + 'static) -> Result<R, Crashed> {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = self.jobs.send(Box::new(move |core| {
            Box::pin(async move {
                let _ = tx.send(f(core).await);
            })
        }));
        rx.recv().map_err(|_| Crashed)
    }

    /// Resumes from a snapshot (a fresh instance, before other calls).
    pub fn resume(&self, snapshot: Vec<u8>) -> Result<(), String> {
        self.call(move |c| Box::pin(async move { c.resume(&snapshot, None) })).map_err(|_| "the instance panicked".to_string())?
    }

    /// Asks the instance to suspend; Some(what it keeps) if it agreed.
    pub fn suspend(&self, reason: SuspendReason) -> Result<Option<Vec<u8>>, String> {
        self.call(move |c| Box::pin(async move { c.suspend(reason) })).map_err(|_| "the instance panicked".to_string())?
    }
}

fn crashed() -> ToolError {
    ToolError::new(tool_contract::codes::INTERNAL, "the tool failed (it panicked) and was restarted, losing its state; try again")
}

impl<T: ToolInstance> Backend for Native<T> {
    fn describe(&mut self) -> Description {
        self.call(|c| Box::pin(async move { c.describe() })).expect("describe() panicked")
    }
    fn configure(&mut self, config: &Map<String, Value>) -> Result<(), ToolError> {
        let config = config.clone();
        let saved = self.settings.clone();
        self.call(move |c| {
            Box::pin(async move {
                let r = c.configure(config.clone());
                if r.is_ok() {
                    *saved.lock().unwrap() = Some(config);
                }
                r
            })
        })
        .unwrap_or_else(|_| Err(crashed()))
    }
    fn invoke(&mut self, cx: &Context, command: &str, input: Value) -> Result<Value, ToolError> {
        let (cx, command) = (cx.clone(), command.to_string());
        self.call(move |c| c.invoke(&cx, &command, input)).unwrap_or_else(|_| Err(crashed()))
    }
    fn shutdown(&mut self) {
        let _ = self.call(|c| Box::pin(async move { c.shutdown() }));
    }
    fn state(&mut self, since: Option<u64>) -> Option<Value> {
        self.call(move |c| Box::pin(async move { c.state(since) })).ok().flatten()
    }
    fn subscribe(&mut self, on_change: Arc<dyn Fn(u64) + Send + Sync>) {
        *self.on_change.lock().unwrap() = Some(on_change);
    }
    fn clone_handle(&self) -> Option<Box<dyn Backend + Send>> {
        Some(Box::new(Native { jobs: self.jobs.clone(), on_change: self.on_change.clone(), settings: self.settings.clone() }))
    }
}

/// The HTTP interface natively: tool-rest on the instance's thread, the
/// body streamed back to the listener in chunks, stopping when the client
/// goes.
struct NativeHttp<T: ToolInstance> {
    jobs: tokio::sync::mpsc::UnboundedSender<Job<T>>,
}

impl<T: ToolInstance> HttpForward for NativeHttp<T> {
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

/// Serving natively: tool-serve's listener, forwarding to the instance's
/// HTTP interface. It belongs to the instance's ports, so a fresh instance
/// (after a panic) isn't serving, as with a component.
struct NativeListener<T: ToolInstance> {
    jobs: tokio::sync::mpsc::UnboundedSender<Job<T>>,
    running: Mutex<Option<tool_serve::Listener>>,
}

impl<T: ToolInstance> Listener for NativeListener<T> {
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

type Customs = Box<dyn FnOnce(&Description) -> Vec<tool_cli::Custom<'static>>>;

/// What a tool adds to the standard CLI: the same shape as tool-host's
/// Extension, so a tool's custom commands serve both builds.
pub struct App {
    pub name: &'static str,
    /// Instructions for MCP clients.
    pub instructions: Option<String>,
    /// Hand-written commands (e.g. an interactive one), given the
    /// description, added to the generated ones or replacing one of the
    /// same name.
    pub commands: Customs,
}

/// Runs the native build: options before the command (`--data-dir DIR`,
/// `--no-data`, `--snapshot FILE`), then the CLI: the commands generated
/// from describe(), plus `describe`, `mcp`, `serve`, and the tool's own.
/// With --snapshot, it resumes from the file and suspends into it on exit,
/// as tool-host does. Exits with the CLI's status.
pub fn run<T: ToolInstance>(app: App, make: impl Fn(Services) -> Arc<T> + Send + 'static) -> ! {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match Options::parse(app.name, &mut args) {
        Ok(o) => o,
        Err(e) => {
            println!("{}", json!({"error": {"code": "invalid_argument", "message": e}}));
            std::process::exit(2);
        }
    };
    let progress = Arc::new(Mutex::new(None::<ports::RoutedProgress>));
    let p = progress.clone();
    let mut backend = start(&opts, move |services| {
        *p.lock().unwrap() = Some(services.progress.clone());
        make(services)
    });
    let desc = backend.describe();
    if let Some(f) = &opts.snapshot {
        if let Ok(bytes) = std::fs::read(f) {
            if let Err(e) = backend.resume(bytes) {
                eprintln!("{}: couldn't resume from {} ({e}); starting empty", app.name, f.display());
            }
        }
    }
    let routed = progress.lock().unwrap().clone().expect("the instance was made");
    let mut customs = vec![
        tool_cli::describe_command(),
        tool_mcp::mcp_command(app.name, app.instructions, Box::new(move |out| routed.route_mcp(out))),
        tool_serve::command(app.name),
    ];
    customs.extend((app.commands)(&desc));
    let code = tool_cli::run(&desc, args, &mut backend, customs);
    if let Some(f) = &opts.snapshot {
        match backend.suspend(SuspendReason::Exit) {
            Ok(Some(bytes)) => {
                if let Err(e) = std::fs::write(f, bytes) {
                    eprintln!("{}: couldn't save the snapshot to {}: {e}", app.name, f.display());
                }
            }
            Ok(None) => {
                let _ = std::fs::remove_file(f);
            }
            Err(why) => eprintln!("{}: not saving a snapshot: {why}", app.name),
        }
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_come_before_the_command() {
        let mut args: Vec<String> = ["--snapshot", "s.bin", "--data-dir", "/tmp/x", "list", "--data-dir", "y"].map(String::from).to_vec();
        let o = Options::parse("t", &mut args).unwrap();
        assert_eq!(o.snapshot.as_deref(), Some(std::path::Path::new("s.bin")));
        assert_eq!(o.data_dir.as_deref(), Some(std::path::Path::new("/tmp/x")));
        assert_eq!(args, ["list", "--data-dir", "y"]);
        let mut args: Vec<String> = ["--no-data", "x"].map(String::from).to_vec();
        assert_eq!(Options::parse("t", &mut args).unwrap().data_dir, None);
    }
}

#[cfg(test)]
mod crash_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static MADE: AtomicU32 = AtomicU32::new(0);

    struct Fragile {
        configured: Mutex<Option<String>>,
    }

    impl ToolInstance for Fragile {
        fn describe(&self) -> Description {
            serde_json::from_value(json!({"contract": "t", "name": "t", "version": "0", "summary": "", "commands": []})).unwrap()
        }
        fn configure(&self, settings: Map<String, Value>) -> Result<(), ToolError> {
            *self.configured.lock().unwrap() = settings.get("x").and_then(|v| v.as_str()).map(String::from);
            Ok(())
        }
        fn invoke(self: Arc<Self>, _: &Context, command: &str, _: Value) -> LocalFuture<Result<Value, ToolError>> {
            let command = command.to_string();
            Box::pin(async move {
                if command == "boom" {
                    panic!("boom");
                }
                Ok(json!({"configured": *self.configured.lock().unwrap()}))
            })
        }
    }

    #[test]
    fn a_panic_gives_an_error_and_a_fresh_configured_instance() {
        let mut b = start(&Options::new("t"), |_| {
            MADE.fetch_add(1, Ordering::SeqCst);
            Arc::new(Fragile { configured: Mutex::default() })
        });
        b.configure(&json!({"x": "y"}).as_object().unwrap().clone()).unwrap();
        let cx = Context::new("terminal");
        let e = b.invoke(&cx, "boom", json!({})).unwrap_err();
        assert_eq!(e.code, "internal");
        // The fresh instance is made on the owner thread; the next call waits behind it.
        let v = b.invoke(&cx, "ok", json!({})).unwrap();
        assert_eq!(v["configured"], "y", "settings are replayed into the fresh instance");
        assert_eq!(MADE.load(Ordering::SeqCst), 2);
    }
}
