//! Permissions (CONTEXT.md): the approvals the user granted, one list kept in the settings.
//! Granting happens where the user is asked (the app's Settings, or an agent's approval code,
//! see [`crate::agents`]); the Live updates helper is asked for once granted, and a refused
//! authentication forgets it again. Revoking can come from any client of the handle, so a revoke
//! undoes the Permission fully here: it stops the Live updates helper, has the host remove the
//! login entry, or refuses the agent from its next request on.

use crate::Engine;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// What a Permission allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Linux: the `fanotify` helper, run as root through pkexec while the app runs (ADR-0009).
    LiveHelper,
    /// The app starts when the user logs in (ADR-0012).
    StartAtLogin,
    /// An agent (an MCP client of `ve mcp serve`) may read one Module.
    Agent,
}

impl Kind {
    /// The subject a Permission of this kind is granted on. An agent's Permission names the
    /// agent instead, so its kind has only the generic word.
    pub fn subject(self) -> &'static str {
        match self {
            Kind::LiveHelper => "ve-fanotify",
            Kind::StartAtLogin => "view-everything",
            Kind::Agent => "agent",
        }
    }

    /// The name Settings shows.
    pub fn label(self) -> &'static str {
        match self {
            Kind::LiveHelper => "Live updates helper",
            Kind::StartAtLogin => "Start at login",
            Kind::Agent => "Agent access",
        }
    }

    /// What revoking undoes, as Settings says it.
    pub fn revoke_note(self) -> &'static str {
        match self {
            Kind::LiveHelper => "Revoking stops the helper; it is not started again until granted again",
            Kind::StartAtLogin => "Revoking removes the login entry",
            Kind::Agent => "Revoking refuses the agent's next request; it has to be approved again",
        }
    }
}

/// One approval: what it allows, on what, and since when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    pub kind: Kind,
    /// What it is granted on: [`Kind::subject`], or an agent's name.
    pub subject: String,
    /// When it was granted, in Unix seconds; `None` for one granted before this version, which
    /// kept no time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<i64>,
    /// The Module an agent may read (`Files`); `None` for the other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// The key `ve mcp serve` issued the agent on first contact: a client presenting another key
    /// under the same name is not this agent. `None` for the other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl Permission {
    /// A Permission of one of the kinds with a fixed subject, granted at `granted_at`.
    pub fn of(kind: Kind, granted_at: Option<i64>) -> Permission {
        Permission {
            kind,
            subject: kind.subject().to_owned(),
            granted_at,
            module: None,
            key: None,
        }
    }
}

/// Removes the login entry, or says why it could not.
pub type RemoveLoginEntry = Box<dyn Fn() -> Result<(), String> + Send + Sync>;
/// Stores the list of Permissions, or says why it could not.
pub type Save = Box<dyn Fn(&[Permission]) -> Result<(), String> + Send + Sync>;

/// What the host does for a revoke the engine cannot finish alone.
pub struct Hooks {
    /// Removes the login entry (only the host knows what it starts). May block: Windows waits
    /// for the elevation prompt.
    pub remove_login_entry: RemoveLoginEntry,
    /// Stores the list after it changed.
    pub save: Save,
}

/// The engine's list of Permissions and the host's hooks.
#[derive(Default)]
pub struct Store {
    list: Mutex<Vec<Permission>>,
    hooks: Mutex<Option<Arc<Hooks>>>,
    saving: Mutex<()>,
}

impl Store {
    /// Forgets the Permission of `kind` without undoing anything: what it allowed was refused
    /// where it was asked for (the helper's authentication prompt).
    #[cfg(target_os = "linux")]
    pub(crate) fn forget(&self, kind: Kind) -> Result<(), String> {
        self.list.lock().retain(|p| p.kind != kind);
        self.save()
    }

