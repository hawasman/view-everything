//! The tools agents make and follow Proposals with: `files.propose_trash` submits a list of
//! Entries to move to the Trash, `files.proposal` reads where one stands. Both need the Agent
//! proposals Permission on Files, which is approved apart from reading it. Neither changes a
//! file: only a person applies a Proposal (see [`crate::proposals`]).

use super::{Agent, Module, Reason, Refusal, ToolInfo};
use crate::Engine;
use crate::permissions::Kind;
use crate::proposals::{LAPSE, MAX_ROWS, Proposal};
use serde::Deserialize;
use serde_json::{Value, json};

/// One of the Proposal tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    ProposeTrash,
    Proposal,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::ProposeTrash, Tool::Proposal];

    /// The name an agent calls it by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::ProposeTrash => "files.propose_trash",
            Tool::Proposal => "files.proposal",
        }
    }

    pub fn from_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    /// The Module it proposes in.
    pub fn module(self) -> Module {
        Module::Files
    }

    /// What it does, for the agent.
    pub fn description(self) -> &'static str {
        match self {
            Tool::ProposeTrash => {
                "Proposes moving Entries (files or folders on the user's local Volumes, by absolute path) to the Trash, \
                 with a reason the user reads. It changes nothing: the user reviews the Proposal and applies or dismisses \
                 it, and unanswered it lapses after 24 hours. A folder takes everything in it. Answers the Proposal's id; \
                 files.proposal says where it stands. Proposing needs its own approval, apart from reading Files."
            }
            Tool::Proposal => {
                "Where a Proposal this agent made stands: pending, applied, dismissed or lapsed. Once applied, what moved \
                 to the Trash, and which rows were dropped because they changed or vanished since they were proposed. \
                 Read-only."
            }
        }
    }

    /// The JSON Schema of its arguments.
    pub fn input_schema(self) -> Value {
        match self {
            Tool::ProposeTrash => json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": MAX_ROWS,
                        "description": format!("Paths of Entries on local Volumes, as files.find answers them (at most {MAX_ROWS}).")
                    },
                    "reason": { "type": "string", "description": "Why, in a sentence the user reads before deciding." }
                },
                "required": ["paths", "reason"],
                "additionalProperties": false
            }),
            Tool::Proposal => json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "minimum": 1, "description": "The id files.propose_trash answered." }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }
}

/// Every Proposal tool, as `tools/list` describes it.
pub fn tools() -> Vec<ToolInfo> {
    Tool::ALL
        .into_iter()
        .map(|t| ToolInfo {
            module: t.module(),
            permission: Kind::AgentProposals,
            read_only: t == Tool::Proposal,
            name: t.name(),
            description: t.description(),
            input_schema: t.input_schema(),
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Propose {
    paths: Vec<String>,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ask {
    id: u64,
}

/// The arguments `arguments` of `tool`, which has required ones, or why not.
fn arguments<T: serde::de::DeserializeOwned>(tool: Tool, arguments: Value) -> Result<T, Refusal> {
    serde_json::from_value(arguments)
        .map_err(|e| Refusal::new(Reason::BadArguments, format!("bad arguments for {}: {e}", tool.name())))
}

/// `proposal` as the agent reads it.
fn status(proposal: &Proposal) -> Value {
    let mut answer = json!({
        "id": proposal.id,
        "change": proposal.change,
        "status": proposal.status.name(),
        "reason": proposal.reason,
        "submitted_at": proposal.submitted_at,
        "lapses_at": proposal.lapses_at,
        "answered_at": proposal.answered_at,
        "rows": proposal.rows.len(),
        "on_disk_size": proposal.on_disk_size(),
    });
    if let Some(outcome) = &proposal.outcome {
        answer["outcome"] = json!(outcome);
    }
    answer
}

/// Answers `agent`'s call of `tool` at `now`; the agent's Permission was checked.
pub(super) fn answer(engine: &Engine, agent: &Agent, tool: Tool, args: Value, now: i64) -> Result<Value, Refusal> {
    match tool {
        Tool::ProposeTrash => {
            let Propose { paths, reason } = arguments(tool, args)?;
            let proposal = engine.proposals().submit_trash(agent, &paths, &reason, now)?;
            let mut answer = status(&proposal);
            answer["message"] = json!(format!(
                "Proposal {id} is waiting for the user to apply it (`ve proposals --apply {id}`) or dismiss it; nothing has \
                 moved. Unanswered, it lapses in {} hours. Ask files.proposal for its status.",
                LAPSE / 3600,
                id = proposal.id,
            ));
            Ok(answer)
        }
        Tool::Proposal => {
            let Ask { id } = arguments(tool, args)?;
            engine.proposals().of_agent(agent, id, now).map(|p| status(&p))
        }
    }
}
