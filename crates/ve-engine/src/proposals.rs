//! Proposals (CONTEXT.md): lists an agent submits for the user to apply or dismiss. A Proposal
//! changes nothing by itself. Today one kind exists: a list of Entries on local Volumes to
//! Trash, with the agent's reason.
//!
//! Each row keeps what the Index said of its Entry when it was proposed (folder or file, On-disk
//! Size of everything in it, modification time). Applying checks every row against the Index
//! again and drops the ones that changed or vanished since, so nothing moves that the user did
//! not see as it is; only the rows the person confirmed move, by the Trash Task. A Proposal left
//! unanswered for [`LAPSE`] lapses. The answer stays readable (the agent asks its status) for
//! [`KEEP`] after it.
//!
//! The Proposals are kept beside the Index (`proposals.json`, readable by the user alone), so a
//! restart of the app neither forgets one nor resets its clock.

use crate::agents::{Agent, Reason, Refusal};
use crate::{Engine, volume_blocker};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use ve_core::{EntryId, Index};

/// How long a Proposal waits for the user, in seconds, before it lapses.
pub const LAPSE: i64 = 24 * 60 * 60;
/// How long an answered or lapsed Proposal is kept for its agent to read, in seconds.
pub const KEEP: i64 = 7 * 24 * 60 * 60;
/// The most rows one Proposal holds.
pub const MAX_ROWS: usize = crate::agents::MAX_LIMIT;
/// The most Proposals one agent may have waiting for the user at once.
pub const MAX_PENDING: usize = 20;
/// The longest reason kept, in characters.
pub const MAX_REASON: usize = 500;

/// Where a Proposal stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting for the user.
    Pending,
    /// The user applied it: [`Proposal::outcome`] says what moved.
    Applied,
    /// The user dismissed it: nothing moved.
    Dismissed,
    /// Unanswered for [`LAPSE`]: nothing moved.
    Lapsed,
}

impl Status {
    /// "pending", "applied", ...: as the agent and `ve proposals` read it.
    pub fn name(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Applied => "applied",
            Status::Dismissed => "dismissed",
            Status::Lapsed => "lapsed",
        }
    }
}

/// What applying a Proposal does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Moves its rows to the Trash.
    Trash,
}

/// One Entry of a Proposal, as the Index had it when proposed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub path: PathBuf,
    pub is_dir: bool,
    /// The On-disk Size of the Entry and everything in it.
    pub on_disk_size: u64,
    /// Its modification time, in the Index's nanoseconds.
    pub modified: i64,
}

/// Why a row was left out when its Proposal was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// Its Entry is no longer in the Index, or no longer on the disk.
    Vanished,
    /// Its Entry is a folder where it was a file (or the other way round), or its size or
    /// modification time changed.
    Changed,
    /// Its Volume is offline now.
    Offline,
}

impl Why {
    /// How `ve proposals` says it.
    pub fn text(self) -> &'static str {
        match self {
            Why::Vanished => "gone since it was proposed",
            Why::Changed => "changed since it was proposed",
            Why::Offline => "its Volume is offline",
        }
    }
}

/// A row applying left out, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dropped {
    pub path: PathBuf,
    pub why: Why,
}

/// A row the Trash could not move, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failed {
    pub path: PathBuf,
    pub error: String,
}

/// What applying a Proposal did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// Moved to the Trash.
    pub trashed: Vec<PathBuf>,
    /// Changed or vanished since proposed: not moved.
    pub dropped: Vec<Dropped>,
    /// Still where they were: the Trash could not move them.
    pub failed: Vec<Failed>,
    /// The person did not confirm them: not moved.
    pub left_out: Vec<PathBuf>,
}

/// A list an agent submitted for the user to apply or dismiss.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: u64,
    /// The agent's name.
    pub agent: String,
    pub change: Change,
    /// Why the agent proposes it, in its words.
    pub reason: String,
    /// Unix seconds.
    pub submitted_at: i64,
    /// Unix seconds: unanswered by then, it lapses.
    pub lapses_at: i64,
    pub status: Status,
    /// When it was applied, dismissed or lapsed, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<i64>,
    pub rows: Vec<Row>,
    /// What applying it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
}

impl Proposal {
    /// The On-disk Size of all its rows, as proposed.
    pub fn on_disk_size(&self) -> u64 {
        self.rows.iter().map(|r| r.on_disk_size).sum()
    }
}