    /// Stores the list as it is now. Saves run one at a time, each reading the list once it
    /// runs, so a slower save never stores an older list over a newer one.
    fn save(&self) -> Result<(), String> {
        let Some(hooks) = self.hooks.lock().clone() else {
            return Ok(());
        };
        let _saving = self.saving.lock();
        let list = self.list.lock().clone();
        (hooks.save)(&list).map_err(|e| format!("Could not save the Permissions: {e}"))
    }

    /// Records an agent's Permission, replacing the one the same agent name held on the same
    /// Module (an older key), and stores the list.
    pub(crate) fn grant_agent(&self, permission: Permission) -> Result<(), String> {
        {
            let mut list = self.list.lock();
            list.retain(|p| !(p.kind == Kind::Agent && p.subject == permission.subject && p.module == permission.module));
            list.push(permission);
        }
        self.save()
    }

    /// Forgets the agent `name`'s Permission on `module`, or says there is none.
    pub(crate) fn revoke_agent(&self, name: &str, module: &str) -> Result<(), String> {
        {
            let mut list = self.list.lock();
            let before = list.len();
            list.retain(|p| !(p.kind == Kind::Agent && p.subject == name && p.module.as_deref() == Some(module)));
            if list.len() == before {
                return Err(format!("No Agent access Permission for {name} on {module}"));
            }
        }
        self.save()
    }
}

/// The Permissions through the [`Engine`] handle (`permissions.list`, `permissions.revoke`).
pub struct Permissions<'a> {
    engine: &'a Engine,
}

impl Engine {
    /// The Permissions the user granted, and their grant and revoke.
    pub fn permissions(&self) -> Permissions<'_> {
        Permissions { engine: self }
    }
}

impl Permissions<'_> {
    fn store(&self) -> &Store {
        &self.engine.shared.permissions
    }

    /// Takes the list the settings hold, as the host loaded it.
    pub fn load(&self, list: Vec<Permission>) {
        *self.store().list.lock() = list;
    }

    /// Gives the engine what the host does for it: remove the login entry, store the list.
    pub fn set_hooks(&self, hooks: Hooks) {
        *self.store().hooks.lock() = Some(Arc::new(hooks));
    }

    /// Every Permission, in the order granted.
    pub fn list(&self) -> Vec<Permission> {
        self.store().list.lock().clone()
    }

    /// Whether a Permission of `kind` is granted.
    pub fn has(&self, kind: Kind) -> bool {
        self.store().list.lock().iter().any(|p| p.kind == kind)
    }

    /// Records that the user granted `kind` at `at` (Unix seconds). Granting it again keeps the
    /// first grant's time. An agent is granted by approving its code instead
    /// ([`crate::agents::Agents::approve`]).
    pub fn grant(&self, kind: Kind, at: i64) -> Result<(), String> {
        if kind == Kind::Agent {
            return Err("An agent is granted access by approving its code".into());
        }
        {
            let mut list = self.store().list.lock();
            if list.iter().any(|p| p.kind == kind && p.subject == kind.subject()) {
                return Ok(());
            }
            list.push(Permission::of(kind, Some(at)));
        }
        self.store().save()
    }

    /// Undoes the Permission of `kind` on `subject` and forgets it (for an agent: its access to
    /// every Module). When the undoing fails the Permission stays, and the error says why; when
    /// only storing the shorter list fails, the Permission is revoked and the error says the
    /// list was not saved.
    pub fn revoke(&self, kind: Kind, subject: &str) -> Result<(), String> {
        let held = |p: &Permission| p.kind == kind && p.subject == subject;
        if !self.store().list.lock().iter().any(held) {
            return Err(format!("No {} Permission on {subject}", kind.label()));
        }
        let hooks = self.store().hooks.lock().clone();
        match kind {
            Kind::LiveHelper => self.engine.set_live_helper(false),
            Kind::StartAtLogin => {
                let hooks = hooks.ok_or("Nothing here can remove the login entry")?;
                (hooks.remove_login_entry)()?;
            }
            // Every request checks the list, so the agent's next one is refused.
            Kind::Agent => {}
        }
        self.store().list.lock().retain(|p| !held(p));
        self.store().save()
    }
}
