//! Agents: MCP clients of `ve mcp serve` asking to read a Module. An agent is its client name
//! plus a key `ve mcp serve` issued it on first contact; it reads a Module only under an Agent
//! access [`Permission`] carrying that name, Module and key. Anything else is refused with an
//! approval request: a 4-character code, held here (never saved) for ten minutes, that the user
//! approves in Settings → Permissions or with `ve mcp --approve CODE`. The host is told of each
//! new request so the tray can say who asked for what.
//!
//! Each Module's tools are in its own module here ([`files`], [`performance`], [`network`],
//! [`apps`]). They share what this one defines: row caps and cursors ([`Page`]) and the
//! [`Refusal`] of a call that cannot be answered.

pub mod apps;
pub mod files;
pub mod network;
pub mod performance;

use crate::Engine;
use crate::permissions::{Kind, Permission};
use crate::timeline::Moment;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

/// The Modules agents can read, by the name their Permission carries.
pub const FILES: &str = "Files";
pub const PERFORMANCE: &str = "Performance";
pub const NETWORK: &str = "Network";
pub const APPS: &str = "Apps";

/// Rows an answer holds unless the call asks for another number.
pub const DEFAULT_LIMIT: usize = 50;
/// The most rows one answer holds, whatever the call asks for.
pub const MAX_LIMIT: usize = 200;

/// A Module agents can read: each is a Permission of its own and a socket method of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Module {
    Files,
    Performance,
    Network,
    Apps,
}

impl Module {
    pub const ALL: [Module; 4] = [Module::Files, Module::Performance, Module::Network, Module::Apps];

    /// Its name, as its Permission and approval requests carry it.
    pub fn name(self) -> &'static str {
        match self {
            Module::Files => FILES,
            Module::Performance => PERFORMANCE,
            Module::Network => NETWORK,
            Module::Apps => APPS,
        }
    }

    /// The socket method that answers its tools.
    pub fn method(self) -> &'static str {
        match self {
            Module::Files => "files",
            Module::Performance => "performance",
            Module::Network => "network",
            Module::Apps => "apps",
        }
    }

    /// The Module whose tools the socket method `method` answers.
    pub fn from_method(method: &str) -> Option<Module> {
        Module::ALL.into_iter().find(|m| m.method() == method)
    }

    /// The Module of the tool called `name`, if there is such a tool.
    pub fn of_tool(name: &str) -> Option<Module> {
        Module::ALL.into_iter().find(|m| m.has_tool(name))
    }

    fn has_tool(self, name: &str) -> bool {
        match self {
            Module::Files => files::Tool::from_name(name).is_some(),
            Module::Performance => performance::Tool::from_name(name).is_some(),
            Module::Network => network::Tool::from_name(name).is_some(),
            Module::Apps => apps::Tool::from_name(name).is_some(),
        }
    }

    /// Its tools, as `tools/list` describes them.
    pub fn tools(self) -> Vec<ToolInfo> {
        macro_rules! of {
            ($tool:ty) => {
                <$tool>::ALL
                    .into_iter()
                    .map(|t| ToolInfo {
                        module: self,
                        name: t.name(),
                        description: t.description(),
                        input_schema: t.input_schema(),
                    })
                    .collect()
            };
        }
        match self {
            Module::Files => of!(files::Tool),
            Module::Performance => of!(performance::Tool),
            Module::Network => of!(network::Tool),
            Module::Apps => of!(apps::Tool),
        }
    }
}

/// A tool an agent can call, as `tools/list` describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    /// The Module whose Permission it needs.
    pub module: Module,
    pub name: &'static str,
    pub description: &'static str,
    /// The JSON Schema of its arguments.
    pub input_schema: Value,
}

/// Every tool agents can call, by Module.
pub fn tools() -> Vec<ToolInfo> {
    Module::ALL.into_iter().flat_map(Module::tools).collect()
}

/// Why a call was not answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The agent has no Permission yet; the user approves the request's code.
    ApprovalPending,
    /// The agent has no Permission, and no app runs to ask the user.
    NotApproved,
    /// The Module is only read by the running app (its recorder, its program list), and none
    /// runs.
    NotRunning,
    UnknownTool,
    BadArguments,
    BadQuery,
    BadPath,
    /// Nothing was recorded at the Moment asked for (a gap, older than an hour, or a system
    /// whose counters cannot be read).
    NoSample,
    /// The app is still reading its Index.
    Loading,
    /// No saved Index to read (no app running, and none built yet).
    NoIndex,
    /// Not the agent's doing: the app answered what `ve mcp` could not read, or the agent's key
    /// could not be kept. Calling again may work.
    Failed,
}

