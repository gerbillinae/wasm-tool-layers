//! Guest-side WASI 0.3 glue for tools (wasm32-wasip2 only):
//!
//! - [`fetch`]: outgoing HTTP through `wasi:http`'s async client, for a
//!   tool's HttpOut port;
//! - [`serve`]: a component's `wasi:http/handler` export, running tool-rest:
//!   converts the request, and streams tool-rest's response back (an event
//!   stream for as long as the client stays).
//!
//! - [`export_tool!`]: a ToolInstance as the component, with its ports.
//!
//! All generic; a component's own glue is then a few lines.

use std::rc::Rc;

pub mod export;

/// For export_tool!'s expansion, so components needn't name these crates.
#[doc(hidden)]
pub mod reexports {
    pub use serde;
    pub use serde_json;
    pub use tool_contract;
    pub use tool_ports;
    pub use wasip3;
}

use tool_contract::{Description, HTTP_DENIED};

/// An outgoing request.
pub struct Fetch {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout_ms: u64,
}

pub struct Fetched {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends a request through wasi:http (async). The error is a transport
/// failure; one the host refused starts with [`HTTP_DENIED`].
pub async fn fetch(req: Fetch) -> Result<Fetched, String> {
    use wasip3::http::types::{ErrorCode, Fields, Method, Request, RequestOptions, Response, Scheme};
    use wasip3::wit_bindgen::StreamResult;

    let (scheme, rest) = req.url.split_once("://").ok_or("not an absolute URL")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut entries: Vec<(String, Vec<u8>)> = req.headers.iter().map(|(k, v)| (k.to_lowercase(), v.as_bytes().to_vec())).collect();
    // Without it the body is sent chunked, which simple servers (AnkiConnect's
    // among them) don't read.
    entries.push(("content-length".into(), req.body.len().to_string().into_bytes()));
    let headers = Fields::from_list(&entries).map_err(|e| format!("headers: {e:?}"))?;

    let (mut body_tx, body_rx) = wasip3::wit_stream::new::<u8>();
    let (trailers_tx, trailers_rx) = wasip3::wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));
    let options = RequestOptions::new();
    let ns = req.timeout_ms * 1_000_000;
    let _ = options.set_connect_timeout(Some(ns));
    let _ = options.set_first_byte_timeout(Some(ns));
    let _ = options.set_between_bytes_timeout(Some(ns));
    let (request, _sent) = Request::new(headers, Some(body_rx), trailers_rx, Some(options));
    request
        .set_method(&match req.method.as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            m => Method::Other(m.into()),
        })
        .map_err(|_| "bad method")?;
    request
        .set_scheme(Some(&match scheme {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            s => Scheme::Other(s.into()),
        }))
        .map_err(|_| "bad scheme")?;
    request.set_authority(Some(authority)).map_err(|_| "bad authority")?;
    request.set_path_with_query(Some(path)).map_err(|_| "bad path")?;

    let body = req.body;
    let write = async move {
        body_tx.write_all(body).await;
        drop(body_tx);
        let _ = trailers_tx.write(Ok(None)).await;
    };
    let (_, response) = futures::join!(write, wasip3::http::client::send(request));
    let response = response.map_err(describe_http_error)?;
    let status = response.get_status_code();
    let (done_tx, done_rx) = wasip3::wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
    let (mut stream, _trailers) = Response::consume_body(response, done_rx);
    let mut data = vec![];
    loop {
        let (result, chunk) = stream.read(Vec::with_capacity(64 * 1024)).await;
        data.extend(chunk);
        match result {
            StreamResult::Complete(_) => {}
            StreamResult::Dropped | StreamResult::Cancelled => break,
        }
    }
    drop(done_tx);
    Ok(Fetched { status, body: data })
}

/// Says plainly when the host refused the request, so a tool can tell the
/// user to grant access rather than to start the server.
fn describe_http_error(e: wasip3::http::types::ErrorCode) -> String {
    match e {
        wasip3::http::types::ErrorCode::HttpRequestDenied => format!("{HTTP_DENIED}: the host didn't grant access to this address"),
        e => format!("{e:?}"),
    }
}

/// Handles a request for a component's wasi:http/handler export with
/// tool-rest. Use it as:
///
/// ```ignore
/// impl wasip3::exports::wasi::http::handler::Guest for HttpExport {
///     async fn handle(r: Request) -> Result<Response, ErrorCode> {
///         tool_wasi::serve(service(), &DESCRIPTION, r).await
///     }
/// }
/// wasip3::http::service::export!(HttpExport);
/// ```
pub async fn serve(
    svc: Rc<dyn tool_rest::Service>,
    desc: &Description,
    request: wasip3::http::types::Request,
) -> Result<wasip3::http::types::Response, wasip3::http::types::ErrorCode> {

    use futures::StreamExt;
    use wasip3::http::types::{ErrorCode, Fields, Method, Request, Response};

    let method = match request.get_method() {
        Method::Get => "GET".to_string(),
        Method::Head => "HEAD".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Patch => "PATCH".into(),
        Method::Options => "OPTIONS".into(),
        Method::Connect => "CONNECT".into(),
        Method::Trace => "TRACE".into(),
        Method::Other(m) => m,
    };
    let target = request.get_path_with_query().unwrap_or_else(|| "/".into());
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let headers = request
        .get_headers()
        .copy_all()
        .into_iter()
        .map(|(k, v)| (k, String::from_utf8_lossy(&v).into_owned()))
        .collect();
    let (done_tx, done_rx) = wasip3::wit_future::new::<Result<(), ErrorCode>>(|| Ok(()));
    let (mut stream, _trailers) = Request::consume_body(request, done_rx);
    let mut body = vec![];
    loop {
        let (result, chunk) = stream.read(Vec::with_capacity(16 * 1024)).await;
        body.extend(chunk);
        if !matches!(result, wasip3::wit_bindgen::StreamResult::Complete(_)) {
            break;
        }
    }
    drop(done_tx);
    let req = tool_contract::HttpRequest {
        method,
        path: path.to_string(),
        query: query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (tool_rest::percent_decode(k), tool_rest::percent_decode(v))
            })
            .collect(),
        headers,
        body,
    };

    let resp = tool_rest::handle(svc, desc, req).await;

    let fields: Vec<(String, Vec<u8>)> = resp.headers.iter().map(|(k, v)| (k.to_lowercase(), v.as_bytes().to_vec())).collect();
    let fields = Fields::from_list(&fields).map_err(|e| ErrorCode::InternalError(Some(format!("{e:?}"))))?;
    let (mut body_tx, body_rx) = wasip3::wit_stream::new::<u8>();
    let (trailers_tx, trailers_rx) = wasip3::wit_future::new::<Result<Option<Fields>, ErrorCode>>(|| Ok(None));
    let (response, _sent) = Response::new(fields, Some(body_rx), trailers_rx);
    let _ = response.set_status_code(resp.status);
    // The body is written after this returns: an event stream for as
    // long as the client stays.
    wit_bindgen::spawn_local(async move {
        match resp.body {
            tool_rest::Body::Full(b) => {
                body_tx.write_all(b).await;
            }
            tool_rest::Body::Stream(mut s) => {
                while let Some(chunk) = s.next().await {
                    if !body_tx.write_all(chunk).await.is_empty() {
                        break; // the client has gone
                    }
                }
            }
        }
        drop(body_tx);
        let _ = trailers_tx.write(Ok(None)).await;
    });
    Ok(response)
}
