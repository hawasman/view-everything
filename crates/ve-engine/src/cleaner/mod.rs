//! The Cleaner: Cleanables found from the Index by Cleanup Rules, reviewed as a Cleanup Plan
//! and removed permanently by Clean (ADR-0008). The rules themselves are in [`rules`]; this keeps
//! the latest Cleanables, looks for them as a Task and runs a Clean as a Task.

pub mod rules;

use crate::tasks::{Outcome as TaskOutcome, TaskHandle, TaskKind};
use crate::{Engine, Shared, format};
use parking_lot::Mutex;
use rules::{Cleanable, Outcome, Rule, Rules, foreign};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use ve_core::EntryId;
use ve_scan::VolumeInfo;

/// Least time between two looks that only follow the Index moving on.
const RELOOK_THROTTLE: Duration = Duration::from_secs(5);

/// Why nothing on `volume` can be changed right now, if so: a Remote, an Offline Volume or a
/// read-only mount.
pub fn volume_blocker(volume: &ve_core::Volume, volumes: &[VolumeInfo]) -> Option<String> {
    if volume.is_remote() {
        return Some("Remote Locations are read-only".into());
    }
    if !volume.online {
        return Some("Offline Volume: it is not mounted, so nothing on it can be changed.".into());
    }
    let info = volumes.iter().find(|v| v.mount == volume.mount)?;
    info.read_only
        .then(|| format!("{} is mounted read-only, so nothing on it can be changed.", info.mount.display()))
}

/// One Cleanable of a Clean: its rule, the path removed (or the command's working folder) and,
/// when it was ticked for being old, the age limit in days it must still meet on disk.
pub type Planned = (Arc<Rule>, PathBuf, Option<u64>);

/// The age limits in days for pre-ticking temporary files and build artefacts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ages {
    pub temp_days: u64,
    pub build_days: u64,
}

/// The latest Cleanables, told apart into the user's own and the admin-only ones.
#[derive(Clone, Default)]
pub struct Found {
    /// The user's own: what a normal Clean may remove.
    pub own: Arc<Vec<Cleanable>>,
    /// Ones the user does not own (or anything inside them): only the admin step removes them.
    pub admin: Arc<Vec<Cleanable>>,
    /// Bumped each time a look's answer replaces them.
    pub version: u64,
}

/// The Cleaner's state, kept in [`Shared`] so every client sees the same Cleanables.
#[derive(Default)]
pub struct Cleaner {
    state: Mutex<State>,
    /// The running look is measuring what it found, not yet checking Entries.
    sizing: AtomicBool,
}

#[derive(Default)]
struct State {
    /// Nothing is looked for before a client says where the rules are and which Volumes exist.
    configured: bool,
    /// The user's `cleanup.toml`.
    rules_path: Option<PathBuf>,
    rules: Arc<Rules>,
    /// When `cleanup.toml` was last read, as its modification time (None: no such file).
    rules_stamp: Option<SystemTime>,
    volumes: Vec<VolumeInfo>,
    ages: Ages,
    /// Index generation `found` was found in.
    seen: Option<u64>,
    /// When the last look started, for the throttle on looking again as the Index moves.
    last_look: Option<Instant>,
    relook_scheduled: bool,
    /// A look runs on a background thread. Nothing it has not told apart into yours and
    /// admin-only is offered, so a Cleanable never shows up as cleanable early.
    looking: bool,
    /// The user cancelled the look: none starts until they ask again or something changes.
    paused: bool,
    /// Cancel flag of the running look, so Cancel works for a look with no Task too.
    look_cancel: Arc<AtomicBool>,
    /// Counts the changes (rules, Volumes, age limits) that make a running look's answer stale.
    epoch: u64,
    found: Found,
}

impl State {
    /// The next `refresh` looks again, and a look already running is forgotten.
    fn invalidate(&mut self) {
        self.seen = None;
        self.paused = false;
        self.epoch += 1;
    }

    fn load_rules(&mut self) {
        self.rules_stamp = rules_stamp(self.rules_path.as_deref());
        self.rules = Arc::new(rules::load(self.rules_path.as_deref()));
    }
}

fn rules_stamp(path: Option<&Path>) -> Option<SystemTime> {
    std::fs::metadata(path?).ok()?.modified().ok()
}

impl Cleaner {
    /// Where the user's `cleanup.toml` is, the mounted Volumes and the age limits; the first call
    /// lets the Cleaner look.
    pub fn configure(&self, rules_path: Option<PathBuf>, volumes: Vec<VolumeInfo>, ages: Ages) {
        let mut state = self.state.lock();
        state.configured = true;
        state.rules_path = rules_path;
        state.load_rules();
        state.volumes = volumes;
        state.ages = ages;
        state.invalidate();
    }

