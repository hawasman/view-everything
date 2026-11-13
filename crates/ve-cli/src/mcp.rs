//! `ve mcp`: the MCP server agents read the app's Modules through, and the shell's way to
//! approve them. `ve mcp serve` speaks MCP (JSON-RPC 2.0, one message per line) on stdin and
//! stdout and offers the read-only tools of [`ve_engine::agents`]. It asks the running app over
//! its socket; with none running it reads the saved Index file for the Files tools, for an agent
//! the saved settings hold a Permission for. The other Modules are read by the running app
//! only. Agent identity is the client's name plus a key issued here on first contact and kept in
//! the data folder.

use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;
use ve_core::paths::APP_DIR;
use ve_core::{Index, Query, QueryContext};
use ve_engine::agents::files::{self, Call};
use ve_engine::agents::{self, Agent, FILES, Module, Reason, Refusal, Request, ToolInfo};
use ve_engine::permissions::Permission;
use ve_engine::socket::{self, Client};

pub const USAGE: &str = "\
Usage:
  ve mcp serve              Serve MCP on stdin/stdout: read-only tools for an agent
  ve mcp --list-requests    Agents' approval requests waiting for the user
  ve mcp --approve <CODE>   Approve a request: that agent may read the Module it asked for

An agent's first call to a Module is refused with a 4-character code; approve it here or in
the app's Settings → Permissions within 10 minutes. Requests are held by the running app. With
the app closed, `ve mcp serve` answers the Files tools from the saved Index file for agents
already approved; the other Modules need the app running.";

/// The protocol versions this server speaks, newest last.
const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// The name an agent goes by when its client sent none.
const UNNAMED: &str = "unnamed agent";
/// The longest agent name kept, in characters.
const MAX_NAME: usize = 64;

pub fn main(args: &[String]) -> ExitCode {
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["serve"] => serve(io::stdin().lock(), io::stdout().lock()).map_err(|e| e.to_string()),
        ["--list-requests"] => list_requests(&mut io::stdout().lock()),
        ["--approve", code] => approve(code, &mut io::stdout().lock()),
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("ve: unknown mcp command\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ve: {e}");
            ExitCode::from(2)
        }
    }
}

/// Connects to the running app, or says why not.
fn connect() -> Result<Client, String> {
    let path = socket::path().ok_or("cannot work out where the app socket is")?;
    Client::connect(&path).map_err(|e| {
        if e.is_not_running() {
            "the app is not running; approval requests are held by the running app".to_owned()
        } else {
            format!("cannot ask the app: {e}")
        }
    })
}

fn list_requests(out: &mut impl Write) -> Result<(), String> {
    let pending = connect()?.call("agents.pending", Value::Null).map_err(|e| e.to_string())?;
    let pending: Vec<Request> = serde_json::from_value(pending).map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let write = |out: &mut dyn Write| -> io::Result<()> {
        if pending.is_empty() {
            return writeln!(out, "No approval requests are pending.");
        }
        writeln!(out, "CODE\tMODULE\tEXPIRES\tAGENT")?;
        for r in &pending {
            let minutes = r.minutes_left(now);
            writeln!(out, "{}\t{}\tin {minutes} min\t{}", r.code, r.module, r.agent)?;
        }
        out.flush()
    };
    write(out).map_err(|e| e.to_string())
}

fn approve(code: &str, out: &mut impl Write) -> Result<(), String> {
    let granted = connect()?
        .call("agents.approve", json!({ "code": code }))
        .map_err(|e| e.to_string())?;
    let field = |key: &str| granted[key].as_str().unwrap_or("?").to_owned();
    writeln!(out, "Approved: {} may read {}", field("agent"), field("module")).map_err(|e| e.to_string())
}

