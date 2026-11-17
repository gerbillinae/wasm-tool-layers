//! A ToolInstance as a component: [`export_tool!`](crate::export_tool)
//! writes the glue every tool's component needs, from the bindings the
//! component generated for its own world (which includes
//! `opinionated:tool/tool-component`):
//!
//! - the `tool` exports (describe, configure, invoke, state, shutdown);
//! - the `lifecycle` exports (suspend, resume), and asking to be suspended
//!   once memory passes the tool's limit;
//! - the `wasi:http/handler` export, serving the tool's HTTP interface;
//! - the core's ports over the host imports and WASI ([`host_ports`]);
//! - `typed_call`, for a typed-commands export to run a command through
//!   the registry.
//!
//! The ports that don't need the component's bindings are here as types.

use tool_contract::LocalFuture;
use tool_ports::{HttpOut, HttpRequest, HttpResponse, Spawn, Timer};

/// WASI 0.3's async clock.
pub struct WasiTimer;

impl Timer for WasiTimer {
    fn sleep(&self, ms: u64) -> LocalFuture<()> {
        Box::pin(wasip3::clocks::monotonic_clock::wait_for(ms * 1_000_000))
    }
}

/// Work that carries on after the call that started it returns.
pub struct TaskSpawn;

impl Spawn for TaskSpawn {
    fn spawn(&self, work: LocalFuture<()>) {
        wit_bindgen::spawn_local(work);
    }
}

/// Outgoing HTTP over WASI 0.3's wasi:http client: async, so the instance
/// keeps serving calls while a request waits.
pub struct WasiHttp;

impl HttpOut for WasiHttp {
    fn send(&self, req: HttpRequest) -> LocalFuture<Result<HttpResponse, String>> {
        Box::pin(async move {
            let r = crate::fetch(crate::Fetch { method: req.method, url: req.url, headers: req.headers, body: req.body, timeout_ms: req.timeout_ms }).await?;
            Ok(HttpResponse { status: r.status, body: r.body })
        })
    }
}

/// The instance's linear memory, in bytes.
pub fn memory_bytes() -> u64 {
    #[cfg(target_arch = "wasm32")]
    return core::arch::wasm32::memory_size::<0>() as u64 * 65536;
    // The crate is only for components; this keeps it compiling natively.
    #[cfg(not(target_arch = "wasm32"))]
    0
}

/// JSON nulls dropped, so absent WIT options become absent fields and the
/// core's defaults apply.
pub fn strip_nulls(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k, strip_nulls(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        v => v,
    }
}

