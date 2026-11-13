//! The socket's Proposal methods, the user's side of [`crate::proposals`]: `proposals.list`
//! (result: every kept [`Proposal`], newest first), `proposals.check` (`{"id"}`; result: a
//! [`Check`]), `proposals.apply` (`{"id", "paths"}`, the paths the person confirmed; result:
//! the applied [`Proposal`], once its Trash Task is done) and `proposals.dismiss` (`{"id"}`;
//! result: the dismissed [`Proposal`]).
//!
//! [`Proposal`]: crate::proposals::Proposal
//! [`Check`]: crate::proposals::Check

use super::send;
use crate::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Id {
    id: u64,
}

#[derive(Deserialize)]
struct Apply {
    id: u64,
    paths: Vec<PathBuf>,
}

/// Whether `method` is one of the Proposal methods.
pub(super) fn answers(method: &str) -> bool {
    matches!(
        method,
        "proposals.list" | "proposals.check" | "proposals.apply" | "proposals.dismiss"
    )
}

/// Answers one of the Proposal methods.
pub(super) fn answer(out: &mut impl Write, id: &Value, method: &str, params: Value, engine: &Engine) -> io::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let proposals = engine.proposals();
    let bad = |e: serde_json::Error| format!("bad {method}: {e}");
    let reply = match method {
        "proposals.list" => Ok(json!(proposals.list(now))),
        "proposals.check" => serde_json::from_value::<Id>(params)
            .map_err(bad)
            .and_then(|Id { id }| proposals.check(id, now))
            .map(|check| json!(check)),
        "proposals.apply" => serde_json::from_value::<Apply>(params)
            .map_err(bad)
            .and_then(|Apply { id, paths }| proposals.apply(id, &paths, now))
            .map(|p| json!(p)),
        "proposals.dismiss" => serde_json::from_value::<Id>(params)
            .map_err(bad)
            .and_then(|Id { id }| proposals.dismiss(id, now))
            .map(|p| json!(p)),
        method => Err(format!("unknown method `{method}`")),
    };
    match reply {
        Ok(result) => send(out, &json!({ "id": id, "result": result })),
        Err(error) => send(out, &json!({ "id": id, "error": error })),
    }
}