/// A call the agent can correct or wait on, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub reason: Reason,
    pub message: String,
    /// The approval code, for [`Reason::ApprovalPending`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// When that code expires, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl Refusal {
    pub fn new(reason: Reason, message: impl Into<String>) -> Refusal {
        Refusal {
            reason,
            message: message.into(),
            code: None,
            expires_at: None,
        }
    }

    /// The refusal of an agent waiting for the user to approve `request`.
    pub fn pending(request: &Request, now: i64) -> Refusal {
        let Request { code, module, .. } = request;
        let minutes = request.minutes_left(now);
        Refusal {
            reason: Reason::ApprovalPending,
            message: format!(
                "approval pending, code {code}: this agent may not read {module} yet. Ask the user to approve code {code} \
                 in view-everything (Settings → Permissions) or by running `ve mcp --approve {code}`, then call again. \
                 The code expires in {minutes} minute{}.",
                if minutes == 1 { "" } else { "s" }
            ),
            code: Some(code.clone()),
            expires_at: Some(request.expires_at),
        }
    }

    /// A Query that does not parse, as the parser said.
    pub fn bad_query(why: impl std::fmt::Display) -> Refusal {
        Refusal::new(
            Reason::BadQuery,
            format!("bad Query: {why} (see the Query language in docs/guide/query-language.md)"),
        )
    }

    /// A tool name no Module has.
    pub fn unknown_tool(name: &str) -> Refusal {
        let names: Vec<_> = tools().into_iter().map(|t| t.name).collect();
        Refusal::new(
            Reason::UnknownTool,
            format!("unknown tool `{name}`; the tools are {}", names.join(", ")),
        )
    }
}

/// The arguments `arguments` (an object, or null for none) of the tool `name`, or why not.
fn arguments<T: DeserializeOwned + Default>(name: &str, arguments: Value) -> Result<T, Refusal> {
    match arguments {
        Value::Null => Ok(T::default()),
        arguments => {
            serde_json::from_value(arguments).map_err(|e| Refusal::new(Reason::BadArguments, format!("bad arguments for {name}: {e}")))
        }
    }
}

/// Which rows of an answer a call asks for: where they start (from its cursor) and how many,
/// capped at [`MAX_LIMIT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    offset: usize,
    limit: usize,
}

impl Page {
    /// The page of the tool `name`'s call with `limit` and `cursor`.
    pub fn parse(name: &str, limit: Option<u64>, cursor: Option<&str>) -> Result<Page, Refusal> {
        let bad = |what: &str| Err(Refusal::new(Reason::BadArguments, format!("{name} {what}")));
        let limit = match limit {
            None => DEFAULT_LIMIT,
            Some(0) => return bad("needs a limit of at least 1"),
            Some(n) => usize::try_from(n).unwrap_or(usize::MAX).min(MAX_LIMIT),
        };
        let offset = match cursor {
            None => 0,
            Some(cursor) => match cursor.parse() {
                Ok(offset) => offset,
                Err(_) => return bad("got a cursor it did not hand out; pass next_cursor as it came"),
            },
        };
        Ok(Page { offset, limit })
    }

    /// Rows per page.
    pub fn limit(self) -> usize {
        self.limit
    }

    /// Where this page of `total` rows ends, and the cursor for the rows after it.
    fn end(self, total: usize) -> (usize, Option<String>) {
        let end = self.offset.saturating_add(self.limit).min(total);
        (end, (end < total).then(|| end.to_string()))
    }

    /// Cuts this page out of `items`: the rows, and the cursor for the ones after them.
    pub fn cut<T>(self, items: Vec<T>) -> (Vec<T>, Option<String>) {
        let (end, next) = self.end(items.len());
        let page = items.into_iter().skip(self.offset).take(end.saturating_sub(self.offset)).collect();
        (page, next)
    }

    /// `fields` with `rows` paged beside them: `total`, the page's `rows`, `next_cursor` and
    /// `limit`. Only the page's rows are turned into JSON.
    fn answer<T>(self, rows: impl ExactSizeIterator<Item = T>, row: impl Fn(T) -> Value, fields: Value) -> Value {
        let total = rows.len();
        let (end, next) = self.end(total);
        let page = rows.skip(self.offset).take(end.saturating_sub(self.offset)).map(row).collect();
        let mut answer = fields;
        answer["total"] = json!(total);
        answer["rows"] = Value::Array(page);
        answer["next_cursor"] = json!(next);
        answer["limit"] = json!(self.limit);
        answer
    }
}

