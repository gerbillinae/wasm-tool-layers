//! Calling a component's typed commands generically. The generated CLI and
//! MCP speak JSON; a component's commands interface (named by describe's
//! `wit` field) has a typed function per command:
//!
//!   list-cards: async func(cx: context, input: selection) -> result<list-result, tool-error>
//!
//! JSON is converted to WIT values by the function's own types, and back:
//! a WIT field `due-by` is JSON's `due_by`, an absent or null JSON field is
//! an option's none (or a list's, bool's or number's zero), and list<u8> is
//! base64 in JSON (JSON has no bytes).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use tool_contract::ToolError;
use wasmtime::component::{Type, Val};

fn json_name(wit: &str) -> String {
    wit.replace('-', "_")
}

/// A JSON value as a WIT value of type `ty`. `None` means the field was
/// absent, which is allowed for options, lists, bools, numbers and strings.
pub fn to_val(ty: &Type, v: Option<&Value>, at: &str) -> Result<Val, String> {
    let v = v.filter(|v| !v.is_null());
    let bad = |want: &str| format!("{at}: expected {want}, got {}", v.map(|v| v.to_string()).unwrap_or("nothing".into()));
    let int = |v: Option<&Value>| -> Result<i64, String> {
        match v {
            None => Ok(0),
            Some(v) => v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)).ok_or_else(|| bad("an integer")),
        }
    };
    Ok(match ty {
        Type::Bool => Val::Bool(match v {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| bad("true or false"))?,
        }),
        Type::S8 => Val::S8(int(v)? as i8),
        Type::U8 => Val::U8(int(v)? as u8),
        Type::S16 => Val::S16(int(v)? as i16),
        Type::U16 => Val::U16(int(v)? as u16),
        Type::S32 => Val::S32(int(v)? as i32),
        Type::U32 => Val::U32(int(v)? as u32),
        Type::S64 => Val::S64(int(v)?),
        Type::U64 => Val::U64(int(v)? as u64),
        Type::Float32 => Val::Float32(v.map(|v| v.as_f64().ok_or_else(|| bad("a number"))).transpose()?.unwrap_or(0.0) as f32),
        Type::Float64 => Val::Float64(v.map(|v| v.as_f64().ok_or_else(|| bad("a number"))).transpose()?.unwrap_or(0.0)),
        Type::Char => Val::Char(v.and_then(|v| v.as_str()).and_then(|s| s.chars().next()).ok_or_else(|| bad("a character"))?),
        Type::String => Val::String(match v {
            None => String::new(),
            Some(v) => v.as_str().ok_or_else(|| bad("a string"))?.to_string(),
        }),
        Type::List(l) => {
            let el = l.ty();
            match (v, &el) {
                (None, _) => Val::List(vec![]),
                (Some(Value::String(s)), Type::U8) => {
                    Val::List(STANDARD.decode(s).map_err(|_| bad("base64 bytes"))?.into_iter().map(Val::U8).collect())
                }
                (Some(Value::Array(items)), _) => Val::List(
                    items.iter().enumerate().map(|(i, x)| to_val(&el, Some(x), &format!("{at}[{i}]"))).collect::<Result<_, _>>()?,
                ),
                _ => return Err(bad("a list")),
            }
        }
        Type::Record(r) => {
            let empty = Map::new();
            let obj = match v {
                None => &empty,
                Some(Value::Object(o)) => o,
                _ => return Err(bad("an object")),
            };
            let names: Vec<String> = r.fields().map(|f| json_name(f.name)).collect();
            if let Some(extra) = obj.keys().find(|k| !names.contains(k)) {
                return Err(format!("{at}: no field {extra:?}"));
            }
            Val::Record(
                r.fields()
                    .map(|f| {
                        let key = json_name(f.name);
                        Ok((f.name.to_string(), to_val(&f.ty, obj.get(&key), &format!("{at}.{key}"))?))
                    })
                    .collect::<Result<_, String>>()?,
            )
        }
        Type::Enum(e) => {
            let s = v.and_then(|v| v.as_str()).ok_or_else(|| bad("a string"))?;
            if !e.names().any(|n| n == s) {
                return Err(format!("{at}: {s:?} isn't one of {}", e.names().collect::<Vec<_>>().join(", ")));
            }
            Val::Enum(s.to_string())
        }
        Type::Option(o) => match v {
            None => Val::Option(None),
            Some(x) => Val::Option(Some(Box::new(to_val(&o.ty(), Some(x), at)?))),
        },
        Type::Variant(var) => {
            // {"case": payload} or "case"
            let (case, payload) = match v {
                Some(Value::String(s)) => (s.clone(), None),
                Some(Value::Object(o)) if o.len() == 1 => {
                    let (k, p) = o.iter().next().unwrap();
                    (k.clone(), Some(p))
                }
                _ => return Err(bad("a variant")),
            };
            let c = var.cases().find(|c| json_name(c.name) == case).ok_or_else(|| format!("{at}: no case {case:?}"))?;
            let payload = match c.ty {
                Some(t) => Some(Box::new(to_val(&t, payload, at)?)),
                None => None,
            };
            Val::Variant(c.name.to_string(), payload)
        }
        Type::Tuple(t) => {
            let items = v.and_then(|v| v.as_array()).ok_or_else(|| bad("a list"))?;
            Val::Tuple(t.types().zip(items).map(|(ty, x)| to_val(&ty, Some(x), at)).collect::<Result<_, _>>()?)
        }
        Type::Flags(_) => Val::Flags(
            v.and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
        ),
        other => return Err(format!("{at}: {other:?} can't come from JSON")),
    })
}

