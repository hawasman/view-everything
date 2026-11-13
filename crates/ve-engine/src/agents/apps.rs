//! The Apps Module's tools for agents: `apps.programs`, the installed programs with their
//! On-disk Size, and `apps.startup`, the Startup items. They read what the running app's Apps
//! page lists, so they answer only while the app runs. A program none of whose folders is in the
//! Index has no size (`null`), never 0.

use super::{Page, Reason, Refusal, cursor_schema, largest_first, limit_schema};
use crate::Engine;
use crate::apps::{App, StartupItem};
use crate::socket::search;
use serde::Deserialize;
use serde_json::{Value, json};

/// One of the Apps tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Programs,
    Startup,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Programs, Tool::Startup];

    /// The name an agent calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Programs => "apps.programs",
            Tool::Startup => "apps.startup",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    /// What it answers, for the agent.
    pub fn description(self) -> &'static str {
        match self {
            Tool::Programs => {
                "The installed programs (system packages, Flatpaks, Steam games, folders under /opt), largest first by \
                 On-disk Size: the size of their folders in the Index, null when none is indexed. Optionally only those \
                 whose name contains a text. Answers one page of rows and a cursor. Read-only."
            }
            Tool::Startup => {
                "The Startup items, what starts when the user logs in: XDG autostart entries and enabled systemd user units, each with what \
                 it runs and the file that defines it. Answers one page of rows and a cursor. Read-only."
            }
        }
    }

    /// The JSON Schema of its arguments.
    pub fn input_schema(self) -> Value {
        let properties = match self {
            Tool::Programs => json!({
                "name": { "type": "string", "description": "Only programs whose name contains this text, in any letter case." },
                "limit": limit_schema(),
                "cursor": cursor_schema(),
            }),
            Tool::Startup => json!({ "limit": limit_schema(), "cursor": cursor_schema() }),
        };
        json!({ "type": "object", "properties": properties, "additionalProperties": false })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProgramsArgs {
    name: Option<String>,
    limit: Option<u64>,
    cursor: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StartupArgs {
    limit: Option<u64>,
    cursor: Option<String>,
}

/// Answers the Apps tool `name` from `engine`'s Apps, or refuses it: a bad call, or the Index
/// (which programs are sized from) still loading.
pub(super) fn answer(engine: &Engine, name: &str, arguments: Value) -> Result<Value, Refusal> {
    match Tool::from_name(name).ok_or_else(|| Refusal::unknown_tool(name))? {
        Tool::Programs => {
            let args: ProgramsArgs = super::arguments(name, arguments)?;
            let page = Page::parse(name, args.limit, args.cursor.as_deref())?;
            // Sizes come from the Index: none is known until it is read.
            if engine.is_loading_index() {
                return Err(Refusal::new(Reason::Loading, search::LOADING));
            }
            let apps = engine.apps_list();
            let wanted = args.name.map(|n| n.to_lowercase());
            let mut rows: Vec<&App> = apps
                .iter()
                .filter(|a| wanted.as_ref().is_none_or(|w| a.program.name.to_lowercase().contains(w)))
                .collect();
            rows.sort_by(|a, b| largest_first(a.size, b.size).then_with(|| a.program.name.cmp(&b.program.name)));
            let sizes = rows.iter().filter_map(|a| a.size);
            let (sized, on_disk) = sizes.fold((0usize, 0u64), |(n, total), size| (n + 1, total + size));
            Ok(page.answer(rows.into_iter(), program, json!({ "sized": sized, "on_disk_size": on_disk })))
        }
        Tool::Startup => {
            let args: StartupArgs = super::arguments(name, arguments)?;
            let page = Page::parse(name, args.limit, args.cursor.as_deref())?;
            Ok(page.answer(engine.apps_startup().into_iter(), |item| startup(&item), json!({})))
        }
    }
}

/// One installed program as a row.
fn program(app: &App) -> Value {
    let p = &app.program;
    json!({
        "name": &p.name,
        "kind": p.kind.name(),
        "source": p.source,
        "id": &p.id,
        "version": &p.version,
        "installed": p.installed,
        "on_disk_size": app.size,
        "roots": app.roots.iter().map(|(path, size)| json!({ "path": path, "on_disk_size": size })).collect::<Vec<_>>(),
    })
}

/// One startup item as a row.
fn startup(item: &StartupItem) -> Value {
    json!({
        "name": &item.name,
        "kind": item.kind.name(),
        "id": &item.id,
        "command": &item.command,
        "source": &item.source,
        "scope": item.scope.name(),
        "only_in": &item.only_in,
        "not_in": &item.not_in,
        "drop_ins": &item.drop_ins,
    })
}
