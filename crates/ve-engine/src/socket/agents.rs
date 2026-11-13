//! The socket's agent methods: `agents.pending` (result: the pending [`Request`]s),
//! `agents.approve` (`{"code"}`; result: `{"agent", "module", "kind", "granted_at"}`), one
//! method per [`Module`] agents read, named for it (`files`, `performance`, `network`, `apps`),
//! and `propose` for the Proposal tools: `{"agent": {"name", "key"}, "tool", "arguments"}`;
//! result: `{"answer"}` or `{"refusal"}`, see [`crate::agents`].
//!
//! [`Request`]: crate::agents::Request

use super::send;
use crate::Engine;
use crate::agents::{Agent, Module};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Write};

#[derive(Deserialize)]
struct Approve {
    code: String,
}

#[derive(Deserialize)]
struct ToolCall {
    agent: Agent,
    tool: String,
    #[serde(default)]
    arguments: Value,
}

/// Whether `method` is one of the agent methods.
pub(super) fn answers(method: &str) -> bool {
    matches!(method, "agents.pending" | "agents.approve" | "propose") || Module::from_method(method).is_some()
}

/// Answers one of the agent methods.
pub(super) fn answer(out: &mut impl Write, id: &Value, method: &str, params: Value, engine: &Engine) -> io::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let tool_call = |params| serde_json::from_value::<ToolCall>(params).map_err(|e| format!("bad {method} call: {e}"));
    let answered = |result: Result<Value, crate::agents::Refusal>| match result {
        Ok(answer) => json!({ "answer": answer }),
        Err(refusal) => json!({ "refusal": refusal }),
    };
    let reply = match method {
        "agents.pending" => Ok(json!(engine.agents().pending(now))),
        "agents.approve" => serde_json::from_value::<Approve>(params)
            .map_err(|e| format!("bad agents.approve: {e}"))
            .and_then(|Approve { code }| engine.agents().approve(&code, now))
            .map(|p| json!({ "agent": p.subject, "module": p.module, "kind": p.kind, "granted_at": p.granted_at })),
        "propose" => tool_call(params)
            .map(|ToolCall { agent, tool, arguments }| answered(engine.agents().propose(&agent, &tool, arguments, now))),
        method => match Module::from_method(method) {
            Some(module) => tool_call(params).map(|ToolCall { agent, tool, arguments }| {
                answered(engine.agents().read(module, &agent, &tool, arguments, now))
            }),
            None => Err(format!("unknown method `{method}`")),
        },
    };
    match reply {
        Ok(result) => send(out, &json!({ "id": id, "result": result })),
        Err(error) => send(out, &json!({ "id": id, "error": error })),
    }
}
