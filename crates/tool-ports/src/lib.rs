//! Ports: the only ways a tool's core reaches the outside world (see
//! opinionated/optional/wasm.md, Rule 1). The core depends on these traits;
//! `tool-native` implements them natively, and a component implements them
//! with WASI and host imports (`tool-wasi` has the generic parts).
//!
//! Anything that waits (HTTP, timers, clips) is async, and background work
//! is spawned, so the core never blocks its one thread; the rest (clock,
//! secrets, questions, progress) is synchronous.

use tool_contract::{Context, ToolError};

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// How an HttpOut error starts when the host refused the request (a WASM
/// host without the grant), as opposed to the server not answering.
pub const HTTP_DENIED: &str = tool_contract::HTTP_DENIED;

/// Outgoing HTTP: AnkiConnect and Google text-to-speech. The error is a
/// transport failure (couldn't connect, timed out, refused by the host),
/// not an HTTP status. It's async, so a slow server doesn't hold up the
/// tool's other work: natively on a blocking pool, in a component through
/// WASI 0.3's wasi:http.
pub trait HttpOut: Send + Sync {
    fn send(&self, req: HttpRequest) -> tool_contract::LocalFuture<Result<HttpResponse, String>>;
}

/// The time, and the local time zone's offset from UTC at a moment (Anki's
/// day starts at 4am local time).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn utc_offset_secs(&self, at_ms: i64) -> i32;
}

/// A secret and where it came from ("env", or a file's path), for status.
#[derive(Debug, Clone)]
pub struct Secret {
    pub value: String,
    pub source: String,
}

pub trait Secrets: Send + Sync {
    fn get(&self, name: &str) -> Option<Secret>;
    /// How to provide a missing secret on this host, e.g. "set GOOGLE_API_KEY,
    /// or put GOOGLE_API_KEY=<key> in ~/keys.env" natively, or "grant it with
    /// --secret GOOGLE_API_KEY" under a WASM host. Only the host knows.
    fn hint(&self, name: &str) -> String;
}

/// Asks the person a question. Only used when the call's context is
/// interactive.
pub trait Asker: Send + Sync {
    fn confirm(&self, question: &str) -> Result<bool, ToolError>;
    fn ask(&self, question: &str) -> Result<String, ToolError>;
}

/// Progress for a person ("Making audio for …"). It's per call, because
/// one instance serves several callers, each with their own place to
/// report to (stderr, an MCP progress notification).
pub trait Progress: Send + Sync {
    fn log(&self, cx: &Context, message: &str);
}

/// A future the core awaits. Not Send: the core runs on one thread (the
/// owner thread natively, the only thread in a component).
pub type LocalFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;

/// Plays sound on this machine's speakers. Optional: a host without audio
/// gives none (or one whose `available` is false), and the play commands
/// are left out.
pub trait Player: Send + Sync {
    fn available(&self) -> bool;
    /// Starts playing.
    fn play(&self, data: Vec<u8>, mime: &str) -> Result<Box<dyn Clip>, String>;
}

/// A clip that's playing.
pub trait Clip: Send {
    /// Resolves when the clip has finished, or been stopped.
    fn finished(&self) -> LocalFuture;
    fn stop(&self);
}

/// Time passing, for work that waits (playback's gaps).
pub trait Timer: Send + Sync {
    fn sleep(&self, ms: u64) -> LocalFuture;
}

/// Runs work in the background, after the call that started it has
/// returned: tokio's spawn_local on the owner thread natively, wit-bindgen's
/// spawn_local in a component (WASI 0.3).
pub trait Spawn: Send + Sync {
    fn spawn(&self, work: LocalFuture);
}

/// Told the shared state's revision whenever it changes, so a host can wake
/// whoever is watching (event streams). Background work changes state
/// between calls, so the host can't find out by itself.
pub trait Notify: Send + Sync {
    fn changed(&self, revision: u64);
}

/// Serves this tool's own HTTP interface. The tool asks for it (web-start)
/// and the host listens: localhost only, with Origin and Host checks,
/// forwarding to the tool's handler. The tool makes and checks the token.
pub trait Listener: Send + Sync {
    /// Starts listening on 127.0.0.1 (port 0: any free one); returns the
    /// address.
    fn listen(&self, port: u16, allow_origins: Vec<String>) -> tool_contract::LocalFuture<Result<String, String>>;
    fn unlisten(&self);
}

pub struct Ports {
    pub http: Box<dyn HttpOut>,
    pub clock: Box<dyn Clock>,
    pub secrets: Box<dyn Secrets>,
    pub asker: Option<Box<dyn Asker>>,
    pub progress: Box<dyn Progress>,
    pub player: Option<Box<dyn Player>>,
    pub timer: Box<dyn Timer>,
    pub spawn: Box<dyn Spawn>,
    pub notify: Box<dyn Notify>,
    /// None on a host that can't serve HTTP.
    pub listener: Option<Box<dyn Listener>>,
    /// A directory the tool may keep files in, for data that outlives the
    /// process (std::fs works on it natively and in a component, where the
    /// host mounts its grant at /data). None if there's none: features
    /// needing it are unavailable. The tool manages its own locking
    /// (tool-files has lock files and atomic writes).
    pub data_dir: Option<std::path::PathBuf>,
}

/// A Progress that drops everything.
pub struct NoProgress;

impl Progress for NoProgress {
    fn log(&self, _: &Context, _: &str) {}
}