    pub fn set_volumes(&self, volumes: Vec<VolumeInfo>) {
        let mut state = self.state.lock();
        state.volumes = volumes;
        state.invalidate();
    }

    /// Takes new age limits and looks again; false when they did not change.
    pub fn set_ages(&self, ages: Ages) -> bool {
        let mut state = self.state.lock();
        if state.ages == ages {
            return false;
        }
        state.ages = ages;
        state.invalidate();
        true
    }

    pub fn ages(&self) -> Ages {
        self.state.lock().ages
    }

    /// Looks again at the next [`refresh`], even when nothing changed.
    pub fn look_again(&self) {
        self.state.lock().invalidate();
    }

    /// The latest Cleanables.
    pub fn found(&self) -> Found {
        self.state.lock().found.clone()
    }

    /// The Cleanup Rules in force, with what was wrong with the others.
    pub fn rules(&self) -> Arc<Rules> {
        self.state.lock().rules.clone()
    }

    /// Whether a look runs with no answer to show yet (the first, or after a change).
    pub fn looking(&self) -> bool {
        let state = self.state.lock();
        state.looking && state.seen.is_none()
    }

    /// Whether the user cancelled the last look.
    pub fn paused(&self) -> bool {
        self.state.lock().paused
    }

    /// Whether the running look is measuring what it found rather than checking Entries.
    pub fn sizing(&self) -> bool {
        self.sizing.load(Ordering::Relaxed)
    }
}

/// Cancels the running look; none starts until asked again or something changes.
pub fn cancel_look(shared: &Shared) {
    shared.cleaner.state.lock().look_cancel.store(true, Ordering::Relaxed);
    shared.tasks.cancel_kind(TaskKind::CleanerSearch);
}

/// What a finished look does to the Cleaner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LookVerdict {
    /// Its answer is current: show it.
    Apply,
    /// The user cancelled it and nothing changed since: no new look until asked.
    Pause,
    /// Rules or Volumes changed meanwhile: its answer is for something else.
    Stale,
}

fn look_verdict(epoch: u64, look_epoch: u64, cancelled: bool) -> LookVerdict {
    match (epoch == look_epoch, cancelled) {
        (false, _) => LookVerdict::Stale,
        (true, true) => LookVerdict::Pause,
        (true, false) => LookVerdict::Apply,
    }
}

/// Starts looking for Cleanables again when the Index or `cleanup.toml` changed since the last
/// look. The look, with the ownership walk over every Cleanable, runs on a background thread;
/// [`Cleaner::found`] meanwhile is the last look's answer. `shared.repaint` is called when it
/// is replaced.
pub fn refresh(shared: &Arc<Shared>) {
    let mut state = shared.cleaner.state.lock();
    if !state.configured {
        return;
    }
    let stamp = rules_stamp(state.rules_path.as_deref());
    if stamp != state.rules_stamp {
        state.load_rules();
        state.invalidate();
    }
    if state.looking || state.paused {
        return;
    }
    let generation = shared.index.read().generation();
    if state.seen == Some(generation) {
        return;
    }
    // Looking again only because the Index moved (Live updates) waits out the throttle, so a
    // busy volume does not re-walk every Cleanable on every generation.
    if state.seen.is_some()
        && let Some(since) = state.last_look.map(|t| t.elapsed())
        && since < RELOOK_THROTTLE
    {
        if !state.relook_scheduled {
            state.relook_scheduled = true;
            let wait = RELOOK_THROTTLE - since;
            let shared = shared.clone();
            std::thread::spawn(move || {
                std::thread::sleep(wait);
                shared.cleaner.state.lock().relook_scheduled = false;
                (shared.repaint)();
            });
        }
        return;
    }
    state.last_look = Some(Instant::now());
    state.looking = true;
    let epoch = state.epoch;
    let (volumes, rules) = (state.volumes.clone(), state.rules.rules.clone());
    // Only a look a client waits on (the first, or after a change of rules or Volumes) is a Task;
    // looking again as the Index moves on is quiet, or every sync would fill the list.
    let task = state.seen.is_none().then(|| {
        shared
            .tasks
            .begin_by_app(TaskKind::CleanerSearch, "Look for Cleanables".into(), true)
    });
    let cancel = task.as_ref().map_or_else(|| Arc::new(AtomicBool::new(false)), |t| t.cancel_flag());
    state.look_cancel = cancel.clone();
    drop(state);
    let shared = shared.clone();
    std::thread::Builder::new()
        .name("ve-cleaner".into())
        .spawn(move || look(&shared, epoch, &volumes, &rules, task, &cancel))
        .expect("spawn Cleaner thread");
}