/// The schema of `limit`.
fn limit_schema() -> Value {
    json!({
        "type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
        "description": format!("Rows per answer (default {DEFAULT_LIMIT}, at most {MAX_LIMIT}).")
    })
}

/// The schema of `cursor`.
fn cursor_schema() -> Value {
    json!({ "type": "string", "description": "The next_cursor of the previous answer, for its next rows." })
}

/// The schema of `at`, a Moment.
fn at_schema() -> Value {
    json!({
        "type": "integer",
        "description": "A second of the last hour, in Unix seconds; omitted for Live (the newest reading)."
    })
}

/// The schemas of `from` and `to`, a stretch of the recorded hour.
fn range_schema() -> (Value, Value) {
    (
        json!({ "type": "integer", "description": "First second, in Unix seconds (default: an hour before `to`)." }),
        json!({ "type": "integer", "description": "Last second, in Unix seconds (default: now)." }),
    )
}

/// The Moment `at` stands for: a time, or Live when omitted.
fn moment(at: Option<i64>) -> Moment {
    at.map_or(Moment::Live, Moment::At)
}

/// The stretch `from` to `to` (inclusive) of the tool `name`'s call at `now`: by default the
/// hour the recorder keeps, up to now.
fn range(name: &str, from: Option<i64>, to: Option<i64>, now: i64) -> Result<(i64, i64), Refusal> {
    let to = to.unwrap_or(now);
    let from = from.unwrap_or(to - crate::perf::SPAN + 1);
    if from > to {
        return Err(Refusal::new(
            Reason::BadArguments,
            format!("{name} needs `from` no later than `to`"),
        ));
    }
    Ok((from, to))
}

/// Orders two measures that can be unknown: largest first, unknown last.
fn largest_first<T: PartialOrd>(a: Option<T>, b: Option<T>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.partial_cmp(&a).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// How long an approval code can be approved, in seconds.
pub const CODE_LIFETIME: i64 = 10 * 60;

/// The characters codes are made of: no 0/O or 1/I to mistake for each other.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Who is asking: the MCP client's name and the key it was issued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub key: String,
}

/// An agent's request for a Module, waiting for the user to approve its code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub code: String,
    /// The agent's name.
    pub agent: String,
    pub module: String,
    /// Unix seconds.
    pub requested_at: i64,
    /// Unix seconds; the code cannot be approved from then on.
    pub expires_at: i64,
    /// Kept here: approving grants this key, and only this key.
    #[serde(skip)]
    key: String,
}

impl Request {
    /// Whole minutes, rounded up, until the code expires at `now` (Unix seconds); 0 once it has.
    pub fn minutes_left(&self, now: i64) -> u64 {
        (self.expires_at - now).max(0).unsigned_abs().div_ceil(60)
    }
}

/// What an agent may do now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    Allowed,
    /// Refused until the user approves this request's code.
    Pending(Request),
}

/// Called with each new request (the tray's notification).
pub type OnRequest = Box<dyn Fn(&Request) + Send + Sync>;

/// The pending requests and who is told of new ones.
#[derive(Default)]
pub struct Store {
    pending: Mutex<Vec<Request>>,
    on_request: Mutex<Option<Arc<OnRequest>>>,
}

/// Agents' access through the [`Engine`] handle (`agents.pending`, `agents.approve`).
pub struct Agents<'a> {
    engine: &'a Engine,
}

impl Engine {
    /// Agents' access to Modules: checks, pending requests and their approval.
    pub fn agents(&self) -> Agents<'_> {
        Agents { engine: self }
    }
}

/// Whether `list` holds a Permission for `agent` (name and key) on `module`. Shared with
/// `ve mcp serve` reading the saved settings when no app runs.
pub fn allowed(list: &[Permission], agent: &Agent, module: &str) -> bool {
    list.iter().any(|p| {
        p.kind == Kind::Agent
            && p.subject == agent.name
            && p.module.as_deref() == Some(module)
            && p.key.as_deref() == Some(agent.key.as_str())
    })
}

/// A new agent key: 128 random bits as hex.
pub fn new_key() -> String {
    format!("{:016x}{:016x}", random(), random())
}

