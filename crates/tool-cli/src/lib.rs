//! A CLI generated at run time from a tool's [`Description`]: one subcommand
//! per command offered to the CLI, one flag per input property, following
//! opinionated/cli-tool-design.md (one JSON value on stdout, errors as
//! {"error": {...}} with a non-zero exit, progress and questions on stderr).
//!
//! Used by wasm-tool (against a component) and by native binaries (against
//! their own core), which may add custom commands.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value, json};
use tool_contract::{CommandInfo, Context, Description, Interface, ToolError, codes};

pub use tool_contract::Backend;

/// A hand-written command, added alongside the generated ones (or replacing
/// one of the same name), such as an interactive `play` or the `mcp` mode.
pub struct Custom<'a> {
    pub command: Command,
    pub run: Box<dyn FnOnce(&mut dyn Backend, &ArgMatches) -> i32 + 'a>,
}

/// How a property is spelled on the command line.
#[derive(Debug, Clone)]
struct Spelling {
    property: String,
    /// "--due-by", or "<DECK>" for a positional argument.
    shown: String,
    kind: Kind,
    file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Str,
    Bool,
    Int,
    Num,
    StrList,
    Json,
}

/// Runs the CLI: parses `args` (without the program name) against the
/// description, and returns the exit status.
pub fn run(desc: &Description, args: Vec<String>, backend: &mut dyn Backend, customs: Vec<Custom<'_>>) -> i32 {
    let mut customs: BTreeMap<String, Custom<'_>> =
        customs.into_iter().map(|c| (c.command.get_name().to_string(), c)).collect();

    let mut root = Command::new(desc.name.clone())
        .version(desc.version.clone())
        .about(desc.summary.clone())
        .long_about(if desc.long.is_empty() { desc.summary.clone() } else { format!("{}\n\n{}", desc.summary, desc.long) })
        .subcommand_required(true)
        .disable_help_subcommand(true);
    for c in &desc.config {
        let mut a = Arg::new(format!("config:{}", c.name))
            .long(flag_name(&c.name))
            .global(true)
            .help_heading("Settings")
            .display_order(1000)
            .value_name("VALUE")
            .help(c.description.clone());
        if let Some(env) = &c.env {
            a = a.env(env.clone());
        }
        if let Some(d) = &c.default {
            a = a.default_value(d.clone());
        }
        root = root.arg(a);
    }

    let mut spellings: BTreeMap<String, Vec<Spelling>> = BTreeMap::new();
    for cmd in &desc.commands {
        if customs.contains_key(&cmd.name) || !cmd.offered_to(Interface::Cli) {
            continue;
        }
        if !desc.available(cmd) {
            // Shown, so asking for it explains why it can't run here.
            root = root.subcommand(
                Command::new(cmd.name.clone())
                    .about(format!("{} (unavailable here: needs {})", cmd.summary, cmd.requires.join(", ")))
                    .hide(true),
            );
            spellings.insert(cmd.name.clone(), vec![]);
            continue;
        }
        let (sub, sp) = build_command(cmd);
        root = root.subcommand(sub);
        spellings.insert(cmd.name.clone(), sp);
    }
    for c in customs.values() {
        root = root.subcommand(c.command.clone());
    }

    let mut argv = vec![desc.name.clone()];
    argv.extend(args);
    let matches = match root.try_get_matches_from(argv) {
        Ok(m) => m,
        Err(e) => {
            use clap::error::ErrorKind::*;
            if matches!(e.kind(), DisplayHelp | DisplayVersion | DisplayHelpOnMissingArgumentOrSubcommand) {
                let _ = e.print();
                return 0;
            }
            let code = if e.kind() == MissingRequiredArgument { codes::MISSING_ARGUMENT } else { codes::INVALID_ARGUMENT };
            let text = e.render().to_string();
            let first = text.lines().next().unwrap_or("").trim_start_matches("error: ").to_string();
            return fail(&ToolError::new(code, format!("{first} (see --help). Nothing was changed.")), &|n| n.to_string());
        }
    };

    let config: Map<String, Value> = desc
        .config
        .iter()
        .filter_map(|c| matches.get_one::<String>(&format!("config:{}", c.name)).map(|v| (c.name.clone(), json!(v))))
        .collect();
    if let Err(e) = backend.configure(&config) {
        return fail(&e, &|n| n.to_string());
    }

    let Some((name, sub)) = matches.subcommand() else { return 0 };
    if let Some(custom) = customs.remove(name) {
        return (custom.run)(backend, sub);
    }
    let cmd = desc.command(name).expect("generated from the description");
    let sp = &spellings[name];
    let spell = |property: &str| -> String {
        sp.iter()
            .find(|s| s.property == property)
            .map(|s| s.shown.clone())
            .or_else(|| desc.config.iter().find(|c| c.name == property).map(|c| format!("--{}", flag_name(&c.name))))
            // An argument other interfaces have but the CLI doesn't.
            .unwrap_or_else(|| format!("{property} (not available on the command line)"))
    };
    if !desc.available(cmd) {
        return fail(
            &ToolError::new(
                codes::FEATURE_UNAVAILABLE,
                format!("{name} isn't available here: it needs {}, which this host doesn't provide", cmd.requires.join(", ")),
            ),
            &spell,
        );
    }

    // Values for the input, and whether stdin is still free for questions.
    let mut input = Map::new();
    let mut stdin_used = false;
    for s in sp {
        let Some(v) = value_of(sub, s) else { continue };
        let v = if s.file {
            let path = v.as_str().unwrap_or_default();
            match read_file(path) {
                Ok(text) => {
                    stdin_used |= path == "-";
                    Value::String(text)
                }
                Err(e) => {
                    return fail(&ToolError::new("file_not_found", format!("can't read {path}: {e}. Nothing was changed.")), &spell);
                }
            }
        } else {
            v
        };
        input.insert(s.property.clone(), v);
    }

    let cx = Context {
        interactive: !stdin_used && std::io::stdin().is_terminal(),
        ..Context::new("terminal")
    };
    match backend.invoke(&cx, name, Value::Object(input)) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            0
        }
        Err(e) => fail(&e, &spell),
    }
}

