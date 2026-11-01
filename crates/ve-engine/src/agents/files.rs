//! The Files Module's tools for agents: `files.find`, `files.top`, `files.locations` and
//! `files.summary`. Each reads the Index and nothing else, answers aggregates with at most
//! [`MAX_LIMIT`] rows, and hands back a cursor for the next rows. The same code answers on the
//! engine's live Index (the socket's `files` method) and on a saved Index file (`ve mcp serve`
//! with no app running). A call that cannot be answered is a [`Refusal`] the agent can act on.

use super::{Agent, Agents, Request};
use crate::socket::search::{self, LastSynced, Row, Select};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use ve_core::{CategoryMap, EntryId, Filter, Index, Query};

/// Rows an answer holds unless the call asks for another number.
pub const DEFAULT_LIMIT: usize = 50;
/// The most rows one answer holds, whatever the call asks for.
pub const MAX_LIMIT: usize = 200;

/// One of the Files tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Find,
    Top,
    Locations,
    Summary,
}

impl Tool {
    pub const ALL: [Tool; 4] = [Tool::Find, Tool::Top, Tool::Locations, Tool::Summary];

    /// The name an agent calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Find => "files.find",
            Tool::Top => "files.top",
            Tool::Locations => "files.locations",
            Tool::Summary => "files.summary",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    /// What it answers, for the agent.
    pub fn description(self) -> &'static str {
        match self {
            Tool::Find => {
                "Entries (files and folders) a Query matches, by path, with their sizes, modification time, Category and Owner. \
                 Answers the number of matches and their total size, plus one page of rows and a cursor for the next page. \
                 Read-only."
            }
            Tool::Top => {
                "Folders (or with files=true, files) ranked by On-disk Size, largest first, under a path or everywhere. \
                 Under a Query, sizes count only the files it matches (Filtered Size). Answers one page of rows and a cursor. \
                 Read-only."
            }
            Tool::Locations => {
                "The Locations in the Index (Volumes and Remotes): where each is, whether it is online, its Last Synced \
                 and its size. Read-only."
            }
            Tool::Summary => {
                "Totals under a path (or everywhere), optionally under a Query: files and sizes, broken down by Category, \
                 largest first. Read-only."
            }
        }
    }

    /// The JSON Schema of its arguments.
    pub fn input_schema(self) -> Value {
        let query = json!({
            "type": "string",
            "description": "A Query in the app's Query language: words, ext:pdf, size:>1GB, dm:2024, since:7d, kind:folder, owner:steam, ..."
        });
        let path = json!({ "type": "string", "description": "Only Entries under this absolute path, which must be in the Index." });
        let limit = json!({
            "type": "integer", "minimum": 1, "maximum": MAX_LIMIT,
            "description": format!("Rows per answer (default {DEFAULT_LIMIT}, at most {MAX_LIMIT}).")
        });
        let cursor = json!({ "type": "string", "description": "The next_cursor of the previous answer, for its next rows." });
        let properties = match self {
            Tool::Find => json!({ "query": query, "path": path, "limit": limit, "cursor": cursor }),
            Tool::Top => json!({
                "path": path, "query": query, "limit": limit, "cursor": cursor,
                "files": { "type": "boolean", "description": "Rank files instead of folders." }
            }),
            Tool::Locations => json!({ "limit": limit, "cursor": cursor }),
            Tool::Summary => json!({ "path": path, "query": query, "limit": limit, "cursor": cursor }),
        };
        let mut schema = json!({ "type": "object", "properties": properties, "additionalProperties": false });
        if self == Tool::Find {
            schema["required"] = json!(["query"]);
        }
        schema
    }
}

/// A tool's arguments, as the agent sent them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Args {
    pub query: String,
    pub path: Option<String>,
    pub files: bool,
    pub limit: Option<u64>,
    pub cursor: Option<String>,
}

/// A tool and its checked arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub tool: Tool,
    pub args: Args,
    /// Where the page starts (from the cursor).
    offset: usize,
    /// Rows per page, capped.
    limit: usize,
}