/// 64 bits from the operating system's random number generator.
fn random() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system's random number generator");
    u64::from_le_bytes(bytes)
}

fn new_code(taken: &[Request]) -> String {
    loop {
        let mut bits = random();
        let code: String = (0..4)
            .map(|_| {
                let c = CODE_ALPHABET[(bits % CODE_ALPHABET.len() as u64) as usize];
                bits /= CODE_ALPHABET.len() as u64;
                c as char
            })
            .collect();
        if !taken.iter().any(|r| r.code == code) {
            return code;
        }
    }
}

impl Agents<'_> {
    fn store(&self) -> &Store {
        &self.engine.shared.agents
    }

    /// Has `on_request` called with each new request.
    pub fn set_on_request(&self, on_request: OnRequest) {
        *self.store().on_request.lock() = Some(Arc::new(on_request));
    }

    /// Whether `agent` may read `module` at `now` (Unix seconds). A first contact, a revoked
    /// agent or a known name with another key is refused with a request: the one already
    /// pending for the same agent, key and Module, else a new one the host is told of.
    pub fn access(&self, agent: &Agent, module: &str, now: i64) -> Access {
        if allowed(&self.engine.permissions().list(), agent, module) {
            return Access::Allowed;
        }
        let request = {
            let mut pending = self.store().pending.lock();
            pending.retain(|r| r.expires_at > now);
            if let Some(held) = pending
                .iter()
                .find(|r| r.agent == agent.name && r.key == agent.key && r.module == module)
            {
                return Access::Pending(held.clone());
            }
            let request = Request {
                code: new_code(&pending),
                agent: agent.name.clone(),
                module: module.to_owned(),
                requested_at: now,
                expires_at: now + CODE_LIFETIME,
                key: agent.key.clone(),
            };
            pending.push(request.clone());
            request
        };
        let on_request = self.store().on_request.lock().clone();
        if let Some(on_request) = on_request {
            on_request(&request);
        }
        (self.engine.shared.repaint)();
        Access::Pending(request)
    }

    /// The requests that can still be approved at `now`, oldest first.
    pub fn pending(&self, now: i64) -> Vec<Request> {
        let mut pending = self.store().pending.lock();
        pending.retain(|r| r.expires_at > now);
        pending.clone()
    }

    /// Approves the request with `code` (any letter case) at `now`: the agent's key may read
    /// the Module from now on, replacing an older key under the same name. Returns the
    /// Permission granted; an unknown or expired code grants nothing.
    pub fn approve(&self, code: &str, now: i64) -> Result<Permission, String> {
        let code = code.trim().to_ascii_uppercase();
        let request = {
            let mut pending = self.store().pending.lock();
            let at = pending
                .iter()
                .position(|r| r.code == code)
                .ok_or_else(|| format!("No request is pending with code {code}"))?;
            let request = pending.remove(at);
            if request.expires_at <= now {
                pending.retain(|r| r.expires_at > now);
                return Err(format!("Code {code} expired; the agent gets a new one when it asks again"));
            }
            request
        };
        let permission = Permission {
            kind: Kind::Agent,
            subject: request.agent,
            granted_at: Some(now),
            module: Some(request.module),
            key: Some(request.key),
        };
        let saved = self.engine.shared.permissions.grant_agent(permission.clone());
        (self.engine.shared.repaint)();
        saved.map(|()| permission)
    }

    /// Revokes the agent `name`'s access to `module`: its next request is refused.
    pub fn revoke(&self, name: &str, module: &str) -> Result<(), String> {
        let revoked = self.engine.shared.permissions.revoke_agent(name, module);
        (self.engine.shared.repaint)();
        revoked
    }

    /// Answers `agent`'s call of `module`'s tool `tool` at `now` (Unix seconds), or refuses it:
    /// no Permission for the Module yet (with the request's code, before anything else is
    /// looked at), a bad call, or nothing to read yet.
    pub fn read(&self, module: Module, agent: &Agent, tool: &str, arguments: Value, now: i64) -> Result<Value, Refusal> {
        if let Access::Pending(request) = self.access(agent, module.name(), now) {
            return Err(Refusal::pending(&request, now));
        }
        let engine = self.engine;
        match module {
            Module::Files => files::read(engine, tool, arguments),
            Module::Performance => performance::answer(&engine.perf, tool, arguments, now),
            Module::Network => network::answer(&engine.net, tool, arguments, now),
            Module::Apps => apps::answer(engine, tool, arguments),
        }
    }
}
