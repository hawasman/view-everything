//! The Network Module's tools for agents: `network.interfaces`, what each interface moved over a
//! stretch of the last hour, `network.connections`, the open connections at a Moment, and
//! `network.traffic`, what each Process moved over TCP at a Moment or over a stretch. Like the
//! Performance tools they read the running app's recorder, so they answer only while the app
//! runs; a reading that was not taken is `null`, never zero.

use super::{Page, Reason, Refusal, at_schema, cursor_schema, limit_schema, range_schema};
use crate::net::{Connection, Holder, InterfaceSummary, Net, ProcessTraffic};
use crate::perf::Reading;
use serde::Deserialize;
use serde_json::{Value, json};

/// One of the Network tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Interfaces,
    Connections,
    Traffic,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Interfaces, Tool::Connections, Tool::Traffic];

    /// The name an agent calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Interfaces => "network.interfaces",
            Tool::Connections => "network.connections",
            Tool::Traffic => "network.traffic",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    /// What it answers, for the agent.
    pub fn description(self) -> &'static str {
        match self {
            Tool::Interfaces => {
                "Each network interface seen from one second to another of the last hour (by default the whole hour): \
                 the bytes it received and sent then and the last second it was there, busiest first; and how many \
                 distinct connections those seconds kept (each keeps the top 200). Answers one page of rows and a cursor. \
                 Read-only."
            }
            Tool::Connections => {
                "The open TCP and UDP Connections, listening ones included, at a second of the last hour, or Live: \
                 protocol, local and remote address, state and the Process holding each, open conversations first. A past \
                 second keeps the top 200. Answers one page of rows and a cursor. Read-only."
            }
            Tool::Traffic => {
                "TCP traffic per Process, the most first: with `at` (or nothing), what each Process received and sent at \
                 that second of the last hour, or Live, in bytes per second; with `from` and/or `to`, the bytes each moved \
                 over that stretch. Only the user's own Processes and TCP are read (UDP and other users' need administrator \
                 rights); a past second keeps the top 200. Answers one page of rows and a cursor. Read-only."
            }
        }
    }

    /// The JSON Schema of its arguments.
    pub fn input_schema(self) -> Value {
        let properties = match self {
            Tool::Interfaces => {
                let (from, to) = range_schema();
                json!({ "from": from, "to": to, "limit": limit_schema(), "cursor": cursor_schema() })
            }
            Tool::Connections => json!({ "at": at_schema(), "limit": limit_schema(), "cursor": cursor_schema() }),
            Tool::Traffic => {
                let (from, to) = range_schema();
                json!({ "at": at_schema(), "from": from, "to": to, "limit": limit_schema(), "cursor": cursor_schema() })
            }
        };
        json!({ "type": "object", "properties": properties, "additionalProperties": false })
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct InterfacesArgs {
    from: Option<i64>,
    to: Option<i64>,
    limit: Option<u64>,
    cursor: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConnectionsArgs {
    at: Option<i64>,
    limit: Option<u64>,
    cursor: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TrafficArgs {
    at: Option<i64>,
    from: Option<i64>,
    to: Option<i64>,
    limit: Option<u64>,
    cursor: Option<String>,
}

/// Answers the Network tool `name` from `net`'s recorder at `now` (Unix seconds), or refuses
/// it: a bad call, or nothing recorded at the Moment asked for.
pub(super) fn answer(net: &Net, name: &str, arguments: Value, now: i64) -> Result<Value, Refusal> {
    match Tool::from_name(name).ok_or_else(|| Refusal::unknown_tool(name))? {
        Tool::Interfaces => {
            let args: InterfacesArgs = super::arguments(name, arguments)?;
            let page = Page::parse(name, args.limit, args.cursor.as_deref())?;
            let (from, to) = super::range(name, args.from, args.to, now)?;
            let summary = net.summary(from, to);
            let mut rows = summary.interfaces;
            rows.sort_by(|a, b| b.moved().cmp(&a.moved()).then_with(|| a.name.cmp(&b.name)));
            Ok(page.answer(
                rows.into_iter(),
                |i: InterfaceSummary| json!({ "name": &*i.name, "received": i.received, "sent": i.sent, "last_seen": i.last_seen }),
                json!({ "from": from, "to": to, "seconds": summary.seconds, "connections": summary.connections }),
            ))
        }
        Tool::Connections => {
            let args: ConnectionsArgs = super::arguments(name, arguments)?;
            let page = Page::parse(name, args.limit, args.cursor.as_deref())?;
            let no_sample = |why: String| Refusal::new(Reason::NoSample, why);
            let sample = net.at(super::moment(args.at)).ok_or_else(|| no_sample(nothing_at(args.at)))?;
            let Reading::Value(connections) = sample.connections else {
                return Err(no_sample(format!(
                    "no connections were read at {}: this system's connections cannot be read, or the first reading is not taken yet",
                    sample.time
                )));
            };
            Ok(page.answer(connections.list.iter(), |c| row(c), json!({ "at": args.at, "time": sample.time })))
        }
        Tool::Traffic => {
            let args: TrafficArgs = super::arguments(name, arguments)?;
            let page = Page::parse(name, args.limit, args.cursor.as_deref())?;
            let no_sample = |why: String| Refusal::new(Reason::NoSample, why);
            if args.from.is_none() && args.to.is_none() {
                let sample = net.at(super::moment(args.at)).ok_or_else(|| no_sample(nothing_at(args.at)))?;
                let Reading::Value(traffic) = sample.traffic else {
                    return Err(no_sample(format!(
                        "no traffic per Process was read at {}: this system cannot read it, or it is the recorder's first \
                         reading, which has nothing to count from",
                        sample.time
                    )));
                };
                let rate = |p: &ProcessTraffic| {
                    json!({
                        "pid": p.id.pid, "process": &*p.name,
                        "received_per_second": traffic.receive_rate(p), "sent_per_second": traffic.send_rate(p),
                    })
                };
                return Ok(page.answer(traffic.processes.iter(), rate, json!({ "at": args.at, "time": sample.time })));
            }
            if args.at.is_some() {
                return Err(Refusal::new(
                    Reason::BadArguments,
                    format!("{name} takes `at` (a second) or `from` and `to` (a stretch), not both"),
                ));
            }
            let (from, to) = super::range(name, args.from, args.to, now)?;
            let processes = net.traffic(from, to).ok_or_else(|| {
                no_sample(format!(
                    "no traffic per Process was read from {from} to {to}: the recorder did not run then, or this system \
                     cannot read it"
                ))
            })?;
            Ok(page.answer(
                processes.into_iter(),
                |p: ProcessTraffic| json!({ "pid": p.id.pid, "process": &*p.name, "received": p.received, "sent": p.sent }),
                json!({ "from": from, "to": to }),
            ))
        }
    }
}

/// Why nothing was recorded at the Moment `at` asks for.
fn nothing_at(at: Option<i64>) -> String {
    match at {
        Some(at) => format!("nothing was recorded at {at}: the recorder did not run then, or it is more than an hour ago"),
        None => "nothing is recorded yet: the recorder has not run".into(),
    }
}

/// One Connection as a row.
fn row(c: &Connection) -> Value {
    let (holder, pid, process) = match &c.holder {
        Holder::Process { id, name } => ("process", Some(id.pid), Some(&**name)),
        Holder::Nobody => ("nobody", None, None),
        Holder::Unknown => ("unknown", None, None),
    };
    json!({
        "protocol": c.protocol.label(),
        "local": c.local.to_string(),
        "remote": c.has_remote().then(|| c.remote.to_string()),
        "state": c.state.label(c.protocol),
        "holder": holder,
        "pid": pid,
        "process": process,
    })
}