/// One look on its own thread, which an app-started Task drops to background priority.
fn look(shared: &Arc<Shared>, epoch: u64, volumes: &[VolumeInfo], rules: &[Arc<Rule>], task: Option<TaskHandle>, cancel: &AtomicBool) {
    // Paused, it waits before taking the Index: a Sync must not wait on it.
    if let Some(task) = &task {
        task.enter_thread();
        task.wait_while_paused();
    }
    let index = shared.index.read();
    let generation = index.generation();
    let progress = |phase: rules::Phase, done: u64, total: u64| {
        shared.cleaner.sizing.store(phase == rules::Phase::Sizing, Ordering::Relaxed);
        if let Some(task) = &task {
            task.set_progress(done, Some(total));
        }
    };
    let found = rules::find(&index, volumes, rules, cancel, &progress);
    drop(index);
    // Fail closed: a Cleanable is the user's only if the whole walk says so.
    // These walks read the disk: a Paused search stops between them.
    let (admin, own): (Vec<_>, Vec<_>) = found.into_iter().partition(|c| {
        if let Some(task) = &task {
            task.wait_while_paused();
        }
        !cancel.load(Ordering::Relaxed) && foreign(&c.path, !c.rule.is_command()).is_some()
    });
    let cancelled = cancel.load(Ordering::Relaxed);
    if let Some(task) = task {
        task.finish(if cancelled {
            TaskOutcome::Cancelled
        } else {
            TaskOutcome::Done(format!("{} Cleanables found", format::count((own.len() + admin.len()) as u64)))
        });
    }
    {
        let mut state = shared.cleaner.state.lock();
        state.looking = false;
        match look_verdict(state.epoch, epoch, cancelled) {
            LookVerdict::Apply => {
                state.seen = Some(generation);
                state.found = Found {
                    own: Arc::new(own),
                    admin: Arc::new(admin),
                    version: state.found.version + 1,
                };
                publish_entries(shared, &state.found);
            }
            // Cancelled: no new look until the user asks again or the rules change.
            LookVerdict::Pause => state.paused = true,
            // Rules or Volumes changed meanwhile: this answer is for something else, so the next
            // refresh looks again.
            LookVerdict::Stale => {}
        }
    }
    (shared.repaint)();
}

/// Hands the Query the Entries of every Cleanable, for `is:cleanable`.
fn publish_entries(shared: &Shared, found: &Found) {
    let all = found.own.iter().chain(found.admin.iter());
    let mut set = ve_core::BitSet::new(all.clone().map(|c| c.entry.index() + 1).max().unwrap_or(0));
    all.for_each(|c| set.insert(c.entry.index()));
    let mut published = shared.cleanables.lock();
    published.set = Arc::new(set);
    published.version += 1;
}

/// The Clean of `cleanables`: each ticked for being old (as of `now_ns`) must still be that old
/// on disk just before removal.
pub fn plan<'a>(cleanables: impl IntoIterator<Item = &'a Cleanable>, ages: Ages, now_ns: i64) -> Vec<Planned> {
    cleanables
        .into_iter()
        .map(|c| {
            let aged = c
                .pre_ticked(now_ns, ages.temp_days, ages.build_days)
                .then(|| c.age_limit(ages.temp_days, ages.build_days))
                .flatten();
            (c.rule.clone(), c.path.clone(), aged)
        })
        .collect()
}

/// Whatever left the disk leaves the Index, skipped-because-vanished included. One look-up per
/// folder: a Clean of many files in one folder would otherwise search its children once per file.
fn drop_from_index(shared: &Shared, outcome: &Outcome) {
    let vanished = outcome
        .skipped
        .iter()
        .filter(|(p, _)| std::fs::symlink_metadata(p).is_err())
        .map(|(p, _)| p);
    let mut by_folder: HashMap<&Path, HashSet<&str>> = HashMap::new();
    for path in outcome.removed.iter().chain(vanished) {
        if let (Some(folder), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) {
            by_folder.entry(folder).or_default().insert(name);
        }
    }
    let mut index = shared.index.write();
    for (folder, names) in by_folder {
        let Some(dir) = index.find_path(folder) else {
            continue;
        };
        let gone: Vec<EntryId> = index.children(dir).filter(|&c| names.contains(index.name(c))).collect();
        index.remove_children(dir, &gone);
    }
}

/// A Clean started as a Task and ready to run, from [`start_clean`] or [`start_admin_clean`].
pub struct Clean {
    shared: Arc<Shared>,
    plan: Vec<Planned>,
    volumes: Vec<VolumeInfo>,
    task: TaskHandle,
    admin: bool,
}

/// Starts a Clean of `plan` as a Task; None while another Clean runs.
pub fn start_clean(shared: &Arc<Shared>, plan: Vec<Planned>) -> Option<Clean> {
    start(shared, plan, false)
}