fn fail(e: &ToolError, spell: &dyn Fn(&str) -> String) -> i32 {
    let rendered = ToolError { message: e.render(spell), ..e.clone() };
    println!("{}", serde_json::to_string_pretty(&json!({"error": rendered})).unwrap_or_default());
    eprintln!("error [{}]: {}", rendered.code, rendered.message);
    1
}

fn read_file(path: &str) -> std::io::Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        std::fs::read_to_string(path)
    }
}

fn flag_name(property: &str) -> String {
    property.replace('_', "-")
}

/// The schema's type, ignoring "null" (an Option).
fn type_of(schema: &Value) -> Option<String> {
    match schema.get("type") {
        Some(Value::String(t)) => Some(t.clone()),
        Some(Value::Array(ts)) => ts.iter().filter_map(|t| t.as_str()).find(|t| *t != "null").map(str::to_string),
        _ if schema.get("enum").is_some() => Some("string".into()),
        _ => None,
    }
}

fn enum_values(schema: &Value) -> Option<Vec<String>> {
    schema
        .get("enum")
        .and_then(|e| e.as_array())
        .map(|vs| vs.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}

fn build_command(cmd: &CommandInfo) -> (Command, Vec<Spelling>) {
    let mut about_long = cmd.summary.clone();
    if !cmd.long.is_empty() {
        about_long = format!("{about_long}\n\n{}", cmd.long);
    }
    if !cmd.examples.is_empty() {
        about_long = format!("{about_long}\n\nExamples:\n{}", cmd.examples.iter().map(|e| format!("  {e}")).collect::<Vec<_>>().join("\n"));
    }
    let mut sub = Command::new(cmd.name.clone()).about(cmd.summary.clone()).long_about(about_long);
    let mut spellings = vec![];
    let required: Vec<&str> = cmd.input.get("required").and_then(|r| r.as_array()).map(|r| r.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    let Some(props) = cmd.input.get("properties").and_then(|p| p.as_object()) else {
        return (sub, spellings);
    };
    for (name, schema) in props {
        if let Some(only) = schema.get("x-interfaces").and_then(|v| v.as_array()) {
            if !only.iter().any(|i| i == "cli") {
                continue;
            }
        }
        let kind = match type_of(schema).as_deref() {
            Some("boolean") => Kind::Bool,
            Some("integer") => Kind::Int,
            Some("number") => Kind::Num,
            Some("string") => Kind::Str,
            Some("array") if matches!(type_of(&schema["items"]).as_deref(), Some("string")) => Kind::StrList,
            _ => Kind::Json,
        };
        let file = schema.get("x-file").and_then(|v| v.as_bool()).unwrap_or(false);
        let flag = schema.get("x-flag").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| flag_name(name));
        let positional = schema.get("x-positional").and_then(|v| v.as_u64());
        let mut help = schema.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string();
        if file {
            help = format!("{help} Give a path, or - for stdin.");
        }
        let mut arg = Arg::new(name.clone()).help(help);
        let shown;
        if let Some(index) = positional {
            shown = format!("<{}>", flag.to_uppercase());
            arg = arg.index(index as usize + 1).value_name(flag.to_uppercase());
        } else {
            let long = if kind == Kind::Json { format!("{flag}-json") } else { flag.clone() };
            shown = format!("--{long}");
            arg = arg.long(long);
            if let Some(short) = schema.get("x-short").and_then(|v| v.as_str()).and_then(|s| s.chars().next()) {
                arg = arg.short(short);
            }
        }
        arg = match kind {
            Kind::Bool => arg.action(ArgAction::SetTrue),
            Kind::StrList => {
                let mut a = arg.action(ArgAction::Append).value_name("VALUE");
                if let Some(vs) = schema.get("items").and_then(enum_values) {
                    a = a.value_parser(vs);
                }
                a
            }
            Kind::Int => arg.value_name("N").value_parser(clap::value_parser!(i64)),
            Kind::Num => arg.value_name("NUMBER").value_parser(clap::value_parser!(f64)),
            Kind::Json => arg.value_name("JSON"),
            Kind::Str => {
                // Values such as -30d (a date) mustn't be read as flags.
                let mut a = arg.allow_hyphen_values(true);
                if positional.is_none() {
                    a = a.value_name(if file { "PATH".into() } else { flag.to_uppercase().replace('-', "_") });
                }
                if let Some(vs) = enum_values(schema) {
                    a = a.value_parser(vs);
                }
                a
            }
        };
        if let Some(d) = schema.get("default") {
            if !matches!(d, Value::Bool(false) | Value::Null) {
                let shown_default = d.as_str().map(str::to_string).unwrap_or_else(|| d.to_string());
                // Shown in help only: the core applies defaults, so MCP and REST get the same ones.
                arg = arg.help(format!(
                    "{} [default: {shown_default}]",
                    schema.get("description").and_then(|d| d.as_str()).unwrap_or("")
                ));
            }
        }
        if required.contains(&name.as_str()) && kind != Kind::Bool {
            arg = arg.required(true);
        }
        sub = sub.arg(arg);
        spellings.push(Spelling { property: name.clone(), shown, kind, file });
    }
    (sub, spellings)
}

fn value_of(m: &ArgMatches, s: &Spelling) -> Option<Value> {
    let id = s.property.as_str();
    match s.kind {
        Kind::Bool => m.get_flag(id).then_some(Value::Bool(true)),
        Kind::Int => m.get_one::<i64>(id).map(|v| json!(v)),
        Kind::Num => m.get_one::<f64>(id).map(|v| json!(v)),
        Kind::Str => m.get_one::<String>(id).map(|v| json!(v)),
        Kind::StrList => m.get_many::<String>(id).map(|vs| json!(vs.collect::<Vec<_>>())),
        Kind::Json => m.get_one::<String>(id).map(|v| serde_json::from_str(v).unwrap_or(Value::String(v.clone()))),
    }
}

/// Flags generated from another command's input, for a custom command that
/// takes the same arguments (an interactive `play` taking play-start's).
pub struct Generated {
    pub command: Command,
    spellings: Vec<Spelling>,
}

impl Generated {
    /// The flags of `source`'s input, on a command named `name`.
    pub fn from(desc: &Description, source: &str, name: &str) -> Option<Generated> {
        let cmd = desc.command(source)?;
        // Build as if offered to the CLI: the custom command is the CLI's
        // way in to a command the CLI doesn't list itself.
        let (command, spellings) = build_command(cmd);
        Some(Generated { command: command.name(name.to_string()), spellings })
    }

    /// The input for the source command, from the parsed flags.
    pub fn input(&self, m: &ArgMatches) -> Value {
        Value::Object(self.spellings.iter().filter_map(|s| value_of(m, s).map(|v| (s.property.clone(), v))).collect())
    }

    /// Prints an error as the generated commands do, with arguments spelled
    /// as these flags, and returns the exit status.
    pub fn fail(&self, e: &ToolError) -> i32 {
        fail(e, &|p| self.spellings.iter().find(|s| s.property == p).map(|s| s.shown.clone()).unwrap_or(p.to_string()))
    }
}

/// The `describe` command: prints the tool's description as JSON (what
/// hosts, MCP and docs read).
pub fn describe_command() -> Custom<'static> {
    Custom {
        command: Command::new("describe").about("Print the tool's description (commands, schemas, routes) as JSON."),
        run: Box::new(|b: &mut dyn Backend, _| {
            println!("{}", serde_json::to_string_pretty(&b.describe()).unwrap_or_default());
            0
        }),
    }
}
