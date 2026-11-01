//! Agents: MCP clients of `ve mcp serve` asking to read a Module. An agent is its client name
//! plus a key `ve mcp serve` issued it on first contact; it reads a Module only under an Agent
//! access [`Permission`] carrying that name, Module and key. Anything else is refused with an
//! approval request: a 4-character code, held here (never saved) for ten minutes, that the user
//! approves in Settings → Permissions or with `ve mcp --approve CODE`. The host is told of each
//! new request so the tray can say who asked for what.

pub mod files;

use crate::Engine;
use crate::permissions::{Kind, Permission};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;

/// The one Module agents can read in this version.
pub const FILES: &str = "Files";

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

/// 64 bits from the operating system's randomness, which seeds every [`std::hash::RandomState`].
fn random() -> u64 {
    let mut hasher = std::hash::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.finish()
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
}
