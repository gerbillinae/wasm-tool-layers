//! The host's HTTP listener, native only, on hyper. A tool asks for it
//! (through its Listener port, or the `listen` host import from a
//! component), so the host owns only the socket and the policy that doesn't
//! need the tool's knowledge: localhost only, and Origin and Host checks
//! (CORS for allowed origins). The tool checks its own token and routes.
//!
//! Requests are forwarded with a signal that fires when the client goes
//! away (hyper notices at once), so event streams end promptly. Bodies
//! stream back.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use bytes::Bytes;
use clap::{Arg, ArgAction, ArgMatches, Command};
use futures::Stream;
use futures::channel::{mpsc, oneshot};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use serde_json::json;
use tool_contract::{Backend, Context, HttpForward, HttpRequest, ToolError};

type Body = BoxBody<Bytes, Infallible>;

struct Policy {
    origin: String,
    allow_origins: Vec<String>,
}

/// A running listener; dropping it (or `stop`) closes it and its
/// connections.
pub struct Listener {
    pub url: String,
    stop: Option<oneshot::Sender<()>>,
}

impl Listener {
    pub fn stop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts listening on 127.0.0.1 (port 0: a free one), forwarding requests
/// that pass the Origin and Host checks to `fwd`.
pub fn listen(fwd: Arc<dyn HttpForward>, port: u16, allow_origins: Vec<String>) -> Result<Listener, String> {
    let std_listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("can't listen on port {port}: {e}"))?;
    std_listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr: SocketAddr = std_listener.local_addr().map_err(|e| e.to_string())?;
    let url = format!("http://127.0.0.1:{}", addr.port());
    let policy = Arc::new(Policy { origin: url.clone(), allow_origins });
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("a tokio runtime");
        rt.block_on(async move {
            let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else { return };
            let accept = async {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let (policy, fwd) = (policy.clone(), fwd.clone());
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| answer(req, policy.clone(), fwd.clone()));
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                            .await;
                    });
                }
            };
            tokio::select! {
                _ = accept => {}
                _ = stop_rx => {}
            }
        });
        // Dropping the runtime closes every open connection.
    });
    Ok(Listener { url, stop: Some(stop_tx) })
}

fn json_body(status: u16, mut headers: Vec<(String, String)>, code: &str, message: String) -> hyper::Response<Body> {
    headers.push(("Content-Type".into(), "application/json".into()));
    let body = serde_json::to_vec(&json!({"error": ToolError::new(code, message)})).unwrap_or_default();
    response(status, headers, Full::new(Bytes::from(body)).boxed())
}

fn response(status: u16, headers: Vec<(String, String)>, body: Body) -> hyper::Response<Body> {
    let mut b = hyper::Response::builder().status(status);
    for (k, v) in headers {
        b = b.header(k, v);
    }
    b.body(body).unwrap_or_else(|_| hyper::Response::new(Full::new(Bytes::new()).boxed()))
}

fn decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The response body: the tool's chunks, holding the "client gone" signal,
/// which fires when hyper drops the body (the client disconnected).
struct Chunks {
    rx: mpsc::Receiver<Vec<u8>>,
    _gone: oneshot::Sender<()>,
}

impl Stream for Chunks {
    type Item = Result<Frame<Bytes>, Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx).map(|c| c.map(|c| Ok(Frame::data(Bytes::from(c)))))
    }
}