/// Starts the admin step for `plan` as a Task; None while another Clean runs.
pub fn start_admin_clean(shared: &Arc<Shared>, plan: Vec<Planned>) -> Option<Clean> {
    start(shared, plan, true)
}

fn start(shared: &Arc<Shared>, plan: Vec<Planned>, admin: bool) -> Option<Clean> {
    // A Clean cannot run twice, whichever client started it.
    if shared.tasks.is_running(TaskKind::Clean) {
        return None;
    }
    let total = plan.len();
    let label = if admin {
        format!("Clean {total} Cleanables as admin")
    } else {
        format!("Clean {total} Cleanables")
    };
    let task = shared.tasks.begin(TaskKind::Clean, label, false);
    task.set_progress(0, Some(total as u64));
    Some(Clean {
        shared: shared.clone(),
        plan,
        volumes: shared.cleaner.state.lock().volumes.clone(),
        task,
        admin,
    })
}

impl Clean {
    /// Runs the Clean on this thread, takes what left the disk out of the Index and finishes the
    /// Task. Slow: never on a UI thread.
    pub fn run(self) -> Outcome {
        let Clean {
            shared,
            plan,
            volumes,
            task,
            admin,
        } = self;
        let total = plan.len() as u64;
        let on_done = |k: usize| task.set_progress(k as u64, Some(total));
        let outcome = if admin {
            rules::admin_clean(&plan, &volumes, &on_done, &|| {
                // No countable progress while pkexec asks and removes: spin.
                task.set_label("Removing as administrator…".into());
                task.set_progress(0, None);
            })
        } else {
            rules::clean(&plan, &volumes, &on_done)
        };
        // Whatever a command changed is read back into the Index: only its working folder, since
        // a command rule's effect is confined to it (the Volume would take minutes to re-walk).
        for command in outcome.commands.iter().filter(|c| c.result.is_ok()) {
            let folder = shared.index.read().find_path(&command.cwd);
            if let Some(folder) = folder {
                let _ = ve_scan::sync_folder(&shared.index, folder, &command.cwd, &shared.progress);
            }
        }
        drop_from_index(&shared, &outcome);
        // No set_retry on purpose: a failed Clean is not offered as Retry, since re-running a
        // destructive Clean unseen is not what the user asked for.
        let failed = outcome.commands.iter().filter(|c| !c.succeeded()).count() + outcome.skipped.len();
        task.finish(if failed == 0 {
            TaskOutcome::Done(format!("Removed {} item(s)", format::count(outcome.removed.len() as u64)))
        } else if admin {
            TaskOutcome::Failed(format!("{failed} item(s) not removed"))
        } else {
            TaskOutcome::Failed(format!("{failed} item(s) skipped or failed"))
        });
        outcome
    }
}

/// The Cleaner through the Engine handle, for clients other than the app's Cleaner page.
impl Engine {
    /// The latest Cleanables, the user's own and the admin-only ones.
    pub fn cleanables(&self) -> Found {
        self.shared.cleaner.found()
    }

    /// Starts a Clean, as a Task, of the user's own Cleanables named by `(rule name, path)`. An
    /// admin-only Cleanable is refused: only the Cleaner page's admin step removes those.
    pub fn clean(&self, keys: &[(String, PathBuf)]) -> Result<(), String> {
        let found = self.shared.cleaner.found();
        let mut chosen = Vec::with_capacity(keys.len());
        for key in keys {
            let is = |c: &&Cleanable| c.rule.name == key.0 && c.path == key.1;
            if let Some(c) = found.own.iter().find(is) {
                chosen.push(c);
            } else if found.admin.iter().any(|c| is(&c)) {
                return Err(format!("{} is not yours; it needs the admin step", key.1.display()));
            } else {
                return Err(format!("{} is not a Cleanable of rule {}", key.1.display(), key.0));
            }
        }
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        let plan = plan(chosen, self.shared.cleaner.ages(), now);
        let clean = start_clean(&self.shared, plan).ok_or("A Clean is already running")?;
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("ve-clean".into())
            .spawn(move || {
                clean.run();
                (shared.repaint)();
            })
            .map_err(|e| format!("Could not start the Clean: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{LookVerdict, look_verdict};

    #[test]
    fn a_cancelled_look_pauses_only_while_its_epoch_is_current() {
        assert_eq!(look_verdict(3, 3, false), LookVerdict::Apply);
        assert_eq!(look_verdict(3, 3, true), LookVerdict::Pause);
        // Rules or Volumes changed meanwhile: neither applied nor latched.
        assert_eq!(look_verdict(4, 3, true), LookVerdict::Stale);
        assert_eq!(look_verdict(4, 3, false), LookVerdict::Stale);
    }
}
