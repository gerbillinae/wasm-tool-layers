//! Conformance: checks any [`Backend`] must pass, and running the same
//! calls through two backends (a tool's native build and its component)
//! to compare what they return. A tool's tests run both, so the runtimes
//! can't drift apart unnoticed.
//!
//! ```ignore
//! let mut native = tool_native::start(&opts, make);
//! let mut wasm = tool_host::Wasm::load("tool.wasm", &grants, Default::default())?;
//! assert_eq!(conformance::check(&mut native), Vec::<String>::new());
//! let diffs = conformance::compare(&mut native, &mut wasm, &steps, conformance::drop_keys(&["at"]));
//! assert!(diffs.is_empty(), "{diffs:#?}");
//! ```

use serde_json::{Value, json};

use crate::{Backend, Context, Interface, codes};

/// Checks every backend must pass; returns what failed (empty: all passed).
pub fn check(b: &mut dyn Backend) -> Vec<String> {
    let mut failed = vec![];
    let d = b.describe();
    if d.name.is_empty() || d.version.is_empty() {
        failed.push("describe() has no name or version".into());
    }
    let mut names: Vec<&str> = d.commands.iter().map(|c| c.name.as_str()).collect();
    names.sort();
    if names.windows(2).any(|w| w[0] == w[1]) {
        failed.push("describe() names a command twice".into());
    }
    for c in &d.commands {
        for e in &c.errors {
            if !d.errors.contains_key(e) {
                failed.push(format!("{}: error {e:?} isn't registered", c.name));
            }
        }
        for f in &c.requires {
            if !d.features.contains_key(f) {
                failed.push(format!("{}: requires {f:?}, which features doesn't list", c.name));
            }
        }
    }
    for r in d.http.iter().flat_map(|h| &h.routes) {
        if let Some(cmd) = &r.command {
            match d.command(cmd) {
                None => failed.push(format!("route {} {} names no command {cmd:?}", r.method, r.path)),
                Some(c) if !c.interfaces.contains(&Interface::Rest) => {
                    failed.push(format!("route {} {} adapts {cmd:?}, which isn't offered to REST", r.method, r.path))
                }
                _ => {}
            }
        }
    }
    let cx = Context::new("terminal");
    match b.invoke(&cx, "no-such-command-here", json!({})) {
        Err(e) if e.code == codes::UNKNOWN_COMMAND => {}
        other => failed.push(format!("an unknown command gave {other:?}, not unknown_command")),
    }
    if let Some(c) = d.commands.iter().find(|c| d.available(c) && c.interfaces.contains(&Interface::Cli)) {
        match b.invoke(&cx, &c.name, json!({"no_such_argument_here": 1})) {
            Err(e) if e.code == codes::INVALID_ARGUMENT => {}
            other => failed.push(format!("{} with an unknown argument gave {other:?}, not invalid_argument", c.name)),
        }
    }
    if let Some((c, by)) = d.commands.iter().filter(|c| !d.available(c)).find_map(|c| Some((c, caller(*c.interfaces.first()?)))) {
        match b.invoke(&Context::new(by), &c.name, json!({})) {
            Err(e) if e.code == codes::FEATURE_UNAVAILABLE => {}
            other => failed.push(format!("{} needs an unavailable feature but gave {other:?}", c.name)),
        }
    }
    let first = b.state(None);
    if let Some(s) = &first {
        if s.get("revision").and_then(Value::as_u64).is_none() {
            failed.push("state() has no revision".into());
        }
        let again = b.state(None).and_then(|s| s.get("revision").and_then(Value::as_u64));
        if again < s.get("revision").and_then(Value::as_u64) {
            failed.push("state()'s revision went backwards".into());
        }
    }
    failed
}

/// The `by` a call from an interface carries.
pub fn caller(i: Interface) -> &'static str {
    match i {
        Interface::Cli => "terminal",
        Interface::Tui => "tui",
        Interface::Mcp => "mcp",
        Interface::Rest => "web",
    }
}

/// One call, by an interface (`by`, as a Context names it).
#[derive(Debug, Clone)]
pub struct Step {
    pub by: String,
    pub command: String,
    pub input: Value,
}

impl Step {
    pub fn new(by: &str, command: &str, input: Value) -> Step {
        Step { by: by.into(), command: command.into(), input }
    }
}

/// Runs the steps through both backends in order, and returns where their
/// results differ (after `normalize`, for times and the like), then where
/// their final states differ.
pub fn compare(a: &mut dyn Backend, b: &mut dyn Backend, steps: &[Step], normalize: impl Fn(&mut Value)) -> Vec<String> {
    let run = |be: &mut dyn Backend, s: &Step| {
        let mut v = match be.invoke(&Context::new(&s.by), &s.command, s.input.clone()) {
            Ok(v) => json!({"ok": v}),
            Err(e) => json!({"error": {"code": e.code, "message": e.message}}),
        };
        normalize(&mut v);
        v
    };
    let mut diffs = vec![];
    for (i, s) in steps.iter().enumerate() {
        let (x, y) = (run(a, s), run(b, s));
        if x != y {
            diffs.push(format!("step {i} ({}): {}\n  first:  {x}\n  second: {y}", s.command, first_difference(&x, &y, "")));
        }
    }
    let (mut x, mut y) = (a.state(Some(0)).unwrap_or_default(), b.state(Some(0)).unwrap_or_default());
    normalize(&mut x);
    normalize(&mut y);
    if x != y {
        diffs.push(format!("final state: {}", first_difference(&x, &y, "")));
    }
    diffs
}

/// Where two values first differ, as a path.
pub fn first_difference(x: &Value, y: &Value, at: &str) -> String {
    match (x, y) {
        (Value::Object(a), Value::Object(b)) => {
            for k in a.keys().chain(b.keys()) {
                match (a.get(k), b.get(k)) {
                    (Some(p), Some(q)) if p == q => {}
                    (Some(p), Some(q)) => return first_difference(p, q, &format!("{at}.{k}")),
                    (p, q) => return format!("{at}.{k}: {p:?} vs {q:?}"),
                }
            }
            format!("{at}: equal")
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (p, q)) in a.iter().zip(b).enumerate() {
                if p != q {
                    return first_difference(p, q, &format!("{at}[{i}]"));
                }
            }
            format!("{at}: equal")
        }
        _ => format!("{at}: {x} vs {y}"),
    }
}

/// A normalizer that masks local ports in strings (http://127.0.0.1:PORT),
/// for servers each runtime starts for itself.
pub fn mask_local_ports(v: &mut Value) {
    match v {
        Value::String(s) => {
            while let Some(i) = s.find("127.0.0.1:") {
                let start = i + "127.0.0.1:".len();
                let end = s[start..].find(|c: char| !c.is_ascii_digit()).map_or(s.len(), |e| start + e);
                if end == start {
                    break;
                }
                s.replace_range(i..end, "localhost:PORT");
            }
        }
        Value::Object(m) => m.values_mut().for_each(mask_local_ports),
        Value::Array(a) => a.iter_mut().for_each(mask_local_ports),
        _ => {}
    }
}

/// A normalizer that removes these keys wherever they appear (timestamps).
pub fn drop_keys(keys: &'static [&'static str]) -> impl Fn(&mut Value) {
    fn walk(v: &mut Value, keys: &[&str]) {
        match v {
            Value::Object(m) => {
                m.retain(|k, _| !keys.contains(&k.as_str()));
                m.values_mut().for_each(|v| walk(v, keys));
            }
            Value::Array(a) => a.iter_mut().for_each(|v| walk(v, keys)),
            _ => {}
        }
    }
    move |v| walk(v, keys)
}