/// The host's checks, then forwarding.
async fn answer(req: hyper::Request<Incoming>, p: Arc<Policy>, fwd: Arc<dyn HttpForward>) -> Result<hyper::Response<Body>, Infallible> {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut cors = vec![];
    if let Some(origin) = header("Origin").filter(|o| *o != p.origin) {
        if !p.allow_origins.iter().any(|a| *a == origin) {
            return Ok(json_body(
                403,
                cors,
                "forbidden_origin",
                format!("requests from {origin} aren't allowed: allow the origin when starting the web UI"),
            ));
        }
        cors.push(("Access-Control-Allow-Origin".to_string(), origin.clone()));
        cors.push(("Vary".into(), "Origin".into()));
        if req.method() == hyper::Method::OPTIONS {
            cors.push(("Access-Control-Allow-Methods".into(), "GET, POST, PATCH, DELETE".into()));
            cors.push(("Access-Control-Allow-Headers".into(), "Authorization, Content-Type".into()));
            return Ok(response(204, cors, Full::new(Bytes::new()).boxed()));
        }
    }
    // Against DNS rebinding: the page must have been loaded from us.
    if header("Host").as_deref() != Some(p.origin.trim_start_matches("http://")) {
        return Ok(json_body(403, cors, "forbidden_origin", format!("unexpected host {:?}", header("Host"))));
    }
    let method = req.method().as_str().to_uppercase();
    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|q| !q.is_empty())
        .map(|q| {
            let (k, v) = q.split_once('=').unwrap_or((q, ""));
            (decode(k), decode(v))
        })
        .collect();
    let headers = req.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
    let body = req.into_body().collect().await.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
    let (gone_tx, gone_rx) = oneshot::channel();
    match fwd.forward(HttpRequest { method, path, query, headers, body }, gone_rx).await {
        Some((head, rx)) => {
            let mut headers: Vec<(String, String)> = head
                .headers
                .into_iter()
                .filter(|(k, _)| !k.eq_ignore_ascii_case("content-length") && !k.eq_ignore_ascii_case("transfer-encoding"))
                .collect();
            headers.extend(cors);
            Ok(response(head.status, headers, StreamBody::new(Chunks { rx, _gone: gone_tx }).boxed()))
        }
        None => Ok(json_body(502, cors, "internal", "the tool's HTTP handler didn't answer".into())),
    }
}

/// The `serve` command: asks the tool to start serving (web-start), prints
/// the address, and serves until Ctrl-C.
pub fn command(name: &str) -> tool_cli::Custom<'static> {
    let name = name.to_string();
    tool_cli::Custom {
        command: Command::new("serve")
            .about("Serve the web UI and HTTP interface on localhost, until stopped.")
            .long_about(
                "Serve the tool's web UI and HTTP interface on 127.0.0.1 (only local connections are accepted). When \
                 it's ready it prints one JSON line on stdout, {\"url\", \"controls_url\", \"token\"}. Every API call \
                 needs the token, as Authorization: Bearer TOKEN (or ?token= for the event stream).",
            )
            .arg(Arg::new("port").long("port").value_parser(clap::value_parser!(u16)).help("Port to listen on. Default: a free one."))
            .arg(
                Arg::new("allow-origin")
                    .long("allow-origin")
                    .action(ArgAction::Append)
                    .help("An origin whose pages may use the API, e.g. to mount the web component elsewhere (repeatable)."),
            ),
        run: Box::new(move |b, m| serve(&name, b, m)),
    }
}

fn serve(name: &str, backend: &mut dyn Backend, m: &ArgMatches) -> i32 {
    let mut input = json!({});
    if let Some(p) = m.get_one::<u16>("port") {
        input["port"] = json!(p);
    }
    if let Some(o) = m.get_many::<String>("allow-origin") {
        input["allow_origins"] = json!(o.collect::<Vec<_>>());
    }
    // An interactive terminal session over commands the plain CLI doesn't
    // list, as the play command does.
    let cx = Context::new("tui");
    let started = match backend.invoke(&cx, "web-start", input) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", json!({"error": e.rendered_plain()}));
            eprintln!("error [{}]: {}", e.code, e.rendered_plain().message);
            return 1;
        }
    };
    println!("{}", json!({"url": started["url"], "controls_url": started["controls_url"], "token": started["token"]}));
    let _ = std::io::stdout().flush();
    eprintln!("{name} serving on {}; Ctrl-C stops it", started["url"].as_str().unwrap_or_default());
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = ctrlc::set_handler(move || {
        let _ = tx.send(());
    });
    let _ = rx.recv();
    eprintln!("stopping");
    let _ = backend.invoke(&cx, "web-stop", json!({}));
    backend.shutdown();
    0
}