/// Serves MCP until the client closes stdin.
pub fn serve(input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut server = Server::default();
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle(message),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// What the running app made of a tool call.
enum App {
    Answered(Result<Value, Refusal>),
    /// No app answers.
    Absent,
    /// The app answering is another version, which does not take the call.
    Incompatible,
}

#[derive(Default)]
struct Server {
    /// The client's name, from `initialize`.
    name: Option<String>,
    agent: Option<Agent>,
    engine: Option<Client>,
    /// The saved Index last read, and its file's modification time.
    saved: Option<(SystemTime, Index)>,
}

impl Server {
    /// The reply to one message; `None` for a notification or a client's response.
    fn handle(&mut self, message: Value) -> Option<Value> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return message
                .get("id")
                .filter(|_| message.get("result").is_none() && message.get("error").is_none())
                .map(|id| error(id.clone(), -32600, "invalid request: no method"));
        };
        // A notification (no id) asks for no reply.
        let id = message.get("id")?.clone();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => json!({}),
            "tools/list" => json!({ "tools": agents::tools().into_iter().map(tool).collect::<Vec<_>>() }),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(id, -32602, "tools/call needs a tool name"));
                };
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                self.call(name, arguments)
            }
            other => return Some(error(id, -32601, &format!("method not found: {other}"))),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    fn initialize(&mut self, params: &Value) -> Value {
        // The name is shown to the user (Settings, the desktop notification): one short line.
        let name: Option<String> = params["clientInfo"]["name"]
            .as_str()
            .map(|n| {
                n.chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_NAME)
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .filter(|n| !n.is_empty());
        self.name = Some(name.unwrap_or_else(|| UNNAMED.to_owned()));
        self.agent = None;
        let asked = params["protocolVersion"].as_str().unwrap_or_default();
        let version = PROTOCOL_VERSIONS
            .into_iter()
            .find(|v| *v == asked)
            .unwrap_or(PROTOCOL_VERSIONS[PROTOCOL_VERSIONS.len() - 1]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "view-everything", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Read-only access to view-everything's Modules: Files (the user's file Index), Performance \
                (the last hour of CPU, memory and processes), Network (the last hour of interface traffic, and open \
                connections) and Apps (installed programs with their size, and what starts at login). Each Module needs \
                its own approval: the first call to one \
                is refused with an approval code until the user approves this agent; tell the user the code. \
                Every answer is capped in rows; pass next_cursor back as cursor for the next rows.",
        })
    }

    /// A tool's MCP result: the answer as text and as structured content, or the refusal.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        match self.answer(name, arguments) {
            Ok((mut answer, source)) => {
                answer["source"] = json!(source);
                let text = serde_json::to_string_pretty(&answer).unwrap_or_default();
                json!({ "content": [{ "type": "text", "text": text }], "structuredContent": answer, "isError": false })
            }
            Err(refusal) => json!({
                "content": [{ "type": "text", "text": refusal.message }],
                "structuredContent": { "refusal": refusal },
                "isError": true,
            }),
        }
    }

    /// Answers a tool call from the running app, else (Files only) from the saved Index file;
    /// the second value says which.
    fn answer(&mut self, name: &str, arguments: Value) -> Result<(Value, &'static str), Refusal> {
        let module = Module::of_tool(name).ok_or_else(|| Refusal::unknown_tool(name))?;
        let agent = self.agent()?;
        let app = self.ask_app(&agent, module, name, &arguments);
        let module_name = module.name();
        match app {
            App::Answered(answered) => answered.map(|answer| (answer, "running app")),
            App::Absent | App::Incompatible if module == Module::Files => {
                self.read_file(&agent, name, arguments).map(|answer| (answer, "saved Index file"))
            }
            App::Absent => Err(Refusal::new(
                Reason::NotRunning,
                format!(
                    "{module_name} is read by the running app, and view-everything is not running. \
                     Ask the user to start view-everything, then call again."
                ),
            )),
            App::Incompatible => Err(Refusal::new(
                Reason::Failed,
                format!(
                    "the running view-everything is another version, which does not answer {module_name} for agents. \
                     Ask the user to update view-everything and run it again."
                ),
            )),
        }
    }

    /// This client as an agent: its name and its key, issued now if it has none yet.
    fn agent(&mut self) -> Result<Agent, Refusal> {
        if let Some(agent) = &self.agent {
            return Ok(agent.clone());
        }
        let name = self.name.clone().unwrap_or_else(|| UNNAMED.to_owned());
        let key = agent_key(&name).map_err(|e| {
            Refusal::new(
                Reason::Failed,
                format!("cannot keep this agent's key, so it cannot be approved: {e}"),
            )
        })?;
        let agent = Agent { name, key };
        self.agent = Some(agent.clone());
        Ok(agent)
    }

    /// Asks the running app to answer the tool `name` of `module`.
    fn ask_app(&mut self, agent: &Agent, module: Module, name: &str, arguments: &Value) -> App {
        let params = json!({ "agent": agent, "tool": name, "arguments": arguments });
        // A connection the app closed (it restarted) is opened again once.
        for _ in 0..2 {
            if self.engine.is_none() {
                self.engine = socket::path().and_then(|path| Client::connect(&path).ok());
            }
            let Some(engine) = self.engine.as_mut() else {
                return App::Absent;
            };
            let reply = match engine.call(module.method(), params.clone()) {
                Ok(reply) => reply,
                Err(socket::Error::Refused(_)) => {
                    self.engine = None;
                    return App::Incompatible;
                }
                Err(socket::Error::Io(_)) => {
                    self.engine = None;
                    continue;
                }
            };
            return App::Answered(match (reply.get("answer"), reply.get("refusal")) {
                (Some(answer), _) => Ok(answer.clone()),
                (None, Some(refusal)) => Err(serde_json::from_value(refusal.clone())
                    .unwrap_or_else(|e| Refusal::new(Reason::Failed, format!("unreadable refusal from the app: {e}")))),
                (None, None) => Err(Refusal::new(Reason::Failed, "the app answered nothing")),
            });
        }
        App::Absent
    }

    /// Answers from the saved Index file, for an agent the saved settings approve.
    fn read_file(&mut self, agent: &Agent, name: &str, arguments: Value) -> Result<Value, Refusal> {
        let not_approved = |why: String| Refusal::new(Reason::NotApproved, why);
        let permissions = saved_permissions().map_err(|e| not_approved(format!("cannot read the settings: {e}")))?;
        if !agents::allowed(&permissions, agent, FILES) {
            return Err(not_approved(format!(
                "this agent may not read {FILES}, and view-everything is not running to ask the user. \
                 Ask the user to start view-everything, then call again for an approval code."
            )));
        }
        let call = Call::parse(name, arguments)?;
        let categories = ve_core::paths::load_categories().map_err(|e| Refusal::new(Reason::NoIndex, e))?;
        let context = QueryContext::new(categories.clone(), chrono::Local::now());
        let query = match call.query().trim() {
            "" => None,
            text => Some(Query::parse(text, &context).map_err(Refusal::bad_query)?),
        };
        if query.as_ref().is_some_and(Query::needs_offline) {
            return Err(Refusal::bad_query(
                "is:offline needs the app running; the saved Index does not know which Volumes are mounted",
            ));
        }
        let index = self.saved_index()?;
        files::answer(index, &categories, query.as_ref(), &call, false)
    }

    /// The saved Index, read again only when its file changed.
    fn saved_index(&mut self) -> Result<&Index, Refusal> {
        let no_index = |why: String| Refusal::new(Reason::NoIndex, why);
        let path = ve_core::paths::index_path().ok_or_else(|| no_index("no data folder on this system".into()))?;
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).map_err(|e| {
            no_index(format!(
                "cannot read the Index at {}: {e} (open the app once to build it)",
                path.display()
            ))
        })?;
        if self.saved.as_ref().is_none_or(|(at, _)| *at != modified) {
            let index = Index::load(&path).map_err(|e| no_index(format!("cannot read the Index at {}: {e}", path.display())))?;
            self.saved = Some((modified, index));
        }
        Ok(&self.saved.as_ref().expect("just read").1)
    }
}