impl Call {
    /// Checks `arguments` (an object, or null for none) for the tool named `name`.
    pub fn parse(name: &str, arguments: Value) -> Result<Call, Refusal> {
        let tool = Tool::from_name(name).ok_or_else(|| {
            let names: Vec<_> = Tool::ALL.iter().map(|t| t.name()).collect();
            Refusal::new(
                Reason::UnknownTool,
                format!("unknown tool `{name}`; the tools are {}", names.join(", ")),
            )
        })?;
        let args: Args = match arguments {
            Value::Null => Args::default(),
            arguments => serde_json::from_value(arguments)
                .map_err(|e| Refusal::new(Reason::BadArguments, format!("bad arguments for {name}: {e}")))?,
        };
        let bad = |what: &str| Err(Refusal::new(Reason::BadArguments, format!("{name} {what}")));
        match tool {
            Tool::Find if args.query.trim().is_empty() => return bad("needs a query"),
            Tool::Locations if !args.query.is_empty() || args.path.is_some() => return bad("takes no query or path"),
            Tool::Find | Tool::Locations | Tool::Summary if args.files => return bad("takes no `files`; it is for files.top"),
            _ => {}
        }
        let limit = match args.limit {
            None => DEFAULT_LIMIT,
            Some(0) => return bad("needs a limit of at least 1"),
            Some(n) => usize::try_from(n).unwrap_or(usize::MAX).min(MAX_LIMIT),
        };
        let offset = match &args.cursor {
            None => 0,
            Some(cursor) => match cursor.parse() {
                Ok(offset) => offset,
                Err(_) => return bad("got a cursor it did not hand out; pass next_cursor as it came"),
            },
        };
        if let Some(path) = &args.path
            && !Path::new(path).is_absolute()
        {
            return Err(Refusal::new(Reason::BadPath, format!("{path} is not an absolute path")));
        }
        Ok(Call { tool, args, offset, limit })
    }

    /// The Query text the call reads under.
    pub fn query(&self) -> &str {
        &self.args.query
    }

    /// Cuts this call's page out of `items`: the rows, and the cursor for the ones after them.
    fn page<T>(&self, items: Vec<T>) -> (Vec<T>, Option<String>) {
        let total = items.len();
        let end = self.offset.saturating_add(self.limit).min(total);
        let page = items.into_iter().skip(self.offset).take(end.saturating_sub(self.offset)).collect();
        (page, (end < total).then(|| end.to_string()))
    }
}

/// Why a call was not answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The agent has no Permission yet; the user approves the request's code.
    ApprovalPending,
    /// The agent has no Permission, and no app runs to ask the user.
    NotApproved,
    UnknownTool,
    BadArguments,
    BadQuery,
    BadPath,
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
}