/// A pending Proposal checked against the Index now: the rows applying would move, and the
/// ones it would leave out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub proposal: Proposal,
    pub keep: Vec<Row>,
    pub dropped: Vec<Dropped>,
}

/// Called with each new Proposal (the tray's notification).
pub type OnProposal = Box<dyn Fn(&Proposal) + Send + Sync>;

/// A Proposal and the key of the agent that made it: only that agent reads its status.
#[derive(Clone, Serialize, Deserialize)]
struct Held {
    #[serde(flatten)]
    proposal: Proposal,
    key: String,
}

impl Held {
    /// Whether `agent` (name and key) made it.
    fn made_by(&self, agent: &Agent) -> bool {
        self.proposal.agent == agent.name && self.key == agent.key
    }
}

/// What `proposals.json` holds.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// Never reused, so an old id never names a newer Proposal.
    next_id: u64,
    proposals: Vec<Held>,
}

impl Saved {
    /// Lapses the Proposals unanswered for [`LAPSE`] at `now` and forgets the ones answered more
    /// than [`KEEP`] ago. Whether anything changed.
    fn tidy(&mut self, now: i64) -> bool {
        let before = self.proposals.len();
        let mut changed = false;
        for held in &mut self.proposals {
            let p = &mut held.proposal;
            if p.status == Status::Pending && p.lapses_at <= now {
                p.status = Status::Lapsed;
                p.answered_at = Some(p.lapses_at);
                changed = true;
            }
        }
        self.proposals
            .retain(|h| h.proposal.answered_at.is_none_or(|at| at.saturating_add(KEEP) > now));
        changed || self.proposals.len() != before
    }

    fn find(&mut self, id: u64) -> Option<&mut Proposal> {
        self.proposals.iter_mut().map(|h| &mut h.proposal).find(|p| p.id == id)
    }
}

/// The Proposals, where they are kept, and who is told of new ones.
pub struct Store {
    /// `proposals.json` beside the Index; `None` keeps them in memory only.
    path: Option<PathBuf>,
    held: Mutex<Saved>,
    on_proposal: Mutex<Option<Arc<OnProposal>>>,
    /// Held while a Proposal is answered, so two answers never race.
    answering: Mutex<()>,
}

impl Store {
    /// The Proposals kept beside the Index at `index_path`, or none kept anywhere without one.
    /// A file that cannot be read is reported and set aside (`proposals.json.bad`): the
    /// Proposals start empty.
    pub(crate) fn load(index_path: Option<&Path>) -> Store {
        let path = index_path.map(|p| p.with_file_name("proposals.json"));
        let saved = match path.as_deref().map(|p| (p, std::fs::read_to_string(p))) {
            None => Saved::default(),
            Some((_, Err(e))) if e.kind() == io::ErrorKind::NotFound => Saved::default(),
            Some((path, read)) => match read
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
            {
                Ok(saved) => saved,
                Err(e) => {
                    let aside = path.with_extension("json.bad");
                    let moved = std::fs::rename(path, &aside).map_or_else(|e| format!(" (not set aside: {e})"), |()| String::new());
                    eprintln!(
                        "Could not read the Proposals ({e}); starting with none, the file is {}{moved}",
                        aside.display()
                    );
                    Saved::default()
                }
            },
        };
        Store {
            path,
            held: Mutex::new(saved),
            on_proposal: Mutex::new(None),
            answering: Mutex::new(()),
        }
    }

    /// Writes `saved` to `proposals.json`, readable by the user alone, or says why it could not.
    fn save(&self, saved: &Saved) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let write = || -> io::Result<()> {
            let text = serde_json::to_string_pretty(saved).map_err(io::Error::other)?;
            let partial = path.with_extension("json.partial");
            let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&partial)?;
            // Also a partial file left by a crash, whatever it allowed.
            #[cfg(unix)]
            file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
            std::fs::rename(&partial, path)
        };
        write().map_err(|e| format!("Could not save the Proposals to {}: {e}", path.display()))
    }

    /// Runs `f` on the Proposals as of `now` (lapsed ones lapsed), saving when `f` says it
    /// changed them or tidying did; the second value is that save's result, if one was made.
    fn with<T>(&self, now: i64, f: impl FnOnce(&mut Saved) -> (T, bool)) -> (T, Option<Result<(), String>>) {
        let mut saved = self.held.lock();
        let tidied = saved.tidy(now);
        let (result, changed) = f(&mut saved);
        let stored = (tidied || changed).then(|| self.save(&saved));
        (result, stored)
    }
}

