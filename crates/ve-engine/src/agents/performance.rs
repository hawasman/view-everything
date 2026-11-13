//! The Performance Module's tools for agents: `performance.summary`, what the recorder saw over
//! a stretch of the last hour, and `performance.processes`, the process table at a Moment. They
//! read the running app's recorder (its one-hour Timeline), so they answer only while the app
//! runs; a reading that was not taken is `null`, never zero.

use super::{Agent, Agents, Page, Reason, Refusal, at_schema, cursor_schema, largest_first, limit_schema, range_schema};
use crate::perf::{Perf, Process, Processes};
use serde::Deserialize;
use serde_json::{Value, json};

/// One of the Performance tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Summary,
    Processes,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::Summary, Tool::Processes];

    /// The name an agent calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Summary => "performance.summary",
            Tool::Processes => "performance.processes",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    /// What it answers, for the agent.
    pub fn description(self) -> &'static str {
        match self {
            Tool::Summary => {
                "What the Performance recorder saw from one second to another of the last hour (by default the whole hour): \
                 how many seconds it recorded, the mean CPU busy share and memory use (0 to 1), and the process that took \
                 the most CPU. Unknown readings are null. Read-only."
            }
            Tool::Processes => {
                "The process table at a second of the last hour, or Live: each process with its CPU share (0 to 1), \
                 resident memory and storage I/O, busiest first (or largest by memory). A past second keeps the top 200. \
                 Answers one page of rows and a cursor. Read-only."
            }
        }
    }

    /// The JSON Schema of its arguments.
    pub fn input_schema(self) -> Value {
        let properties = match self {
            Tool::Summary => {
                let (from, to) = range_schema();
                json!({ "from": from, "to": to })
            }
            Tool::Processes => json!({
                "at": at_schema(),
                "sort": { "type": "string", "enum": ["cpu", "memory"], "description": "Rank by CPU share (default) or resident memory." },
                "limit": limit_schema(),
                "cursor": cursor_schema(),
            }),
        };
        json!({ "type": "object", "properties": properties, "additionalProperties": false })
    }
}

/// What processes rank by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Cpu,
    Memory,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SummaryArgs {
    from: Option<i64>,
    to: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProcessesArgs {
    at: Option<i64>,
    sort: Sort,
    limit: Option<u64>,
    cursor: Option<String>,
}

/// Answers the Performance tool `name` on `perf` at `now`.
fn answer(perf: &Perf, name: &str, arguments: Value, now: i64) -> Result<Value, Refusal> {
    match Tool::from_name(name).ok_or_else(|| Refusal::unknown_tool(name))? {
        Tool::Summary => {
            let args: SummaryArgs = super::arguments(name, arguments)?;
            let (from, to) = super::range(name, args.from, args.to, now)?;
            let summary = perf.summary(from, to);
            Ok(json!({
                "from": from,
                "to": to,
                "seconds": summary.seconds,
                "cpu": summary.cpu,
                "memory": summary.memory,
                "top_process": summary.top.map(|top| json!({ "pid": top.id.pid, "name": &*top.name, "cpu": top.cpu })),
            }))
        }
        Tool::Processes => {
            let args: ProcessesArgs = super::arguments(name, arguments)?;
            let page = Page::parse(name, args.limit, args.cursor.as_deref())?;
            let moment = super::moment(args.at);
            let table = perf.processes(moment).ok_or_else(|| {
                Refusal::new(
                    Reason::NoSample,
                    match args.at {
                        Some(at) => format!(
                            "no process table was recorded at {at}: the recorder did not run then, or it is more than an hour ago"
                        ),
                        None => "no process table is recorded yet: the recorder has not run, or this system's processes cannot be read".into(),
                    },
                )
            })?;
            let mut rows: Vec<&Process> = table.list.iter().collect();
            rows.sort_by(|a, b| {
                let by = match args.sort {
                    Sort::Cpu => largest_first(a.cpu.value(), b.cpu.value()),
                    Sort::Memory => largest_first(a.rss.value(), b.rss.value()),
                };
                by.then(a.id.cmp(&b.id))
            });
            Ok(page.answer(
                rows,
                |p| row(&table, p),
                json!({ "at": args.at, "time": table.time, "sort": args.sort_name() }),
            ))
        }
    }
}

impl ProcessesArgs {
    fn sort_name(&self) -> &'static str {
        match self.sort {
            Sort::Cpu => "cpu",
            Sort::Memory => "memory",
        }
    }
}

/// One process of `table` as a row.
fn row(table: &Processes, p: &Process) -> Value {
    let io = p.io.value();
    json!({
        "pid": p.id.pid,
        "ppid": p.ppid,
        "name": &*p.name,
        "cpu": p.cpu.value(),
        "memory": p.rss.value(),
        "io_read": io.map(|io| io.read),
        "io_write": io.map(|io| io.write),
        "started": table.started(p),
        "cpu_seconds": table.cpu_seconds(p),
    })
}

impl Agents<'_> {
    /// Answers `agent`'s call of the Performance tool `tool` at `now` (Unix seconds) from the
    /// recorder, or refuses it: no Permission yet (with the request's code, before anything else
    /// is looked at), a bad call, or nothing recorded at the Moment asked for.
    pub fn performance(&self, agent: &Agent, tool: &str, arguments: Value, now: i64) -> Result<Value, Refusal> {
        self.check(agent, super::PERFORMANCE, now)?;
        answer(&self.engine.perf, tool, arguments, now)
    }
}