/// Answers `call` on `index`, its Query already parsed. `live` says whether the Index is the
/// running app's, which knows which Locations are online.
pub fn answer(index: &Index, categories: &CategoryMap, query: Option<&Query>, call: &Call, live: bool) -> Result<Value, Refusal> {
    let path = match &call.args.path {
        None => None,
        Some(path) => {
            let path = Path::new(path);
            if index.find_path(path).is_none() {
                return Err(Refusal::new(
                    Reason::BadPath,
                    format!(
                        "{} is not in the Index; files.locations lists the Locations it holds",
                        path.display()
                    ),
                ));
            }
            Some(path)
        }
    };
    let last_synced = LastSynced::of(index);
    let filter = Filter::compute(index, query);
    let selected = |select| search::select(index, &filter, query, path, select).map_err(|e| Refusal::new(Reason::BadPath, e));
    let rows = |ids: Vec<EntryId>| -> Vec<Row> { ids.into_iter().map(|id| search::row(index, &filter, categories, id)).collect() };
    let answer = match call.tool {
        Tool::Find => {
            let ids = selected(Select::Matches)?;
            let (files, logical, on_disk) = ids
                .iter()
                .filter(|&&id| !index.is_dir(id))
                .map(|&id| filter.totals(index, id))
                .fold((0u64, 0u64, 0u64), |(n, l, d), t| (n + 1, l + t.logical, d + t.on_disk));
            let total = ids.len();
            let (page, next) = call.page(ids);
            json!({
                "total": total,
                "files": files,
                "folders": total as u64 - files,
                "logical_size": logical,
                "on_disk_size": on_disk,
                "rows": rows(page),
                "next_cursor": next,
            })
        }
        Tool::Top => {
            let ids = selected(if call.args.files {
                Select::LargestFiles
            } else {
                Select::LargestFolders
            })?;
            let total = ids.len();
            let (page, next) = call.page(ids);
            json!({ "total": total, "rows": rows(page), "next_cursor": next })
        }
        Tool::Locations => {
            let locations: Vec<Value> = index
                .volumes()
                .map(|(_, v)| {
                    let totals = filter.totals(index, v.root);
                    json!({
                        "name": v.remote_display_name().unwrap_or_else(|| v.mount.to_string_lossy().into_owned()),
                        "path": v.mount.to_string_lossy(),
                        "kind": v.remote().map_or("volume", |r| r.kind()),
                        "fs_type": &v.fs_type,
                        // A saved Index does not know what is mounted now.
                        "online": live.then_some(v.online),
                        "last_synced": v.last_synced,
                        "files": totals.files,
                        "logical_size": totals.logical,
                        "on_disk_size": totals.on_disk,
                        "capacity": v.capacity.map(|c| json!({ "total": c.total, "free": c.free })),
                    })
                })
                .collect();
            let total = locations.len();
            let (page, next) = call.page(locations);
            json!({ "total": total, "rows": page, "next_cursor": next })
        }
        Tool::Summary => {
            let mut by_category: HashMap<&str, (u64, u64, u64)> = HashMap::new();
            let mut sums = (0u64, 0u64, 0u64);
            for id in search::scope(index, path).map_err(|e| Refusal::new(Reason::BadPath, e))? {
                if index.is_dir(id) || !filter.is_counted(id) {
                    continue;
                }
                let t = filter.totals(index, id);
                let entry = by_category.entry(categories.of_name(index.name(id)).name()).or_default();
                *entry = (entry.0 + 1, entry.1 + t.logical, entry.2 + t.on_disk);
                sums = (sums.0 + 1, sums.1 + t.logical, sums.2 + t.on_disk);
            }
            let mut categories: Vec<_> = by_category.into_iter().collect();
            categories.sort_by(|a, b| b.1.2.cmp(&a.1.2).then(a.0.cmp(b.0)));
            let total = categories.len();
            let (page, next) = call.page(categories);
            let rows: Vec<Value> = page
                .into_iter()
                .map(|(category, (files, logical, on_disk))| {
                    json!({ "category": category, "files": files, "logical_size": logical, "on_disk_size": on_disk })
                })
                .collect();
            json!({
                "path": path.map(|p| p.to_string_lossy()),
                "files": sums.0,
                "logical_size": sums.1,
                "on_disk_size": sums.2,
                "total": total,
                "rows": rows,
                "next_cursor": next,
            })
        }
    };
    let mut answer = answer;
    answer["limit"] = json!(call.limit);
    answer["last_synced"] = json!(last_synced);
    Ok(answer)
}

impl Agents<'_> {
    /// Answers `agent`'s call of the Files tool `tool` at `now` on the live Index, or refuses it:
    /// no Permission yet (with the request's code, before anything else is looked at), a bad
    /// call, or the Index still loading.
    pub fn files(&self, agent: &Agent, tool: &str, arguments: Value, now: i64) -> Result<Value, Refusal> {
        if let super::Access::Pending(request) = self.access(agent, super::FILES, now) {
            return Err(Refusal::pending(&request, now));
        }
        let call = Call::parse(tool, arguments)?;
        let engine = self.engine;
        if engine.is_loading_index() {
            return Err(Refusal::new(Reason::Loading, search::LOADING));
        }
        let query = engine.parse_query(call.query()).map_err(Refusal::bad_query)?;
        let index = engine.shared.index.read();
        answer(&index, &engine.shared.categories, query.as_ref(), &call, true)
    }
}
