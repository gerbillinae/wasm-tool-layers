//! A tool's HTTP interface, generated from its description: one route per
//! command offered to REST (the command's own route adaptor, or the default
//! `GET`/`POST /api/<name>`), plus the browser adapters (the page and its
//! files, `/api/state`, `/api/events`). See opinionated/optional/wasm.md.
//!
//! It runs where the tool runs: inside the component (which exports it as
//! `wasi:http/handler`) or on the native core's thread. It's async and
//! single-threaded, with no sockets. It checks the token (the tool made it
//! when it asked to be served); the host's listener checks Origin and Host.

use std::pin::Pin;
use std::rc::Rc;

use base64::Engine;
use futures::stream::{self, Stream, StreamExt};
use serde_json::{Map, Value, json};
use tool_contract::{
    Asset, BodyKind, Category, Context, Description, HttpRequest, LocalFuture, Route, ToolError, codes, render_args,
    render_commands,
};

/// What tool-rest needs from the tool, in process.
pub trait Service {
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

pub enum Body {
    Full(Vec<u8>),
    /// Chunks until the stream ends (or the client goes away and the
    /// stream is dropped).
    Stream(Pin<Box<dyn Stream<Item = Vec<u8>>>>),
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

pub fn status_for(c: Category) -> u16 {
    match c {
        Category::InvalidInput => 400,
        Category::Refused => 403,
        Category::NotFound => 404,
        Category::Conflict => 409,
        Category::Unavailable => 502,
        Category::Internal => 500,
    }
}

fn json_response(status: u16, v: &Value) -> Response {
    Response {
        status,
        headers: vec![("Content-Type".into(), "application/json".into()), ("Cache-Control".into(), "no-store".into())],
        body: Body::Full(serde_json::to_vec(v).unwrap_or_default()),
    }
}

fn error_response(desc: &Description, e: &ToolError) -> Response {
    let status = if e.code == "no_route" { 404 } else { status_for(desc.category(&e.code)) };
    let e = ToolError { message: render_commands(&render_args(&e.message, |a| a.to_string()), |c| c.to_string()), ..e.clone() };
    json_response(status, &json!({"error": e}))
}

/// Matches a path against a pattern with {name} segments; a last segment
/// {name*} takes the rest of the path (e.g. /{file*} for vendor/x.js).
pub fn match_path(pattern: &str, path: &str) -> Option<Map<String, Value>> {
    let (p, a): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    let mut params = Map::new();
    for (i, seg) in p.iter().enumerate() {
        if let Some(name) = seg.strip_prefix('{').and_then(|n| n.strip_suffix("*}")) {
            let rest = a.get(i..).map(|r| r.join("/")).unwrap_or_default();
            if rest.is_empty() || i != p.len() - 1 {
                return None;
            }
            params.insert(name.into(), Value::String(percent_decode(&rest)));
            return Some(params);
        }
        let got = a.get(i)?;
        if let Some(name) = seg.strip_prefix('{').and_then(|n| n.strip_suffix('}')) {
            if got.is_empty() {
                return None;
            }
            params.insert(name.into(), Value::String(percent_decode(got)));
        } else if seg != got {
            return None;
        }
    }
    (p.len() == a.len()).then_some(params)
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The route a request matches, and its path parameters.
pub fn find_route<'a>(desc: &'a Description, method: &str, path: &str) -> Option<(&'a Route, Map<String, Value>)> {
    let routes = &desc.http.as_ref()?.routes;
    routes.iter().find_map(|r| {
        if r.method != method {
            return None;
        }
        if let Some(c) = &r.command {
            if desc.command(c).is_none_or(|c| !desc.available(c)) {
                return None;
            }
        }
        match_path(&r.path, path).map(|p| (r, p))
    })
}

/// Query parameters as a command's input, typed by its schema: repeated
/// keys for arrays, true/false for booleans, numbers parsed.
fn decode_query(desc: &Description, command: &str, query: &[(String, String)]) -> Result<Map<String, Value>, ToolError> {
    let props = desc.command(command).and_then(|c| c.input.get("properties")).and_then(|p| p.as_object());
    let mut out = Map::new();
    for (k, v) in query {
        if k == "token" {
            continue;
        }
        let Some(schema) = props.and_then(|p| p.get(k)) else {
            return Err(ToolError::new(codes::INVALID_ARGUMENT, format!("{{cmd:{command}}} doesn't take {k:?}")));
        };
        let ty = match schema.get("type") {
            Some(Value::String(t)) => t.clone(),
            Some(Value::Array(ts)) => ts.iter().filter_map(|t| t.as_str()).find(|t| *t != "null").unwrap_or("string").to_string(),
            _ => "string".into(),
        };
        let bad = || ToolError::new(codes::INVALID_ARGUMENT, format!("{k}: {v:?} isn't a {ty}"));
        let value = match ty.as_str() {
            "boolean" => Value::Bool(matches!(v.as_str(), "true" | "1" | "")),
            "integer" => json!(v.parse::<i64>().map_err(|_| bad())?),
            "number" => json!(v.parse::<f64>().map_err(|_| bad())?),
            "array" => {
                if let Value::Array(a) = out.entry(k.clone()).or_insert_with(|| Value::Array(vec![])) {
                    a.push(Value::String(v.clone()));
                }
                continue;
            }
            _ => Value::String(v.clone()),
        };
        out.insert(k.clone(), value);
    }
    Ok(out)
}

/// Handles one (already checked) request.
pub async fn handle(svc: Rc<dyn Service>, desc: &Description, req: HttpRequest) -> Response {
    let Some((route, params)) = find_route(desc, &req.method, &req.path) else {
        return error_response(desc, &ToolError::new("no_route", format!("no route {} {}", req.method, req.path)));
    };
    if !route.public {
        // As a bearer token, or ?token= for event streams (EventSource can't
        // send headers).
        let given = req
            .header("Authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| req.query.iter().find(|(k, _)| k == "token").map(|(_, v)| v.clone()))
            .unwrap_or_default();
        if given.is_empty() || !svc.authorized(&given) {
            return json_response(
                401,
                &json!({"error": ToolError::new("unauthorized", "missing or wrong token: use the token from the address the tool gave when it started serving")}),
            );
        }
    }
    match route.body {
        BodyKind::File => {
            let path = params.get("file").and_then(|f| f.as_str()).unwrap_or("");
            match svc.asset(path) {
                Some(a) => Response { status: 200, headers: vec![("Content-Type".into(), a.content_type)], body: Body::Full(a.data) },
                None => error_response(desc, &ToolError::new("no_route", format!("no file {}", req.path))),
            }
        }
        BodyKind::EventStream => events(svc),
        BodyKind::Json | BodyKind::Bytes if route.command.is_none() => {
            // /api/state
            let since = req.query.iter().find(|(k, _)| k == "since_revision").and_then(|(_, v)| v.parse().ok());
            match svc.state(since) {
                Some(s) => json_response(200, &s),
                None => error_response(desc, &ToolError::new("no_route", "this tool has no shared state")),
            }
        }
        BodyKind::Json | BodyKind::Bytes => command(svc, desc, route, params, req).await,
    }
}

/// A route adapting a command: input from the path, query or body; the
/// result as JSON (with the revision) or, for a bytes route, raw.
async fn command(svc: Rc<dyn Service>, desc: &Description, route: &Route, params: Map<String, Value>, req: HttpRequest) -> Response {
    let name = route.command.as_deref().unwrap_or_default();
    let mut input = if req.method == "GET" || req.method == "DELETE" {
        match decode_query(desc, name, &req.query) {
            Ok(m) => m,
            Err(e) => return error_response(desc, &e),
        }
    } else if req.body.iter().all(|b| b.is_ascii_whitespace()) {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(&req.body) {
            Ok(Value::Object(m)) => m,
            _ => {
                return error_response(
                    desc,
                    &ToolError::new(codes::INVALID_ARGUMENT, format!("the request body for {} isn't a JSON object", req.path)),
                );
            }
        }
    };
    input.extend(params);
    let result = svc.invoke(&Context::new("web"), name, Value::Object(input)).await;
    match (result, &route.bytes) {
        (Err(e), _) => error_response(desc, &e),
        (Ok(v), Some(f)) => {
            // The command's result carries the bytes as base64 (it crossed
            // invoke as JSON); the adaptor sends them raw.
            let data = v.get(&f.data).and_then(|b| b.as_str()).and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
            let ct = v.get(&f.content_type).and_then(|t| t.as_str()).unwrap_or("application/octet-stream").to_string();
            match data {
                Some(data) => Response { status: 200, headers: vec![("Content-Type".into(), ct)], body: Body::Full(data) },
                None => error_response(desc, &ToolError::new(codes::INTERNAL, format!("{name}'s result has no bytes in {:?}", f.data))),
            }
        }
        (Ok(mut v), None) => {
            if let (Some(o), Some(rev)) = (v.as_object_mut(), svc.state(None).and_then(|s| s.get("revision").cloned())) {
                o.entry("revision").or_insert(rev);
            }
            json_response(200, &v)
        }
    }
}

/// Handles a request and streams the response into channels: the head
/// first, then the body's chunks. It stops as soon as `gone` resolves (the
/// client disconnected) or the receiver is dropped, which drops an event
/// stream and tells the tool the page has gone. Used by hosts that run
/// tool-rest on the tool's thread (natively).
pub async fn serve_into(
    svc: Rc<dyn Service>,
    desc: &Description,
    req: HttpRequest,
    head: futures::channel::oneshot::Sender<tool_contract::HttpHead>,
    mut chunks: futures::channel::mpsc::Sender<Vec<u8>>,
    gone: futures::channel::oneshot::Receiver<()>,
) {
    use futures::SinkExt;
    let resp = handle(svc, desc, req).await;
    if head.send(tool_contract::HttpHead { status: resp.status, headers: resp.headers }).is_err() {
        return;
    }
    let mut body: Pin<Box<dyn Stream<Item = Vec<u8>>>> = match resp.body {
        Body::Full(b) => Box::pin(stream::iter([b])),
        Body::Stream(s) => s,
    };
    let mut gone = gone;
    loop {
        let next = futures::future::select(body.next(), &mut gone).await;
        match next {
            futures::future::Either::Left((Some(chunk), _)) => {
                if chunks.send(chunk).await.is_err() {
                    break;
                }
            }
            _ => break, // finished, or the client has gone
        }
    }
}

/// How often an idle event stream writes a comment.
const PING_MS: u64 = 5_000;

/// Tells the tool when the page goes away (the stream is dropped when the
/// client disconnects).
struct PageGuard(Rc<dyn Service>);

impl Drop for PageGuard {
    fn drop(&mut self) {
        self.0.page(false);
    }
}

/// Server-sent events: the state now, then at every change with the changes
/// since the previous message. A comment every 5 s keeps writing, because a
/// closed connection is only noticed when a write fails (and the first write
/// after a close usually succeeds), so the page's disconnect reaches the
/// tool within a few of these.
fn events(svc: Rc<dyn Service>) -> Response {
    svc.page(true);
    let guard = Rc::new(PageGuard(svc.clone()));
    let first = svc.state(None).map(|mut s| {
        s["changes"] = json!([]);
        s
    });
    let start = first.as_ref().and_then(|s| s["revision"].as_u64()).unwrap_or(0);
    let head = stream::iter(first.map(|s| format!("data: {s}\n\n").into_bytes()));
    let rest = stream::unfold((svc, start, guard), |(svc, last, guard)| async move {
        let changed = svc.changed(last);
        let ping = svc.sleep(PING_MS);
        match futures::future::select(changed, ping).await {
            futures::future::Either::Left(_) => {
                let s = svc.state(Some(last))?;
                let rev = s["revision"].as_u64().unwrap_or(last);
                Some((format!("data: {s}\n\n").into_bytes(), (svc, rev, guard)))
            }
            futures::future::Either::Right(_) => Some((b": ping\n\n".to_vec(), (svc, last, guard))),
        }
    });
    Response {
        status: 200,
        headers: vec![("Content-Type".into(), "text/event-stream".into()), ("Cache-Control".into(), "no-store".into())],
        body: Body::Stream(Box::pin(head.chain(rest))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(match_path("/api/media/{file}", "/api/media/a%20b.mp3").unwrap()["file"], "a b.mp3");
        assert!(match_path("/api/cards", "/api/cards/parse").is_none());
        assert!(match_path("/{file*}", "/").is_none());
        assert_eq!(match_path("/{file*}", "/vendor/x.js").unwrap()["file"], "vendor/x.js");
        assert!(match_path("/api/media/{file}", "/api/media/a/b").is_none());
        assert_eq!(percent_decode("x%2Fy+z"), "x/y z");
    }
}
