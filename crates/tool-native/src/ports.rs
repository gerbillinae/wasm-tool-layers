//! Native implementations of the generic ports (tool-ports).

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tool_contract::{Context, LocalFuture as Future, ToolError};
use tool_ports::*;

#[derive(Clone)]
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl UreqHttp {
    pub fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        UreqHttp { agent }
    }

    fn blocking(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        let timeout = Some(Duration::from_millis(req.timeout_ms));
        let mut resp = if req.method == "GET" {
            let mut b = self.agent.get(&req.url).config().timeout_global(timeout).build();
            for (k, v) in &req.headers {
                b = b.header(k, v);
            }
            b.call().map_err(|e| e.to_string())?
        } else {
            let mut b = self.agent.post(&req.url).config().timeout_global(timeout).build();
            for (k, v) in &req.headers {
                b = b.header(k, v);
            }
            b.send(&req.body[..]).map_err(|e| e.to_string())?
        };
        let status = resp.status().as_u16();
        let body = resp.body_mut().with_config().limit(256 * 1024 * 1024).read_to_vec().map_err(|e| e.to_string())?;
        Ok(HttpResponse { status, body })
    }
}

impl HttpOut for UreqHttp {
    fn send(&self, req: HttpRequest) -> Future<Result<HttpResponse, String>> {
        let me = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || me.blocking(&req)).await.unwrap_or_else(|e| Err(e.to_string()))
        })
    }
}

/// The system clock, with the local time zone's offset.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn utc_offset_secs(&self, at_ms: i64) -> i32 {
        use chrono::{Local, Offset, TimeZone};
        Local.timestamp_millis_opt(at_ms).single().map(|t| t.offset().fix().local_minus_utc()).unwrap_or(0)
    }
}

/// Secrets from the environment, or else a keys file of NAME=value lines
/// (by default ~/keys.env).
pub struct EnvSecrets {
    pub keys_file: Option<std::path::PathBuf>,
}

impl Default for EnvSecrets {
    fn default() -> Self {
        EnvSecrets { keys_file: std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join("keys.env")) }
    }
}

impl EnvSecrets {
    fn keys_file(&self) -> Option<std::path::PathBuf> {
        self.keys_file.clone()
    }
}

impl Secrets for EnvSecrets {
    fn get(&self, name: &str) -> Option<Secret> {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return Some(Secret { value: v.trim().into(), source: "env".into() });
            }
        }
        let path = self.keys_file()?;
        let text = std::fs::read_to_string(&path).ok()?;
        for line in text.lines() {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line).trim();
            let Some((k, v)) = line.split_once('=') else { continue };
            if k.trim() != name {
                continue;
            }
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(Secret { value: v.into(), source: path.display().to_string() });
            }
        }
        None
    }

    fn hint(&self, name: &str) -> String {
        let file = self.keys_file().map(|p| p.display().to_string()).unwrap_or("~/keys.env".into());
        format!("set {name}, or put `{name}=<key>` in {file}")
    }
}

/// Questions on the terminal: the question on stderr, the answer from stdin.
pub struct TerminalAsker;

impl Asker for TerminalAsker {
    fn ask(&self, question: &str) -> Result<String, ToolError> {
        eprint!("{question}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(n) if n > 0 => Ok(line.trim().to_string()),
            _ => Err(ToolError::new("not_confirmed", "no answer was given. Nothing was changed.")),
        }
    }
    fn confirm(&self, question: &str) -> Result<bool, ToolError> {
        let a = self.ask(&format!("{question} [y/N] "))?.to_lowercase();
        Ok(a == "y" || a == "yes")
    }
}

type Sink = Box<dyn Fn(&Context, &str) -> bool + Send + Sync>;

/// Progress on stderr, unless a mode installs a sink that takes it (the
/// mcp mode sends progress for calls with a progress token as MCP
/// notifications). Which it is depends on the mode, which is chosen after
/// the ports are made, hence the indirection.
#[derive(Clone, Default)]
pub struct RoutedProgress {
    sink: Arc<Mutex<Option<Sink>>>,
}

impl RoutedProgress {
    pub fn install(&self, sink: Sink) {
        *self.sink.lock().unwrap() = Some(sink);
    }
}

impl Progress for RoutedProgress {
    fn log(&self, cx: &Context, message: &str) {
        if let Some(sink) = self.sink.lock().unwrap().as_ref() {
            if sink(cx, message) {
                return;
            }
        }
        eprintln!("{message}");
    }
}

impl RoutedProgress {
    /// In mcp mode: progress for a call with a progress token becomes an MCP
    /// notification on `out`; anything else still goes to stderr.
    pub fn route_mcp(&self, out: Arc<Mutex<std::io::Stdout>>) {
        let counter = AtomicU64::new(0);
        self.install(Box::new(move |cx, message| {
            let Some(token) = &cx.request else { return false };
            let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
            let mut o = out.lock().unwrap();
            let _ = writeln!(o, "{}", tool_mcp::progress_notification(token, n, message));
            let _ = o.flush();
            true
        }));
    }
}

/// Time, through tokio, on the core's thread.
pub struct TokioTimer;

impl Timer for TokioTimer {
    fn sleep(&self, ms: u64) -> tool_ports::LocalFuture {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

/// Background work, on the core's thread (tokio's LocalSet).
pub struct LocalSpawn;

impl Spawn for LocalSpawn {
    fn spawn(&self, work: LocalFuture) {
        tokio::task::spawn_local(work);
    }
}

/// Tells whoever subscribed (Backend::subscribe) about new revisions.
pub struct Subscribers(pub Arc<Mutex<Option<Arc<dyn Fn(u64) + Send + Sync>>>>);

impl Notify for Subscribers {
    fn changed(&self, revision: u64) {
        if let Some(f) = self.0.lock().unwrap().as_ref() {
            f(revision);
        }
    }
}

/// Where a native tool keeps its data unless told otherwise; see
/// [`tool_contract::standard_data_dir`].
pub fn default_data_dir(name: &str) -> Option<std::path::PathBuf> {
    tool_contract::standard_data_dir(name)
}