/// The problem the app reports while the Proposals cannot be saved.
const UNSAVED: &str = "proposals";

/// The Proposals through the [`Engine`] handle (`proposals.list`, `proposals.check`,
/// `proposals.apply`, `proposals.dismiss`).
pub struct Proposals<'a> {
    engine: &'a Engine,
}

impl Engine {
    /// Agents' Proposals: submitting one, and the user's answer.
    pub fn proposals(&self) -> Proposals<'_> {
        Proposals { engine: self }
    }
}

/// The On-disk Size of `id` and everything in it.
fn subtree_on_disk(index: &Index, id: EntryId) -> u64 {
    let mut total = 0;
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        total += index.on_disk(current);
        stack.extend(index.children(current));
    }
    total
}

/// `id` as a row, as the Index has it now.
fn row(index: &Index, id: EntryId) -> Row {
    Row {
        path: index.path(id),
        is_dir: index.is_dir(id),
        on_disk_size: subtree_on_disk(index, id),
        modified: index.modified(id),
    }
}

/// The Location `id` is on.
fn volume(index: &Index, id: EntryId) -> Option<&ve_core::Volume> {
    index.volume(index.volume_of(id)?)
}

/// Why `row` cannot be applied as proposed, if so.
fn recheck(index: &Index, row: &Row) -> Option<Why> {
    let Some(id) = index.find_path(&row.path) else {
        return Some(Why::Vanished);
    };
    if volume(index, id).is_none_or(|v| !v.online) {
        return Some(Why::Offline);
    }
    (self::row(index, id) != *row).then_some(Why::Changed)
}

/// A reason as kept: one line without control characters, at most [`MAX_REASON`] characters.
fn clean_reason(reason: &str) -> String {
    let line: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_REASON)
        .collect();
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first few of `paths`, for a message.
fn some(paths: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut text = paths.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > SHOWN {
        text.push_str(&format!(" and {} more", paths.len() - SHOWN));
    }
    text
}

/// Why a Proposal that is not pending cannot be answered.
fn answered(p: &Proposal) -> String {
    match p.status {
        Status::Pending => format!("Proposal {} is pending", p.id),
        Status::Applied => format!("Proposal {} was already applied", p.id),
        Status::Dismissed => format!("Proposal {} was dismissed", p.id),
        Status::Lapsed => format!("Proposal {} lapsed: it was left unanswered for {} hours", p.id, LAPSE / 3600),
    }
}