/// A WIT value of type `ty` as JSON.
pub fn to_json(ty: &Type, v: &Val) -> Value {
    match (ty, v) {
        (_, Val::Bool(b)) => json!(b),
        (_, Val::S8(n)) => json!(n),
        (_, Val::U8(n)) => json!(n),
        (_, Val::S16(n)) => json!(n),
        (_, Val::U16(n)) => json!(n),
        (_, Val::S32(n)) => json!(n),
        (_, Val::U32(n)) => json!(n),
        (_, Val::S64(n)) => json!(n),
        (_, Val::U64(n)) => json!(n),
        (_, Val::Float32(n)) => json!(n),
        (_, Val::Float64(n)) => json!(n),
        (_, Val::Char(c)) => json!(c.to_string()),
        (_, Val::String(s)) => json!(s),
        (Type::List(l), Val::List(items)) => match l.ty() {
            Type::U8 => json!(STANDARD.encode(items.iter().map(|x| if let Val::U8(b) = x { *b } else { 0 }).collect::<Vec<u8>>())),
            el => Value::Array(items.iter().map(|x| to_json(&el, x)).collect()),
        },
        (Type::Record(r), Val::Record(fields)) => Value::Object(
            r.fields()
                .zip(fields)
                .map(|(f, (_, x))| (json_name(f.name), to_json(&f.ty, x)))
                .collect(),
        ),
        (_, Val::Enum(s)) => json!(s),
        (Type::Option(o), Val::Option(x)) => x.as_ref().map(|x| to_json(&o.ty(), x)).unwrap_or(Value::Null),
        (Type::Variant(var), Val::Variant(case, payload)) => {
            let ty = var.cases().find(|c| c.name == case).and_then(|c| c.ty);
            match (ty, payload) {
                (Some(t), Some(p)) => json!({ json_name(case): to_json(&t, p) }),
                _ => json!(json_name(case)),
            }
        }
        (Type::Tuple(t), Val::Tuple(items)) => Value::Array(t.types().zip(items).map(|(ty, x)| to_json(&ty, x)).collect()),
        (_, Val::Flags(f)) => json!(f),
        _ => Value::Null,
    }
}

/// A WIT tool-error value as a ToolError.
pub fn to_tool_error(v: &Val) -> ToolError {
    let mut e = ToolError::new("internal", "the component returned an error it didn't describe");
    if let Val::Record(fields) = v {
        for (name, x) in fields {
            match (name.as_str(), x) {
                ("code", Val::String(s)) => e.code = s.clone(),
                ("message", Val::String(s)) => e.message = s.clone(),
                ("details", Val::Option(Some(d))) => {
                    if let Val::String(s) = d.as_ref() {
                        e.details = serde_json::from_str(s).ok();
                    }
                }
                _ => {}
            }
        }
    }
    e
}