/// Exports a [`ToolInstance`](tool_contract::ToolInstance) as this
/// component: `export_tool!(Component, MyTool, || MyTool::new(host_ports()))`.
/// Put it after `wit_bindgen::generate!` for a world that includes
/// `opinionated:tool/tool-component` (so `exports::…` and `opinionated::…`
/// exist here). The closure makes the one instance; `host_ports()` gives
/// the ports the host provides (custom ones, such as a player, are None:
/// set them). It defines the type `$component`, exports it, and defines
/// `typed_call` for typed-commands exports.
#[macro_export]
macro_rules! export_tool {
    ($component:ident, $instance:ty, $make:expr) => {
        static __TOOL: ::std::sync::LazyLock<::std::sync::Arc<$instance>> = ::std::sync::LazyLock::new($make);
        static __DESC: ::std::sync::LazyLock<$crate::reexports::tool_contract::Description> =
            ::std::sync::LazyLock::new(|| $crate::reexports::tool_contract::ToolInstance::describe(&**__TOOL));
        /// The memory a fresh instance had: recycling below twice this
        /// wouldn't shed much, and would only thrash.
        static __FRESH: ::std::sync::LazyLock<u64> = ::std::sync::LazyLock::new($crate::export::memory_bytes);

        pub struct $component;

        fn __from_wit(cx: opinionated::tool::types::Context) -> $crate::reexports::tool_contract::Context {
            $crate::reexports::tool_contract::Context {
                by: cx.by,
                session: cx.session,
                interactive: cx.interactive,
                request: cx.request.and_then(|r| $crate::reexports::serde_json::from_str(&r).ok()),
            }
        }

        fn __to_wit_cx(cx: &$crate::reexports::tool_contract::Context) -> opinionated::tool::types::Context {
            opinionated::tool::types::Context {
                by: cx.by.clone(),
                session: cx.session.clone(),
                interactive: cx.interactive,
                request: cx.request.as_ref().map(|r| r.to_string()),
            }
        }

        fn __to_wit(e: $crate::reexports::tool_contract::ToolError) -> opinionated::tool::types::ToolError {
            opinionated::tool::types::ToolError { code: e.code, message: e.message, details: e.details.map(|d| d.to_string()) }
        }

        fn __from_wit_err(e: opinionated::tool::types::ToolError) -> $crate::reexports::tool_contract::ToolError {
            $crate::reexports::tool_contract::ToolError {
                code: e.code,
                message: e.message,
                details: e.details.and_then(|d| $crate::reexports::serde_json::from_str(&d).ok()),
            }
        }

        /// After a command: linear memory never shrinks, so once it's past
        /// the tool's limit (and twice a fresh instance's), ask the host for
        /// a fresh instance, keeping a snapshot, if the tool agrees.
        fn __shed_memory() {
            use $crate::reexports::tool_contract::{SuspendReason, ToolInstance};
            let Some(limit) = __TOOL.memory_limit() else { return };
            ::std::sync::LazyLock::force(&__DESC);
            let bytes = $crate::export::memory_bytes();
            if bytes <= limit || bytes < 2 * *__FRESH {
                return;
            }
            if let Ok(snapshot) = __TOOL.suspend(SuspendReason::Memory) {
                opinionated::tool::host::request_suspend(snapshot.as_deref());
            }
        }

        /// Runs a command through the registry with typed input and output,
        /// for a typed-commands export.
        #[allow(dead_code)]
        async fn typed_call<I: $crate::reexports::serde::Serialize, O: $crate::reexports::serde::de::DeserializeOwned>(
            cx: opinionated::tool::types::Context,
            command: &str,
            input: Option<I>,
        ) -> Result<O, opinionated::tool::types::ToolError> {
            use $crate::reexports::serde_json::{self, Value};
            let input = input
                .map(|i| $crate::export::strip_nulls(serde_json::to_value(i).unwrap_or_default()))
                .unwrap_or(Value::Object(Default::default()));
            let out = $crate::reexports::tool_contract::ToolInstance::invoke(__TOOL.clone(), &__from_wit(cx), command, input).await;
            __shed_memory();
            serde_json::from_value(out.map_err(__to_wit)?).map_err(|e| opinionated::tool::types::ToolError {
                code: "internal".into(),
                message: format!("{command}'s result doesn't match its WIT type: {e}"),
                details: None,
            })
        }

        impl exports::opinionated::tool::tool::Guest for $component {
            async fn describe() -> String {
                // The description first: the fresh size includes it.
                ::std::sync::LazyLock::force(&__DESC);
                ::std::sync::LazyLock::force(&__FRESH);
                // Fresh each time: features and needs can follow config.
                // (__DESC, for routing, keeps the routes, which don't.)
                let desc = $crate::reexports::tool_contract::ToolInstance::describe(&**__TOOL);
                $crate::reexports::serde_json::to_string(&desc).unwrap_or_default()
            }

            async fn configure(settings: String) -> Result<(), opinionated::tool::types::ToolError> {
                let settings = match $crate::reexports::serde_json::from_str(&settings) {
                    Ok($crate::reexports::serde_json::Value::Object(m)) => m,
                    _ => {
                        return Err(opinionated::tool::types::ToolError {
                            code: "invalid_argument".into(),
                            message: "settings aren't a JSON object".into(),
                            details: None,
                        });
                    }
                };
                $crate::reexports::tool_contract::ToolInstance::configure(&**__TOOL, settings).map_err(__to_wit)
            }

            async fn invoke(
                cx: opinionated::tool::types::Context,
                command: String,
                input: String,
            ) -> Result<String, opinionated::tool::types::ToolError> {
                let input = $crate::reexports::serde_json::from_str(&input).map_err(|e| opinionated::tool::types::ToolError {
                    code: "invalid_argument".into(),
                    message: format!("input isn't JSON: {e}"),
                    details: None,
                })?;
                let out = $crate::reexports::tool_contract::ToolInstance::invoke(__TOOL.clone(), &__from_wit(cx), &command, input).await;
                __shed_memory();
                out.map(|v| v.to_string()).map_err(__to_wit)
            }

            async fn shutdown() {
                $crate::reexports::tool_contract::ToolInstance::shutdown(&**__TOOL);
            }

            async fn state(since: Option<u64>) -> Option<String> {
                $crate::reexports::tool_contract::ToolInstance::state(&**__TOOL, since).map(|v| v.to_string())
            }
        }

        impl exports::opinionated::tool::lifecycle::Guest for $component {
            async fn suspend(reason: exports::opinionated::tool::lifecycle::SuspendReason) -> exports::opinionated::tool::lifecycle::SuspendReply {
                use exports::opinionated::tool::lifecycle::{SuspendReason as R, SuspendReply};
                use $crate::reexports::tool_contract::SuspendReason;
                let reason = match reason {
                    R::Idle => SuspendReason::Idle,
                    R::Memory => SuspendReason::Memory,
                    R::Exit => SuspendReason::Exit,
                };
                match $crate::reexports::tool_contract::ToolInstance::suspend(&**__TOOL, reason) {
                    Ok(keep) => SuspendReply::Accept(keep),
                    Err(why) => SuspendReply::Refuse(why),
                }
            }

            async fn resume(snapshot: Vec<u8>, listening: Option<String>) -> Result<(), String> {
                let r = $crate::reexports::tool_contract::ToolInstance::resume(&**__TOOL, &snapshot, listening);
                ::std::sync::LazyLock::force(&__DESC);
                ::std::sync::LazyLock::force(&__FRESH);
                r
            }
        }

        impl exports::wasi::http::handler::Guest for $component {
            async fn handle(
                request: $crate::reexports::wasip3::http::types::Request,
            ) -> Result<$crate::reexports::wasip3::http::types::Response, $crate::reexports::wasip3::http::types::ErrorCode> {
                match $crate::reexports::tool_contract::ToolInstance::http(__TOOL.clone()) {
                    Some(svc) => $crate::serve(svc, &__DESC, request).await,
                    None => Err($crate::reexports::wasip3::http::types::ErrorCode::InternalError(Some("this tool has no HTTP interface".into()))),
                }
            }
        }

        export!($component);

        /// The ports this host provides, through its imports and WASI. Ports
        /// for custom capabilities (a player) are None: set them.
        #[allow(dead_code)]
        fn host_ports() -> $crate::reexports::tool_ports::Ports {
            use $crate::reexports::tool_ports as p;

            struct HostSecrets;
            impl p::Secrets for HostSecrets {
                fn get(&self, name: &str) -> Option<p::Secret> {
                    opinionated::tool::host::get_secret(name).map(|s| p::Secret { value: s.value, source: s.source })
                }
                fn hint(&self, name: &str) -> String {
                    opinionated::tool::host::secret_hint(name)
                }
            }

            struct HostAsker;
            impl p::Asker for HostAsker {
                fn confirm(&self, question: &str) -> Result<bool, $crate::reexports::tool_contract::ToolError> {
                    opinionated::tool::host::confirm(question).map_err(__from_wit_err)
                }
                fn ask(&self, question: &str) -> Result<String, $crate::reexports::tool_contract::ToolError> {
                    opinionated::tool::host::ask(question).map_err(__from_wit_err)
                }
            }

            struct HostProgress;
            impl p::Progress for HostProgress {
                fn log(&self, cx: &$crate::reexports::tool_contract::Context, message: &str) {
                    opinionated::tool::host::progress(&__to_wit_cx(cx), message);
                }
            }

            struct HostNotify;
            impl p::Notify for HostNotify {
                fn changed(&self, revision: u64) {
                    opinionated::tool::host::changed(revision);
                }
            }

            struct HostListener;
            impl p::Listener for HostListener {
                fn listen(&self, port: u16, allow_origins: Vec<String>) -> $crate::reexports::tool_contract::LocalFuture<Result<String, String>> {
                    let r = opinionated::tool::host::listen(port, &allow_origins);
                    Box::pin(async move { r })
                }
                fn unlisten(&self) {
                    opinionated::tool::host::unlisten();
                }
            }

            struct HostClock;
            impl p::Clock for HostClock {
                fn now_ms(&self) -> i64 {
                    ::std::time::SystemTime::now().duration_since(::std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
                }
                fn utc_offset_secs(&self, at_ms: i64) -> i32 {
                    opinionated::tool::host::utc_offset(at_ms)
                }
            }

            p::Ports {
                http: Box::new($crate::export::WasiHttp),
                clock: Box::new(HostClock),
                secrets: Box::new(HostSecrets),
                asker: Some(Box::new(HostAsker)),
                progress: Box::new(HostProgress),
                player: None,
                timer: Box::new($crate::export::WasiTimer),
                spawn: Box::new($crate::export::TaskSpawn),
                notify: Box::new(HostNotify),
                listener: Some(Box::new(HostListener)),
                // A host that grants a data directory mounts it here.
                data_dir: Some(::std::path::PathBuf::from("/data")).filter(|d| d.is_dir()),
            }
        }
    };
}