/// A tool as `tools/list` describes it.
fn tool(tool: ToolInfo) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.input_schema,
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false },
    })
}

/// Where the app keeps its settings (the app's own `settings.toml`).
fn settings_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join(APP_DIR).join("settings.toml"))
}

/// The Permissions the saved settings hold.
fn saved_permissions() -> Result<Vec<Permission>, String> {
    #[derive(Deserialize)]
    struct Saved {
        #[serde(default)]
        permissions: Vec<Permission>,
    }
    let Some(path) = settings_path() else {
        return Ok(Vec::new());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<Saved>(&text)
            .map(|s| s.permissions)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Where the agents' keys are kept, by agent name.
fn keys_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join(APP_DIR).join("agent-keys.json"))
}

/// The key of the agent called `name`, issuing and keeping a new one on its first contact.
fn agent_key(name: &str) -> Result<String, String> {
    let path = keys_path().ok_or("no data folder on this system")?;
    let mut keys: BTreeMap<String, String> = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if let Some(key) = keys.get(name) {
        return Ok(key.clone());
    }
    let key = agents::new_key();
    keys.insert(name.to_owned(), key.clone());
    write_private(&path, &serde_json::to_string_pretty(&keys).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(key)
}

/// Replaces `path` whole with `text`, readable by this user alone.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("json.partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&partial)?.write_all(text.as_bytes())?;
    std::fs::rename(&partial, path)
}