impl Proposals<'_> {
    fn store(&self) -> &Store {
        &self.engine.shared.proposals
    }

    /// [`Store::with`], with a failed save reported as the app's problem (and the report
    /// dropped once a save works again): what was answered stays answered, but would not
    /// survive a restart.
    fn with<T>(&self, now: i64, f: impl FnOnce(&mut Saved) -> (T, bool)) -> T {
        let (result, stored) = self.store().with(now, f);
        match stored {
            None => {}
            Some(Ok(())) => self.engine.shared.resolve(UNSAVED),
            Some(Err(e)) => {
                eprintln!("{e}");
                self.engine.shared.report(UNSAVED, e);
            }
        }
        result
    }

    /// Has `on_proposal` called with each new Proposal.
    pub fn set_on_proposal(&self, on_proposal: OnProposal) {
        *self.store().on_proposal.lock() = Some(Arc::new(on_proposal));
    }

    /// Submits `agent`'s Proposal to move `paths` to the Trash, for `reason`, at `now`. Every
    /// path must be an Entry of an online local Volume (not a whole Volume); a folder takes
    /// everything in it, so a path inside another one proposed is left out. Refused, nothing
    /// is submitted.
    pub fn submit_trash(&self, agent: &Agent, paths: &[String], reason: &str, now: i64) -> Result<Proposal, Refusal> {
        let bad = |why: String| Refusal::new(Reason::BadArguments, why);
        let reason = clean_reason(reason);
        if reason.is_empty() {
            return Err(bad("a Proposal needs a reason the user can read".into()));
        }
        if paths.is_empty() {
            return Err(bad("a Proposal needs at least one path".into()));
        }
        if paths.len() > MAX_ROWS {
            return Err(bad(format!(
                "a Proposal holds at most {MAX_ROWS} paths; propose the rest in another one"
            )));
        }
        if self.engine.is_loading_index() {
            return Err(Refusal::new(Reason::Loading, crate::socket::search::LOADING));
        }
        let mounted = ve_scan::discover_volumes();
        let rows = {
            let index = self.engine.shared.index.read();
            let (mut remote, mut offline, mut unknown, mut volumes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut read_only = Vec::new();
            let mut ids = Vec::new();
            for text in paths {
                // Not only absolute paths: a Remote's Entries read `ssh:user@host:/path/...`.
                let Some(id) = index.find_path(Path::new(text)) else {
                    unknown.push(text.clone());
                    continue;
                };
                match volume(&index, id) {
                    Some(v) if v.is_remote() => remote.push(text.clone()),
                    Some(v) if !v.online => offline.push(text.clone()),
                    _ if index.parent(id).is_none() => volumes.push(text.clone()),
                    // Neither Remote nor offline: what blocks it is a read-only mount.
                    Some(v) if volume_blocker(v, &mounted).is_some() => read_only.push(text.clone()),
                    _ => ids.push(id),
                }
            }
            if !remote.is_empty() {
                return Err(Refusal::new(
                    Reason::Remote,
                    format!(
                        "{} is on a Remote, which is read-only and has no Trash; propose only Entries on local Volumes",
                        some(&remote)
                    ),
                )
                .with_paths(remote));
            }
            if !offline.is_empty() {
                return Err(Refusal::new(
                    Reason::Offline,
                    format!(
                        "{} is on an Offline Volume, which cannot be changed until it is mounted again",
                        some(&offline)
                    ),
                )
                .with_paths(offline));
            }
            if !unknown.is_empty() {
                return Err(Refusal::new(
                    Reason::BadPath,
                    format!("{} is not a path in the Index; files.find finds Entries by name", some(&unknown)),
                )
                .with_paths(unknown));
            }
            if !volumes.is_empty() {
                return Err(Refusal::new(
                    Reason::BadPath,
                    format!("{} is a whole Volume, which cannot be moved to the Trash", some(&volumes)),
                )
                .with_paths(volumes));
            }
            if !read_only.is_empty() {
                return Err(Refusal::new(
                    Reason::BadPath,
                    format!("{} is on a read-only mount, so it cannot be moved to the Trash", some(&read_only)),
                )
                .with_paths(read_only));
            }
            let chosen: HashSet<EntryId> = ids.iter().copied().collect();
            let mut seen = HashSet::new();
            ids.into_iter()
                .filter(|&id| seen.insert(id))
                .filter(|&id| {
                    // A folder proposed takes this one with it.
                    let mut ancestor = index.parent(id);
                    while let Some(a) = ancestor {
                        if chosen.contains(&a) {
                            return false;
                        }
                        ancestor = index.parent(a);
                    }
                    true
                })
                .map(|id| row(&index, id))
                .collect::<Vec<_>>()
        };
        let proposal = self.with(now, |saved| {
            let waiting = saved
                .proposals
                .iter()
                .filter(|h| h.proposal.status == Status::Pending && h.made_by(agent))
                .count();
            if waiting >= MAX_PENDING {
                let refusal = Refusal::new(
                    Reason::TooManyPending,
                    format!(
                        "this agent already has {waiting} Proposals waiting for the user; wait until the user answers one \
                         (files.proposal says when), or it lapses"
                    ),
                );
                return (Err(refusal), false);
            }
            saved.next_id = saved.next_id.max(1);
            let proposal = Proposal {
                id: saved.next_id,
                agent: agent.name.clone(),
                change: Change::Trash,
                reason,
                submitted_at: now,
                lapses_at: now + LAPSE,
                status: Status::Pending,
                answered_at: None,
                rows,
                outcome: None,
            };
            saved.next_id += 1;
            saved.proposals.push(Held {
                proposal: proposal.clone(),
                key: agent.key.clone(),
            });
            (Ok(proposal), true)
        })?;
        let on_proposal = self.store().on_proposal.lock().clone();
        if let Some(on_proposal) = on_proposal {
            on_proposal(&proposal);
        }
        (self.engine.shared.repaint)();
        Ok(proposal)
    }

    /// `agent`'s Proposal `id` as it stands at `now`; another agent's is not found.
    pub fn of_agent(&self, agent: &Agent, id: u64, now: i64) -> Result<Proposal, Refusal> {
        self.with(now, |saved| {
            let found = saved
                .proposals
                .iter()
                .find(|h| h.proposal.id == id && h.made_by(agent))
                .map(|h| h.proposal.clone());
            (found, false)
        })
        .ok_or_else(|| {
            Refusal::new(
                Reason::NoProposal,
                format!(
                    "this agent has no Proposal {id}; an answered Proposal is kept for {} days",
                    KEEP / 86_400
                ),
            )
        })
    }

    /// Every Proposal kept at `now`, newest first.
    pub fn list(&self, now: i64) -> Vec<Proposal> {
        self.with(now, |saved| {
            (saved.proposals.iter().rev().map(|h| h.proposal.clone()).collect(), false)
        })
    }

    /// The pending Proposal `id` at `now`, or why it cannot be answered.
    fn pending(&self, id: u64, now: i64) -> Result<Proposal, String> {
        self.with(now, |saved| {
            let found = match saved.find(id) {
                None => Err(format!("No Proposal {id}")),
                Some(p) if p.status != Status::Pending => Err(answered(p)),
                Some(p) => Ok(p.clone()),
            };
            (found, false)
        })
    }

    /// Checks the pending Proposal `id` against the Index at `now`: the rows applying it would
    /// move, and the ones it would leave out because they changed or vanished.
    pub fn check(&self, id: u64, now: i64) -> Result<Check, String> {
        let proposal = self.pending(id, now)?;
        if self.engine.is_loading_index() {
            return Err(crate::socket::search::LOADING.to_owned());
        }
        let index = self.engine.shared.index.read();
        let (mut keep, mut dropped) = (Vec::new(), Vec::new());
        for row in &proposal.rows {
            match recheck(&index, row) {
                None => keep.push(row.clone()),
                Some(why) => dropped.push(Dropped {
                    path: row.path.clone(),
                    why,
                }),
            }
        }
        drop(index);
        Ok(Check { proposal, keep, dropped })
    }

    /// Applies the pending Proposal `id` at `now`: checks each row against the Index again,
    /// then moves to the Trash, as a Task, the rows still as proposed among `confirmed` (the
    /// paths the person was shown and agreed to). Rows that changed or vanished are dropped,
    /// and the rest left out; the answer says which. Returns the applied Proposal.
    pub fn apply(&self, id: u64, confirmed: &[PathBuf], now: i64) -> Result<Proposal, String> {
        let _answering = self.store().answering.lock();
        let Check { keep, mut dropped, .. } = self.check(id, now)?;
        let (go, left_out): (Vec<Row>, Vec<Row>) = keep.into_iter().partition(|r| confirmed.contains(&r.path));
        let trashed = if go.is_empty() {
            crate::trash::Trashed::default()
        } else {
            self.engine.move_to_trash(go.into_iter().map(|r| r.path).collect())
        };
        dropped.extend(trashed.missing.into_iter().map(|path| Dropped { path, why: Why::Vanished }));
        let outcome = Outcome {
            trashed: trashed.moved,
            dropped,
            failed: trashed.failed.into_iter().map(|(path, error)| Failed { path, error }).collect(),
            left_out: left_out.into_iter().map(|r| r.path).collect(),
        };
        let applied = self.answer(id, now, Status::Applied, Some(outcome));
        (self.engine.shared.repaint)();
        applied
    }

    /// Dismisses the pending Proposal `id` at `now`: nothing moves.
    pub fn dismiss(&self, id: u64, now: i64) -> Result<Proposal, String> {
        let _answering = self.store().answering.lock();
        self.pending(id, now)?;
        let dismissed = self.answer(id, now, Status::Dismissed, None);
        (self.engine.shared.repaint)();
        dismissed
    }

    /// Records the user's answer to Proposal `id`.
    fn answer(&self, id: u64, now: i64, status: Status, outcome: Option<Outcome>) -> Result<Proposal, String> {
        self.with(now, |saved| match saved.find(id) {
            Some(p) => {
                p.status = status;
                p.answered_at = Some(now);
                p.outcome = outcome;
                (Ok(p.clone()), true)
            }
            None => (Err(format!("No Proposal {id}")), false),
        })
    }
}
