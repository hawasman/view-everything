//! The socket's agent methods: `agents.pending` (result: the pending [`Request`]s),
//! `agents.approve` (`{"code"}`; result: `{"agent", "module", "granted_at"}`), and `files`
//! (`{"agent": {"name", "key"}, "tool", "arguments"}`; result: `{"answer"}` or `{"refusal"}`,
//! see [`crate::agents::files`]).

use super::send;
use crate::Engine;
use crate::agents::Agent;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Write};

#[derive(Deserialize)]
struct Approve {
    code: String,
}

#[derive(Deserialize)]
struct FilesCall {
    agent: Agent,
    tool: String,
    #[serde(default)]
    arguments: Value,
}

/// Answers one of the agent methods.
pub(super) fn answer(out: &mut impl Write, id: &Value, method: &str, params: Value, engine: &Engine) -> io::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let reply = match method {
        "agents.pending" => Ok(json!(engine.agents().pending(now))),
        "agents.approve" => serde_json::from_value::<Approve>(params)
            .map_err(|e| format!("bad agents.approve: {e}"))
            .and_then(|Approve { code }| engine.agents().approve(&code, now))
            .map(|p| json!({ "agent": p.subject, "module": p.module, "granted_at": p.granted_at })),
        "files" => serde_json::from_value::<FilesCall>(params)
            .map_err(|e| format!("bad files call: {e}"))
            .map(
                |FilesCall { agent, tool, arguments }| match engine.agents().files(&agent, &tool, arguments, now) {
                    Ok(answer) => json!({ "answer": answer }),
                    Err(refusal) => json!({ "refusal": refusal }),
                },
            ),
        other => Err(format!("unknown method `{other}`")),
    };
    match reply {
        Ok(result) => send(out, &json!({ "id": id, "result": result })),
        Err(error) => send(out, &json!({ "id": id, "error": error })),
    }
}
