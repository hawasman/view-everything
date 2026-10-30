//! Background work: loading, syncing and saving the Index, and computing Views for the UI.

mod live;

use crate::settings;
use crate::tasks::{Outcome, TaskKind, Tasks};
use crate::view::{FileOrder, View};
use chrono::{Local, Utc};
use live::Live;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use ve_core::snapshot::{self, Snapshot};
use ve_core::{BitSet, CategoryMap, EntryId, Filter, Index, Query, QueryContext, Remote, SyncState, Volume, VolumeId};
use ve_scan::duplicates::{self, Group, HashCache, HashProgress};
use ve_scan::{Progress, SharedIndex, VolumeInfo};

/// How often a Volume kept current by the NTFS change journal is polled while the app runs.
const JOURNAL_POLL: Duration = Duration::from_secs(5);
/// How often the GUI is told to redraw the progress of a running sync.
const PROGRESS_NUDGE: Duration = Duration::from_millis(250);
/// Minimum time between View recomputations while the Index is being filled.
const VIEW_THROTTLE: Duration = Duration::from_millis(800);
/// How often changes picked up by journal polls are saved; a full save rewrites the Index.
const POLL_SAVE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Refresh interval of a Volume the settings do not know.
const DEFAULT_REFRESH: Duration = Duration::from_secs(30 * 60);
/// The indexer never sleeps less than this between checks for stale Volumes, so a Volume that
/// keeps failing to sync is retried at a calm pace.
const MIN_WAIT: Duration = Duration::from_secs(60);

/// Per-Volume refresh interval by mount point.
pub type Intervals = HashMap<PathBuf, Duration>;

/// Tells whichever GUI is showing the Engine that it should draw again.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// State shared between the UI and the background threads.
pub struct Shared {
    pub index: SharedIndex,
    pub progress: Progress,
    pub status: Mutex<Status>,
    /// The latest View; replaced whole when recomputed.
    pub view: Mutex<Option<Arc<View>>>,
    /// The Query text last asked for, as typed even when it does not parse; a reopened window
    /// shows it again.
    pub query: Mutex<String>,
    /// Why the current Query text could not be parsed.
    pub query_error: Mutex<Option<String>>,
    busy: AtomicBool,
    /// Duplicates found by hashing, and how far a running search has got.
    pub duplicates: Mutex<DuplicateState>,
    pub hash_progress: HashProgress,
    /// The Duplicate Exclusions Query text, as set in Settings.
    pub duplicate_exclusions: Mutex<String>,
    categories: CategoryMap,
    hashing: AtomicBool,
    /// Held while the Index is written to disk, so two saves never share the temporary file.
    saving: Mutex<()>,
    /// The names the user gave Remotes, by identity; copied onto the Index's Remote Locations.
    remote_names: Mutex<HashMap<String, String>>,
    /// Set while a Snapshot is being captured and written.
    snapshotting: AtomicBool,
    /// The FSEvents event id up to which changes are in the Index; 0 when there is none.
    live_position: AtomicU64,
    /// Changes were missed and a re-walk of every Volume is owed (counts them): the position
    /// reported meanwhile waits in `live_held`, so a restart before the re-walk is done does not
    /// replay from past the lost changes.
    live_lost: AtomicU32,
    live_held: AtomicU64,
    /// The Volumes a running Live updates source keeps current, as of the last time the
    /// indexer looked (see `Live::covered`). The sidebar shows these as Live.
    pub followed: Mutex<HashSet<VolumeId>>,
    /// A Sync now is queued for the indexer and not yet started.
    pub sync_queued: AtomicBool,
    /// The folders being Synced alone now, so the same one is not started twice.
    folders_syncing: Mutex<Vec<EntryId>>,
    /// Background work the sidebar lists (Sync, Duplicate search, Cleaner, ...).
    pub tasks: Tasks,
    /// Called from the background threads whenever something the GUI shows has changed.
    pub(crate) repaint: Repaint,
}

/// The Duplicates found so far; replaced whole when a search finishes.
#[derive(Clone, Default)]
pub struct DuplicateState {
    /// Largest wasted space first.
    pub groups: Arc<Vec<Group>>,
    /// Entries in `groups`, for `dupe:`.
    pub set: Arc<BitSet>,
    /// Bumped each time a search finishes.
    pub version: u64,
    /// The Index generation the latest search started from, `None` before the first search.
    pub generation: Option<u64>,
    /// When the latest search finished.
    pub finished: Option<std::time::Instant>,
    /// The Duplicate Exclusions the latest search ran under.
    pub exclusions: String,
    /// Files the latest search skipped because of them.
    pub skipped: u64,
    /// Why the Duplicate Exclusions could not be used (they were ignored), if so.
    pub exclusion_error: Option<String>,
    /// The Duplicate Exclusions a cancelled search ran under: a search under them is not
    /// started again on its own.
    pub cancelled_under: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    /// Problems worth telling the user about, as (cause, message). A cause is reported once and
    /// dropped when the condition that raised it resolves.
    pub problems: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
pub enum Phase {
    LoadingIndex,
    /// Reading Volumes. `catching_up` when a saved Index is being brought up to date rather
    /// than built from nothing.
    Syncing {
        volumes: Vec<PathBuf>,
        catching_up: bool,
        started: Instant,
    },
    Idle,
}

enum Command {
    /// Every Volume mounted, the ones of them to index, and the Remotes to hold.
    SetVolumes(Vec<VolumeInfo>, Vec<VolumeInfo>, Vec<Remote>),
    /// Syncs the Location with this identity now (Sync now), in full.
    SyncLocation(String),
    /// Drops an Offline Volume, by identity, from the Index.
    Forget(String),
    /// Live updates lost changes or came back: every online Volume is caught up.
    Refresh,
    /// The user's Sync now of every online Volume.
    SyncNow,
    SetIntervals(Intervals),
    /// Turns the Linux `fanotify` helper on or off.
    SetLiveHelper(bool),
    Shutdown,
}

pub struct Engine {
    pub shared: Arc<Shared>,
    commands: Sender<Command>,
    queries: Sender<String>,
    /// How many Queries were sent; the view thread stops computing one when this has moved on.
    newest_query: Arc<AtomicU64>,
    indexer: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    /// Starts loading the saved Index, then Catch-up or a first scan of `volumes`.
    pub fn start(
        repaint: Repaint,
        mounted: Vec<VolumeInfo>,
        volumes: Vec<VolumeInfo>,
        remotes: Vec<Remote>,
        intervals: Intervals,
        categories: CategoryMap,
        live_helper: bool,
    ) -> Engine {
        let tasks = Tasks::new();
        {
            let repaint = repaint.clone();
            tasks.set_repaint(move || repaint());
        }
        let shared = Arc::new(Shared {
            index: SharedIndex::new(Index::new()),
            progress: Progress::default(),
            status: Mutex::new(Status {
                phase: Phase::LoadingIndex,
                problems: Vec::new(),
            }),
            view: Mutex::new(None),
            query: Mutex::new(String::new()),
            query_error: Mutex::new(None),
            busy: AtomicBool::new(true),
            duplicates: Mutex::new(DuplicateState::default()),
            hash_progress: HashProgress::default(),
            duplicate_exclusions: Mutex::new(String::new()),
            remote_names: Mutex::new(HashMap::new()),
            categories: categories.clone(),
            hashing: AtomicBool::new(false),
            saving: Mutex::new(()),
            snapshotting: AtomicBool::new(false),
            live_position: AtomicU64::new(0),
            live_lost: AtomicU32::new(0),
            live_held: AtomicU64::new(0),
            followed: Mutex::new(HashSet::new()),
            sync_queued: AtomicBool::new(false),
            folders_syncing: Mutex::new(Vec::new()),
            tasks,
            repaint,
        });
        let (commands, command_rx) = mpsc::channel();
        let (queries, query_rx) = mpsc::channel();
        let newest_query = Arc::new(AtomicU64::new(0));
        let indexer = {
            let shared = shared.clone();
            let command_tx = commands.clone();
            thread::Builder::new()
                .name("ve-indexer".into())
                .spawn(move || run_indexer(&shared, mounted, volumes, remotes, intervals, &command_rx, command_tx))
                .expect("spawn indexer thread")
        };
        {
            let shared = shared.clone();
            let newest = newest_query.clone();
            thread::Builder::new()
                .name("ve-view".into())
                .spawn(move || run_viewer(&shared, &query_rx, &newest, &categories))
                .expect("spawn view thread");
        }
        let _ = commands.send(Command::SetLiveHelper(live_helper));
        let _ = queries.send(String::new());
        Engine {
            shared,
            commands,
            queries,
            newest_query,
            indexer: Mutex::new(Some(indexer)),
        }
    }

    pub fn set_query(&self, text: String) {
        self.newest_query.fetch_add(1, Ordering::Relaxed);
        self.shared.query.lock().clone_from(&text);
        let _ = self.queries.send(text);
    }

    /// Sets the names the user gave Remotes (by identity), shown wherever a Remote is named.
    pub fn set_remote_names(&self, names: &std::collections::BTreeMap<String, String>) {
        *self.shared.remote_names.lock() = names.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        // Off the UI thread: the Index's write lock can be held for a whole save or sync.
        let shared = self.shared.clone();
        let _ = thread::Builder::new().name("ve-remote-names".into()).spawn(move || {
            shared.apply_remote_names();
            (shared.repaint)();
        });
    }

    pub fn set_volumes(&self, mounted: Vec<VolumeInfo>, volumes: Vec<VolumeInfo>, remotes: Vec<Remote>) {
        let _ = self.commands.send(Command::SetVolumes(mounted, volumes, remotes));
    }

    /// Brings one Location (by identity) up to date now, whatever its refresh interval, and
    /// leaves the others alone.
    pub fn sync_location(&self, identity: String) {
        let _ = self.commands.send(Command::SyncLocation(identity));
    }

    pub fn forget(&self, identity: String) {
        let _ = self.commands.send(Command::Forget(identity));
    }

    pub fn set_live_helper(&self, on: bool) {
        let _ = self.commands.send(Command::SetLiveHelper(on));
    }

    pub fn set_intervals(&self, intervals: Intervals) {
        let _ = self.commands.send(Command::SetIntervals(intervals));
    }

    /// Sets the Query naming the files Duplicate searches leave out. A search made under other
    /// text is stale: the next look at the Duplicates redoes it.
    pub fn set_duplicate_exclusions(&self, text: String) {
        let mut current = self.shared.duplicate_exclusions.lock();
        if *current != text {
            *current = text;
            drop(current);
            (self.shared.repaint)();
        }
    }

    /// Sync now: every online Volume (not the Remotes) is read again in full, even one Live
    /// updates cover; an NTFS Volume's whole Master File Table is read.
    pub fn sync_now(&self) {
        // A Sync cannot be started twice: one running or already queued covers this request.
        if self.shared.is_syncing() || self.shared.sync_queued.swap(true, Ordering::Relaxed) {
            return;
        }
        let _ = self.commands.send(Command::SyncNow);
    }

    /// Syncs `folder` (a file's parent, for a file) alone, as a cancellable Task.
    pub fn sync_folder(&self, folder: EntryId) {
        sync_folder(&self.shared, folder);
    }

    pub fn is_busy(&self) -> bool {
        self.shared.busy.load(Ordering::Relaxed)
    }

    /// Saves the Index and stops. If a sync is still running, the Index as filled so far is
    /// saved without waiting for it: the next launch's Catch-up completes it.
    pub fn shutdown(&self) {
        if self.is_busy() {
            if let Some(path) = settings::index_path() {
                let _saving = self.shared.saving.lock();
                if let Err(e) = self.shared.index.read().save(&path) {
                    eprintln!("Could not save the Index to {}: {e}", path.display());
                }
            }
            return;
        }
        let _ = self.commands.send(Command::Shutdown);
        if let Some(indexer) = self.indexer.lock().take() {
            let _ = indexer.join();
        }
    }
}

impl Shared {
    /// Copies the Remote names onto the Index's Remote Locations.
    fn apply_remote_names(&self) {
        let names = self.remote_names.lock().clone();
        let changed: Vec<_> = {
            let index = self.index.read();
            index
                .volumes()
                .filter(|(_, v)| v.is_remote())
                .filter(|(_, v)| v.name.as_deref() != names.get(&v.identity).map(|n| n.trim()).filter(|n| !n.is_empty()))
                .map(|(id, v)| (id, v.identity.clone()))
                .collect()
        };
        if changed.is_empty() {
            return;
        }
        let mut index = self.index.write();
        for (id, identity) in changed {
            index.set_remote_name(id, names.get(&identity).map(String::as_str));
        }
    }

    /// Whether Volumes are being read now (a Sync or Catch-up Task is running).
    pub fn is_syncing(&self) -> bool {
        matches!(self.status.lock().phase, Phase::Syncing { .. })
    }

    /// Whether the saved Index is still being loaded.
    pub fn is_loading(&self) -> bool {
        matches!(self.status.lock().phase, Phase::LoadingIndex)
    }

    /// The mount paths of the Volumes being read now.
    pub fn syncing_mounts(&self) -> Vec<PathBuf> {
        match &self.status.lock().phase {
            Phase::Syncing { volumes, .. } => volumes.clone(),
            _ => Vec::new(),
        }
    }

    pub fn hashing_now(&self) -> bool {
        self.hashing.load(Ordering::Relaxed)
    }

    fn set_phase(&self, phase: Phase) {
        self.status.lock().phase = phase;
        (self.repaint)();
    }

    fn report(&self, cause: impl Into<String>, problem: String) {
        let cause = cause.into();
        let mut status = self.status.lock();
        status.problems.retain(|(c, _)| *c != cause);
        status.problems.push((cause, problem));
        drop(status);
        (self.repaint)();
    }

    /// Forgets the problem raised for `cause`, now that it no longer holds.
    fn resolve(&self, cause: &str) {
        let mut status = self.status.lock();
        let before = status.problems.len();
        status.problems.retain(|(c, _)| c != cause);
        let changed = status.problems.len() != before;
        drop(status);
        if changed {
            (self.repaint)();
        }
    }
}

fn run_indexer(
    shared: &Arc<Shared>,
    mut mounted: Vec<VolumeInfo>,
    mut wanted: Vec<VolumeInfo>,
    mut remotes: Vec<Remote>,
    mut intervals: Intervals,
    commands: &Receiver<Command>,
    command_tx: Sender<Command>,
) {
    let index_path = settings::index_path();
    if let Some(path) = &index_path {
        match Index::load(path) {
            Ok(index) => *shared.index.write() = index,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // The format changed (a new column, say): say so rather than show a bare error.
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported && e.to_string().contains("needs rebuild") => shared.report(
                "Index format",
                "The Index format changed, so it is being rebuilt; search works as Locations finish.".to_owned(),
            ),
            Err(e) => shared.report("save", format!("The saved Index could not be used ({e}); building a new one")),
        }
    }
    (shared.repaint)();

    let mut saved_generation = shared.index.read().generation();
    let mut last_save = Instant::now();
    let returned = reconcile_volumes(shared, &mounted, &wanted, &remotes);
    // The saved Index is already on screen; Catch-up runs where it has gone stale, and for a
    // Volume that is back (possibly at another path).
    let retry_tx = command_tx.clone();
    let mut live = Live::new(shared, command_tx);
    #[cfg(target_os = "macos")]
    let replaying = {
        // Changes since the last launch are replayed from the saved event id, which stands in
        // for the re-walk of Volumes that have been walked before.
        let saved = index_path.as_deref().and_then(live::saved_position);
        live.start_stream(local_mounts(&wanted), saved);
        saved.is_some()
    };
    #[cfg(not(target_os = "macos"))]
    live.set_mounts(local_mounts(&wanted));
    let mut pending = stale_volumes(&shared.index.read(), &intervals, now_secs());
    #[cfg(target_os = "macos")]
    if replaying {
        let covered = live.covered();
        pending.retain(|id| !covered.contains(id));
    }
    for id in returned {
        if !pending.contains(&id) {
            pending.push(id);
        }
    }
    // When each Volume was last tried, so one that keeps failing is not retried on every
    // journal poll.
    let mut last_attempt: HashMap<VolumeId, Instant> = HashMap::new();
    // Journal polls run every few seconds; they neither show as busy nor save every time.
    let mut quiet = false;
    // The lost-changes count the pending re-walk of every Volume answers.
    let mut answers: Option<u32> = None;
    // Volumes the user asked to Sync now: read in full, not caught up.
    let mut forced: HashSet<VolumeId> = HashSet::new();
    // Volumes whose Sync was cancelled, and when: not retried on their own until their next interval.
    let mut cancelled_at: HashMap<VolumeId, Instant> = HashMap::new();
    // The first sync after launch is the Catch-up of the saved Index.
    let mut launch = true;
    loop {
        if !pending.is_empty() {
            let cancelled = sync_volumes(shared, &pending, &forced, launch, quiet, &retry_tx);
            launch = false;
            forced.clear();
            // A cancelled re-walk did not cover the lost changes: the position stays held.
            if !cancelled && let Some(count) = answers.take() {
                release_position(shared, count);
            }
            let _ = live.covered();
            let now = Instant::now();
            if cancelled {
                cancelled_at.extend(pending.iter().map(|&id| (id, now)));
            }
            last_attempt.extend(pending.drain(..).map(|id| (id, now)));
            if !quiet || last_save.elapsed() >= POLL_SAVE_INTERVAL {
                // Last Synced moved even when no Entry did, and a restart relies on it.
                save_if_changed(shared, index_path.as_deref(), &mut saved_generation, !quiet);
                last_save = Instant::now();
            }
        }
        if shared.busy.swap(false, Ordering::Relaxed) {
            shared.set_phase(Phase::Idle);
        }

        let journaled: Vec<VolumeId> = shared
            .index
            .read()
            .volumes()
            .filter(|(_, v)| v.online && matches!(v.sync, SyncState::Ntfs { .. }))
            .map(|(id, _)| id)
            .collect();
        let timeout = {
            let wait = next_wait(&shared.index.read(), &intervals, now_secs());
            if journaled.is_empty() { wait } else { JOURNAL_POLL.min(wait) }
        };
        quiet = false;
        match commands.recv_timeout(timeout) {
            Ok(Command::SetVolumes(now_mounted, volumes, now_remotes)) => {
                mounted = now_mounted;
                wanted = volumes;
                remotes = now_remotes;
                pending = reconcile_volumes(shared, &mounted, &wanted, &remotes);
                forced.clear();
                live.set_mounts(local_mounts(&wanted));
            }
            Ok(Command::SetLiveHelper(on)) => live.set_helper(on),
            Ok(Command::SyncLocation(identity)) => {
                let target = refresh_target(&shared.index.read(), &identity);
                // Several requests in a row (a filesystem's Volumes) are all kept.
                if let Some(id) = target
                    && !pending.contains(&id)
                {
                    pending.push(id);
                }
                if let Some(id) = target {
                    cancelled_at.remove(&id);
                }
                forced.extend(target);
            }
            Ok(Command::Forget(identity)) => {
                let mut index = shared.index.write();
                // Only an Offline Volume can be forgotten; a mounted one is un-ticked instead.
                if let Some(id) = index.volume_with_identity(&identity)
                    && index.volume(id).is_some_and(|v| !v.online)
                {
                    index.remove_volume(id);
                }
                drop(index);
                save_if_changed(shared, index_path.as_deref(), &mut saved_generation, false);
                (shared.repaint)();
                // Others queued for catch-up stay queued.
                let index = shared.index.read();
                pending.retain(|&id| index.volume(id).is_some());
            }
            Ok(Command::SyncNow) => {
                shared.sync_queued.store(false, Ordering::Relaxed);
                // Every online Volume, not the Remotes, read again in full whatever Live updates cover.
                let all: Vec<VolumeId> = shared
                    .index
                    .read()
                    .volumes()
                    .filter(|(_, v)| v.online && !v.is_remote())
                    .map(|(id, _)| id)
                    .collect();
                // Merged into what is queued (a Remote's refresh), not replacing it.
                for &id in &all {
                    if !pending.contains(&id) {
                        pending.push(id);
                    }
                    cancelled_at.remove(&id);
                }
                forced.extend(all.iter().copied());
                let count = shared.live_lost.load(Ordering::Relaxed);
                if all.is_empty() {
                    release_position(shared, count);
                } else {
                    answers = Some(count);
                }
            }
            Ok(Command::Refresh) => {
                pending = shared.index.read().volumes().filter(|(_, v)| v.online).map(|(id, _)| id).collect();
                let count = shared.live_lost.load(Ordering::Relaxed);
                if pending.is_empty() {
                    release_position(shared, count);
                } else {
                    answers = Some(count);
                }
            }
            Ok(Command::SetIntervals(new)) => {
                intervals = new;
                pending = Vec::new();
                forced.clear();
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                save_if_changed(shared, index_path.as_deref(), &mut saved_generation, true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                pending = stale_volumes(&shared.index.read(), &intervals, now_secs());
                {
                    let index = shared.index.read();
                    cancelled_at.retain(|id, at| index.volume(*id).is_some_and(|v| at.elapsed() < interval_of(&intervals, &v.mount)));
                }
                pending.retain(|id| !cancelled_at.contains_key(id));
                // Volumes followed live are current: no re-walk, and Last Synced keeps up.
                let covered = live.covered();
                pending.retain(|id| !covered.contains(id));
                {
                    let mut index = shared.index.write();
                    for &id in &covered {
                        let capacity = index.volume(id).and_then(|v| v.capacity);
                        index.mark_synced(id, now_secs(), capacity);
                    }
                }
                pending.retain(|id| journaled.contains(id) || last_attempt.get(id).is_none_or(|t| t.elapsed() >= MIN_WAIT));
                quiet = pending.iter().all(|id| journaled.contains(id));
                if !covered.is_empty() && last_save.elapsed() >= POLL_SAVE_INTERVAL {
                    save_if_changed(shared, index_path.as_deref(), &mut saved_generation, false);
                    last_save = Instant::now();
                }
            }
        }
        // The sidebar's Live state follows the helper and the Volumes at once.
        let _ = live.covered();
        if !pending.is_empty() && !quiet {
            shared.busy.store(true, Ordering::Relaxed);
        }
    }
}

/// Everything missed up to `count` lost-changes reports is in the Index now: the position held
/// back meanwhile is the one to save.
fn release_position(shared: &Shared, count: u32) {
    if shared
        .live_lost
        .compare_exchange(count, 0, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        shared
            .live_position
            .fetch_max(shared.live_held.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// The mount points of the Volumes whose changes a live source can follow: local ones.
fn local_mounts(volumes: &[VolumeInfo]) -> Vec<PathBuf> {
    volumes.iter().filter(|v| !v.network).map(|v| v.mount.clone()).collect()
}

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

fn interval_of(intervals: &Intervals, mount: &Path) -> Duration {
    intervals.get(mount).copied().unwrap_or(DEFAULT_REFRESH)
}

/// The Volumes whose Last Synced is older than their refresh interval, plus NTFS Volumes.
fn stale_volumes(index: &Index, intervals: &Intervals, now: i64) -> Vec<VolumeId> {
    index
        .volumes()
        .filter(|(_, v)| v.online && v.needs_catch_up(now, interval_of(intervals, &v.mount)))
        .map(|(id, _)| id)
        .collect()
}

/// How long the indexer can sleep before some Volume next goes stale.
fn next_wait(index: &Index, intervals: &Intervals, now: i64) -> Duration {
    index
        .volumes()
        .filter(|(_, v)| v.online)
        .map(|(_, v)| {
            let interval = interval_of(intervals, &v.mount);
            match v.last_synced {
                Some(at) if at <= now => {
                    let age = Duration::from_secs(u64::try_from(now.saturating_sub(at)).unwrap_or(0));
                    interval.saturating_sub(age)
                }
                _ => interval,
            }
        })
        .min()
        .unwrap_or(DEFAULT_REFRESH)
        .max(MIN_WAIT)
}

/// "Last synced 5 min ago" for the app's overall Last Synced (the oldest Volume's), if there
/// are any Volumes.
pub fn last_synced_label(oldest: Option<Option<i64>>, now: i64) -> Option<String> {
    Some(match oldest? {
        None => "Not synced yet".to_owned(),
        Some(at) => {
            let secs = (now - at).max(0);
            let ago = match secs {
                0..60 => "just now".to_owned(),
                60..3600 => format!("{} min ago", secs / 60),
                3600..86400 => format!("{} h ago", secs / 3600),
                _ => format!("{} d ago", secs / 86400),
            };
            format!("Last synced {ago}")
        }
    })
}

/// The Location to sync for a Sync now of `identity`: only that one, and only while it is
/// reachable (an Offline Volume has no mount to read).
fn refresh_target(index: &Index, identity: &str) -> Option<VolumeId> {
    let id = index.volume_with_identity(identity)?;
    index.volume(id)?.online.then_some(id)
}

/// Makes the Index agree with what is mounted (see [`ve_scan::reconcile`]) and with the
/// Remotes the user added. Returns the Locations newly added.
fn reconcile_volumes(shared: &Shared, mounted: &[VolumeInfo], wanted: &[VolumeInfo], remotes: &[Remote]) -> Vec<VolumeId> {
    let (mut added, failed) = ve_scan::reconcile(&shared.index, mounted, wanted);
    added.extend(ve_scan::reconcile_remotes(&shared.index, remotes));
    shared.apply_remote_names();
    for (mount, e) in failed {
        shared.report(
            format!("volume {}", mount.display()),
            format!("Cannot index {}: {e}", mount.display()),
        );
    }
    for &id in &added {
        let mount = shared.index.read().volume(id).map(|v| v.mount.clone());
        if let Some(mount) = mount {
            shared.resolve(&format!("volume {}", mount.display()));
        }
    }
    added
}

struct FolderGuard(Arc<Shared>, EntryId);

impl Drop for FolderGuard {
    fn drop(&mut self) {
        self.0.folders_syncing.lock().retain(|&f| f != self.1);
    }
}

impl Shared {
    /// Whether a Sync of this folder alone is running.
    pub fn folder_syncing(&self, folder: EntryId) -> bool {
        self.folders_syncing.lock().contains(&folder)
    }
}

/// The folder `entry` stands for: itself for a folder, its parent for a file (what a Sync, a scope or a terminal acts on).
pub fn folder_of(index: &Index, entry: EntryId) -> EntryId {
    if index.is_dir(entry) {
        entry
    } else {
        index.parent(entry).unwrap_or(entry)
    }
}

/// Syncs the folder of `entry` (its parent, for a file) alone, as a cancellable Task.
pub fn sync_folder(shared: &Arc<Shared>, entry: EntryId) {
    let folder = folder_of(&shared.index.read(), entry);
    spawn_folder_sync(shared, folder);
}

/// Whether `folder` can be synced alone: always on an online local Volume, on a Remote only where
/// its protocol lists one folder.
pub fn can_sync_folder(index: &Index, folder: EntryId) -> bool {
    let Some(volume) = index.volume_of(folder) else { return false };
    let Some(v) = index.volume(volume) else { return false };
    if v.is_remote() {
        ve_scan::remote::can_sync_folder(index, volume)
    } else {
        v.online
    }
}

/// Why a folder cannot be synced alone, for the disabled button's tooltip.
pub const WHOLE_ONLY: &str = "This Remote can only be synced whole";

/// Syncs one folder on a thread of its own, as a cancellable Task (kind Sync, "Sync <folder>").
/// Its Progress is its own, so it neither blocks nor is stopped by a whole-Volume Sync.
fn spawn_folder_sync(shared: &Arc<Shared>, folder: EntryId) {
    let (label, path, remote) = {
        let index = shared.index.read();
        let remote = index.volume_of(folder).and_then(|v| index.volume(v)).is_some_and(|v| v.is_remote());
        let path = index.path(folder);
        let remote_name = index
            .volume_of(folder)
            .and_then(|v| index.volume(v))
            .filter(|v| v.root == folder)
            .and_then(Volume::remote_display_name);
        let name = if let Some(name) = remote_name {
            name
        } else if index.parent(folder).is_none() || index.name(folder).is_empty() {
            path.display().to_string()
        } else {
            index.name(folder).to_owned()
        };
        (format!("Sync {name}"), path, remote)
    };
    {
        let mut syncing = shared.folders_syncing.lock();
        if syncing.contains(&folder) {
            return;
        }
        syncing.push(folder);
    }
    let guard = FolderGuard(shared.clone(), folder);
    let task = shared.tasks.begin(TaskKind::Sync, label, true);
    let shared = shared.clone();
    let spawned = thread::Builder::new().name("ve-sync-folder".into()).spawn({
        let shared = shared.clone();
        move || {
            let _guard = guard;
            let progress = Progress::default();
            let done = AtomicBool::new(false);
            let result = thread::scope(|scope| {
                scope.spawn(|| {
                    while !done.load(Ordering::Relaxed) {
                        task.set_progress(progress.entries(), None);
                        if task.is_cancelled() {
                            progress.cancel.store(true, Ordering::Relaxed);
                        }
                        thread::sleep(PROGRESS_NUDGE);
                    }
                });
                let result = if remote {
                    ve_scan::remote::sync_folder(&shared.index, folder, &progress)
                } else {
                    ve_scan::sync_folder(&shared.index, folder, &path, &progress)
                };
                done.store(true, Ordering::Relaxed);
                result
            });
            let outcome = match result {
                _ if progress.cancelled() || task.is_cancelled() => Outcome::Cancelled,
                Ok(()) => Outcome::Done(format!("{} Entries read", crate::format::count(progress.entries()))),
                Err(e) => {
                    let retry_shared = shared.clone();
                    task.set_retry(Arc::new(move |_| spawn_folder_sync(&retry_shared, folder)));
                    Outcome::Failed(e.to_string())
                }
            };
            task.finish(outcome);
            (shared.repaint)();
        }
    });
    // No thread: the Task handle was dropped with the closure, which fails it as "stopped".
    let _ = spawned;
}

fn sync_volumes(
    shared: &Shared,
    volumes: &[VolumeId],
    forced: &HashSet<VolumeId>,
    launch: bool,
    quiet: bool,
    retry: &Sender<Command>,
) -> bool {
    // Measuring (examples/gen_index.rs): the synthetic Index's Volume does not exist on disk.
    #[cfg(feature = "bench")]
    if std::env::var_os("VE_BENCH_MOUNT").is_some() {
        return false;
    }
    let (mounts, catching_up) = {
        let index = shared.index.read();
        let mounts: Vec<(VolumeId, PathBuf)> = volumes
            .iter()
            .filter_map(|&id| Some((id, index.volume(id)?.mount.clone())))
            .collect();
        // Catch-up is only the launch-time bring-up-to-date of a saved Index.
        let catching_up = launch
            && volumes
                .iter()
                .filter_map(|&id| index.volume(id))
                .any(|v| !index.children(v.root).is_empty());
        (mounts, catching_up)
    };
    if !quiet {
        shared.progress.entries.store(0, Ordering::Relaxed);
        shared.progress.unreadable.store(0, Ordering::Relaxed);
        shared.set_phase(Phase::Syncing {
            volumes: mounts.iter().map(|(_, m)| m.clone()).collect(),
            catching_up,
            started: Instant::now(),
        });
    }
    let done = AtomicBool::new(false);
    let failed: Mutex<Vec<String>> = Mutex::new(Vec::new());
    shared.progress.cancel.store(false, Ordering::Relaxed);
    // A quiet journal poll is not a Task: it neither shows as busy nor takes long.
    let task = (!quiet).then(|| {
        let kind = if catching_up { TaskKind::CatchUp } else { TaskKind::Sync };
        let label = match mounts.as_slice() {
            [(id, m)] => {
                let shown = shared.index.read().volume(*id).and_then(Volume::remote_display_name);
                format!("{} {}", kind.name(), shown.unwrap_or_else(|| m.display().to_string()))
            }
            all => format!("{} {} Locations", kind.name(), all.len()),
        };
        shared.tasks.begin(kind, label, true)
    });
    thread::scope(|outer| {
        // The entry count moves without a notice, so the GUI is nudged while a sync runs; the
        // same loop carries a cancel from the Task to the walk.
        if let Some(task) = &task {
            outer.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    task.set_progress(shared.progress.entries(), None);
                    if task.is_cancelled() {
                        shared.progress.cancel.store(true, Ordering::Relaxed);
                    }
                    thread::sleep(PROGRESS_NUDGE);
                }
            });
        }
        thread::scope(|scope| {
            for (id, mount) in &mounts {
                let failed = &failed;
                scope.spawn(move || {
                    let cause = format!("volume {}", mount.display());
                    let remote = shared.index.read().volume(*id).is_some_and(|v| v.is_remote());
                    // Stamped before the walk: changes made while it runs are not covered.
                    let started = now_secs();
                    let synced = if forced.contains(id) {
                        ve_scan::sync_full(&shared.index, *id, &shared.progress)
                    } else {
                        ve_scan::sync(&shared.index, *id, &shared.progress)
                    };
                    match synced {
                        // A cancelled walk covered only part of the Volume: it is not Synced.
                        Ok(_) if shared.progress.cancelled() => {}
                        Ok(_) => {
                            shared.resolve(&cause);
                            // A Remote has no mount to measure: its sync stored what `df` said.
                            let capacity = if remote {
                                shared.index.read().volume(*id).and_then(|v| v.capacity)
                            } else {
                                ve_scan::capacity(mount)
                            };
                            shared.index.write().mark_synced(*id, started, capacity);
                        }
                        // A cancel interrupts the read; that is not a failure.
                        Err(_) if shared.progress.cancelled() => {}
                        Err(e) => {
                            let identity = shared.index.read().volume(*id).map(|v| v.identity.clone());
                            failed.lock().extend(identity);
                            if remote {
                                let shown = shared.index.read().volume(*id).and_then(Volume::remote_display_name);
                                let shown = shown.unwrap_or_else(|| mount.display().to_string());
                                shared.report(cause, format!("Could not sync {shown}: {e}"));
                            } else {
                                shared.report(cause, format!("Could not read {}: {e}", mount.display()));
                            }
                        }
                    }
                });
            }
        });
        done.store(true, Ordering::Relaxed);
    });
    if let Some(task) = task {
        let entries = shared.progress.entries();
        let failed = failed.into_inner();
        let outcome = if shared.progress.cancelled() {
            Outcome::Cancelled
        } else if !failed.is_empty() {
            let retry = retry.clone();
            task.set_retry(Arc::new(move |_| {
                for identity in &failed {
                    let _ = retry.send(Command::SyncLocation(identity.clone()));
                }
            }));
            Outcome::Failed("A Location could not be read".into())
        } else {
            Outcome::Done(format!("{} Entries read", crate::format::count(entries)))
        };
        task.finish(outcome);
    }
    let cancelled = shared.progress.cancelled();
    shared.progress.cancel.store(false, Ordering::Relaxed);
    cancelled
}

/// Saves the Index if an Entry changed since the last save, or always when `force`.
fn save_if_changed(shared: &Arc<Shared>, path: Option<&Path>, saved_generation: &mut u64, force: bool) {
    let Some(path) = path else { return };
    let _saving = shared.saving.lock();
    let index = shared.index.read();
    // Taken before the save: the Index holds at least what this position covers.
    #[cfg(target_os = "macos")]
    let position = shared.live_position.load(Ordering::Relaxed);
    if !force && index.generation() == *saved_generation {
        return;
    }
    match index.save(path) {
        Ok(()) => {
            #[cfg(target_os = "macos")]
            live::save_position(path, position);
            *saved_generation = index.generation();
            shared.resolve("save");
            // A Location still on its first scan would make a partial Snapshot, and a comparison
            // with it would call everything the scan has not reached yet removed.
            if matches!(index.oldest_synced(), Some(Some(_))) {
                // The rebuild after a format change is complete.
                shared.resolve("Index format");
                snapshot_first_save_of_day(shared, path);
            }
        }
        Err(e) => shared.report("save", format!("Could not save the Index to {}: {e}", path.display())),
    }
}

const SNAPSHOTS: &str = "snapshots";

/// The folder of Snapshots, beside the Index.
pub fn snapshots_dir() -> Option<PathBuf> {
    Some(settings::index_path()?.with_file_name(SNAPSHOTS))
}

/// After the first Index save of a day (once every Location has synced), stores that day's Snapshot on a thread of its own, so
/// neither the save nor the GUI waits for it, then drops the Snapshots retention no longer keeps.
fn snapshot_first_save_of_day(shared: &Arc<Shared>, index_path: &Path) {
    let dir = index_path.with_file_name(SNAPSHOTS);
    let today = Local::now().date_naive();
    let file = snapshot::path_for(&dir, today);
    if file.exists() || shared.snapshotting.swap(true, Ordering::AcqRel) {
        return;
    }
    let shared = shared.clone();
    let spawned = thread::Builder::new().name("ve-snapshot".into()).spawn({
        let shared = shared.clone();
        move || {
            // The Index stays locked only while the folders are read, not while they are written.
            let taken = Snapshot::capture(&shared.index.read());
            match taken.save(&file) {
                Ok(()) => snapshot::prune(&dir, today),
                Err(e) => eprintln!("Could not save the Snapshot to {}: {e}", file.display()),
            }
            shared.snapshotting.store(false, Ordering::Release);
        }
    });
    if spawned.is_err() {
        shared.snapshotting.store(false, Ordering::Release);
    }
}

/// The cache of file hashes, beside the Index.
fn hashes_path() -> Option<PathBuf> {
    Some(settings::index_path()?.with_file_name("hashes.bin"))
}

/// Starts a search for Duplicates unless the last one is current for the Index, or the Index
/// is still being read (hashing would chase every change). Cached hashes keep a repeat cheap.
pub fn refresh_duplicates(shared: &Arc<Shared>) {
    let generation = shared.index.read().generation();
    let exclusions = shared.duplicate_exclusions.lock().clone();
    let state = shared.duplicates.lock();
    // The Index moves on with every live change: a finished search is redone only once it is
    // a while old, so painting the panel does not keep every Volume being walked and hashed.
    // New Duplicate Exclusions make a finished search stale at once.
    let fresh = state.finished.is_some_and(|at| at.elapsed() < DUPLICATES_MAX_AGE);
    let current = (state.exclusions == exclusions && (state.generation == Some(generation) || fresh))
        || (state.cancelled_under.as_ref() == Some(&exclusions) && fresh);
    drop(state);
    if !current && matches!(shared.status.lock().phase, Phase::Idle) {
        start_hashing(shared);
    }
}

/// The files the Duplicate Exclusions Query selects (inside a matching folder counts), and why
/// the Query was not usable. Unusable exclusions exclude nothing.
fn excluded_files(shared: &Shared, text: &str) -> (BitSet, Option<String>) {
    let index = shared.index.read();
    let mut set = BitSet::new(index.capacity());
    if text.trim().is_empty() {
        return (set, None);
    }
    let context = QueryContext::new(shared.categories.clone(), Local::now());
    let query = match Query::parse(text, &context) {
        Ok(query) => query,
        Err(e) => return (set, Some(e.to_string())),
    };
    let filter = Filter::compute(&index, Some(&query));
    for i in 0..index.capacity() {
        if filter.is_counted(EntryId(i as u32)) {
            set.insert(i);
        }
    }
    (set, None)
}

/// Searches for Duplicates redone at most this often because the Index changed.
const DUPLICATES_MAX_AGE: Duration = Duration::from_secs(300);

struct HashingDone(Arc<Shared>);

impl Drop for HashingDone {
    fn drop(&mut self) {
        // Zeroed so the next search starts at "grouping" rather than showing this one's bar.
        self.0.hash_progress.done.store(0, Ordering::Relaxed);
        self.0.hash_progress.total.store(0, Ordering::Relaxed);
        self.0.hash_progress.stage.store(0, Ordering::Relaxed);
        self.0.hashing.store(false, Ordering::Relaxed);
    }
}

pub fn start_hashing(shared: &Arc<Shared>) {
    if shared.hashing.swap(true, Ordering::Relaxed) {
        return;
    }
    let generation = shared.index.read().generation();
    let shared = shared.clone();
    let task = Arc::new(Mutex::new(Some(shared.tasks.begin(
        TaskKind::DuplicateSearch,
        "Duplicate search".into(),
        true,
    ))));
    let cancel = task.lock().as_ref().expect("just begun").cancel_flag();
    // Redraws the progress while files are hashed, and reports it to the Task.
    {
        let shared = shared.clone();
        let task = task.clone();
        let _ = thread::Builder::new().name("ve-hash-progress".into()).spawn(move || {
            while shared.hashing.load(Ordering::Relaxed) {
                thread::sleep(PROGRESS_NUDGE);
                let total = shared.hash_progress.total.load(Ordering::Relaxed);
                if let Some(task) = task.lock().as_ref() {
                    task.set_progress(shared.hash_progress.done.load(Ordering::Relaxed), (total > 0).then_some(total));
                }
                (shared.repaint)();
            }
        });
    }
    let spawned = thread::Builder::new().name("ve-hasher".into()).spawn({
        let shared = shared.clone();
        move || {
            // Clears `hashing` even if the search panics, so Rescan and `dupe:` keep working.
            let done = HashingDone(shared.clone());
            let exclusions = shared.duplicate_exclusions.lock().clone();
            let (excluded, exclusion_error) = excluded_files(&shared, &exclusions);
            let path = hashes_path();
            let mut cache = path.as_deref().map(HashCache::load).unwrap_or_default();
            let (groups, stats) = duplicates::find(
                &shared.index,
                &mut cache,
                duplicates::DEFAULT_MIN_SIZE,
                &excluded,
                &shared.hash_progress,
                &cancel,
            );
            if let Some(path) = &path
                && let Err(e) = cache.save(path)
            {
                eprintln!("Could not save the hash cache to {}: {e}", path.display());
            }
            let cancelled = cancel.load(Ordering::Relaxed);
            let mut state = shared.duplicates.lock();
            if cancelled {
                // What was found is partial: keep the earlier result (and the exclusions it
                // ran under), and do not start again until asked or the Index is old.
                state.cancelled_under = Some(exclusions);
            } else {
                let mut set = BitSet::new(shared.index.read().capacity());
                for id in groups.iter().flat_map(|g| &g.files) {
                    set.insert(id.index());
                }
                state.groups = Arc::new(groups);
                state.set = Arc::new(set);
                state.version += 1;
                state.generation = Some(generation);
                state.exclusions = exclusions;
                state.cancelled_under = None;
                state.skipped = stats.excluded;
                state.exclusion_error = exclusion_error;
            }
            state.finished = Some(std::time::Instant::now());
            drop(state);
            drop(done);
            if let Some(task) = task.lock().take() {
                task.finish(if cancelled {
                    Outcome::Cancelled
                } else if stats.excluded > 0 {
                    Outcome::Done(format!(
                        "{} files skipped by Duplicate Exclusions",
                        crate::format::count(stats.excluded)
                    ))
                } else {
                    Outcome::Done(String::new())
                });
            }
            (shared.repaint)();
        }
    });
    if spawned.is_err() {
        shared.hashing.store(false, Ordering::Relaxed);
    }
}

fn run_viewer(shared: &Arc<Shared>, queries: &Receiver<String>, newest: &AtomicU64, categories: &CategoryMap) {
    let mut seen_query = 0;
    let mut text = String::new();
    let mut query: Option<Query> = None;
    let mut computed: Option<(String, u64, u64)> = None;
    let mut last_compute = Instant::now() - VIEW_THROTTLE;
    let mut order = FileOrder::default();
    let mut seen_duplicates = 0;
    loop {
        let received = match queries.recv_timeout(Duration::from_millis(200)) {
            Ok(latest) => {
                text = queries.try_iter().last().unwrap_or(latest);
                seen_query = newest.load(Ordering::Relaxed);
                true
            }
            Err(RecvTimeoutError::Timeout) => {
                // Idle: sort the files of the newest generation now, so the next keystroke does
                // not, at most as often as a changing Index gets a new View.
                if last_compute.elapsed() >= VIEW_THROTTLE {
                    order.ensure_shared(&shared.index, &|| newest.load(Ordering::Relaxed) != seen_query);
                }
                false
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // `dupe:` needs the hashes.
        if text.contains("dupe:") {
            refresh_duplicates(shared);
        }
        // A finished search changes what `dupe:` means: parse again with its result.
        let duplicates = shared.duplicates.lock().clone();
        if received || duplicates.version != seen_duplicates {
            seen_duplicates = duplicates.version;
            {
                let parsed = if text.trim().is_empty() {
                    Ok(None)
                } else {
                    let context = QueryContext::new(categories.clone(), Local::now()).with_duplicates(duplicates.set);
                    Query::parse(&text, &context).map(Some)
                };
                match parsed {
                    Ok(parsed) => {
                        query = parsed;
                        *shared.query_error.lock() = None;
                    }
                    Err(e) => {
                        *shared.query_error.lock() = Some(e.to_string());
                        (shared.repaint)();
                        continue;
                    }
                }
            }
        }
        if shared.query_error.lock().is_some() {
            continue;
        }
        let generation = shared.index.read().generation();
        let up_to_date = computed
            .as_ref()
            .is_some_and(|(t, g, d)| *t == text && *g == generation && *d == seen_duplicates);
        let only_index_moved = computed.as_ref().is_some_and(|(t, _, d)| *t == text && *d == seen_duplicates);
        if up_to_date || (only_index_moved && last_compute.elapsed() < VIEW_THROTTLE) {
            continue;
        }
        let stale = || newest.load(Ordering::Relaxed) != seen_query;
        // Sorted without the Index lock held, so writers and a keystroke are not kept waiting.
        if !order.ensure_shared(&shared.index, &stale) {
            continue;
        }
        let view = {
            let index = shared.index.read();
            View::compute_until(&index, &text, query.as_ref(), categories, &mut order, &stale)
        };
        // Superseded mid-compute: the newer Query is in the channel; take it now.
        let Some(view) = view else { continue };
        *shared.view.lock() = Some(Arc::new(view));
        computed = Some((text.clone(), generation, seen_duplicates));
        last_compute = Instant::now();
        (shared.repaint)();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            index: SharedIndex::new(Index::new()),
            progress: Progress::default(),
            status: Mutex::new(Status {
                phase: Phase::Idle,
                problems: Vec::new(),
            }),
            view: Mutex::new(None),
            query: Mutex::new(String::new()),
            query_error: Mutex::new(None),
            busy: AtomicBool::new(false),
            duplicates: Mutex::new(DuplicateState::default()),
            hash_progress: HashProgress::default(),
            duplicate_exclusions: Mutex::new(String::new()),
            remote_names: Mutex::new(HashMap::new()),
            categories: CategoryMap::builtin(),
            sync_queued: AtomicBool::new(false),
            folders_syncing: Mutex::new(Vec::new()),
            hashing: AtomicBool::new(false),
            saving: Mutex::new(()),
            snapshotting: AtomicBool::new(false),
            live_position: AtomicU64::new(0),
            live_lost: AtomicU32::new(0),
            live_held: AtomicU64::new(0),
            followed: Mutex::new(HashSet::new()),
            tasks: Tasks::new(),
            repaint: Arc::new(|| {}),
        }
    }

    fn index_with_volumes(synced: &[Option<i64>]) -> Index {
        let mut index = Index::new();
        let root = ve_core::EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        for (i, at) in synced.iter().enumerate() {
            let id = index.add_volume(Path::new(&format!("/v{i}")), "ext4", &format!("v{i}"), root, SyncState::Walk);
            if let Some(at) = at {
                index.mark_synced(id, *at, None);
            }
        }
        index
    }

    #[::core::prelude::v1::test]
    fn no_snapshot_is_stored_until_every_location_has_synced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        let shared = Arc::new(shared());
        *shared.index.write() = index_with_volumes(&[Some(100), None]);
        let mut saved = 0;
        save_if_changed(&shared, Some(&path), &mut saved, true);
        assert!(path.exists() && !dir.path().join("snapshots").exists());
        // The second Location finishes its first sync: the next save stores the day's Snapshot.
        shared.index.write().mark_synced(VolumeId(1), 200, None);
        save_if_changed(&shared, Some(&path), &mut saved, true);
        let today = snapshot::path_for(&dir.path().join("snapshots"), Local::now().date_naive());
        for _ in 0..100 {
            if today.exists() && !shared.snapshotting.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(today.exists());
    }

    #[::core::prelude::v1::test]
    fn launch_catches_up_only_the_stale_volumes() {
        let now = 100_000;
        let index = index_with_volumes(&[Some(now - 60), Some(now - 3600), None]);
        let hour = Duration::from_secs(3600);
        let intervals: Intervals = [(PathBuf::from("/v0"), hour), (PathBuf::from("/v1"), hour / 2)]
            .into_iter()
            .collect();
        // /v0 is fresh, /v1 is older than its half hour, /v2 never synced (default interval).
        let stale = stale_volumes(&index, &intervals, now);
        assert_eq!(stale, vec![VolumeId(1), VolumeId(2)]);
    }

    #[::core::prelude::v1::test]
    fn the_indexer_sleeps_until_the_next_volume_goes_stale() {
        let now = 100_000;
        let index = index_with_volumes(&[Some(now - 600)]);
        let intervals: Intervals = [(PathBuf::from("/v0"), Duration::from_secs(1800))].into_iter().collect();
        assert_eq!(next_wait(&index, &intervals, now), Duration::from_secs(1200));
        // Already overdue: wait the floor rather than spinning.
        let overdue = index_with_volumes(&[Some(now - 7200)]);
        assert_eq!(next_wait(&overdue, &intervals, now), MIN_WAIT);
    }

    #[::core::prelude::v1::test]
    fn a_remote_is_searchable_and_its_manual_interval_never_goes_stale() {
        let mut index = index_with_volumes(&[Some(100_000 - 60)]);
        let remote = Remote::new("me@box", "/srv").unwrap();
        let id = index.add_remote(&remote);
        let root = index.volume(id).unwrap().root;
        let file = ve_core::EntryMeta {
            logical: 10,
            on_disk: 4096,
            ..Default::default()
        };
        let hit = index.insert(root, "quarterly-report.pdf", file);
        let categories = CategoryMap::builtin();
        let query = Query::parse("report", &QueryContext::new(categories.clone(), Local::now())).unwrap();
        let view = View::compute(&index, "report", Some(&query), &categories, &mut FileOrder::default());
        assert_eq!(view.flat, vec![hit]);
        assert_eq!(index.path(hit).to_string_lossy(), "ssh:me@box:/srv/quarterly-report.pdf");
        // Never synced, Manual: launch Catch-up leaves it alone; with an interval it is due.
        let manual: Intervals = [(PathBuf::from(remote.identity()), ve_core::MANUAL_REFRESH)].into_iter().collect();
        assert!(!stale_volumes(&index, &manual, 100_000).contains(&id));
        let hourly: Intervals = [(PathBuf::from(remote.identity()), Duration::from_secs(3600))]
            .into_iter()
            .collect();
        assert!(stale_volumes(&index, &hourly, 100_000).contains(&id));
    }

    #[::core::prelude::v1::test]
    fn a_targeted_refresh_names_one_reachable_location() {
        let mut index = index_with_volumes(&[Some(1), Some(2)]);
        let ids: Vec<_> = index.volumes().map(|(id, v)| (id, v.identity.clone())).collect();
        let remote = index.add_remote(&Remote::new("me@box", "/srv").unwrap());
        assert_eq!(refresh_target(&index, &ids[1].1), Some(ids[1].0));
        assert_eq!(refresh_target(&index, "ssh:me@box:/srv"), Some(remote));
        let s3 = Remote::S3(ve_core::S3Remote::new("b", "p", "minio.lan:9000", "us-east-1", true, "", "AKIA").unwrap());
        let s3_id = index.add_remote(&s3);
        assert_eq!(refresh_target(&index, &s3.identity()), Some(s3_id));
        assert_eq!(refresh_target(&index, "nobody"), None);
        index.set_mount(ids[0].0, None);
        assert_eq!(refresh_target(&index, &ids[0].1), None);
    }

    #[::core::prelude::v1::test]
    fn the_overall_last_synced_reads_as_age_of_the_oldest() {
        let now = 1_000_000;
        assert_eq!(last_synced_label(None, now), None);
        assert_eq!(last_synced_label(Some(None), now).unwrap(), "Not synced yet");
        assert_eq!(last_synced_label(Some(Some(now - 5 * 60)), now).unwrap(), "Last synced 5 min ago");
        assert_eq!(last_synced_label(Some(Some(now - 3 * 3600)), now).unwrap(), "Last synced 3 h ago");
    }

    #[::core::prelude::v1::test]
    fn a_problem_clears_when_its_cause_resolves() {
        let shared = shared();
        shared.report("volume /a", "Could not read /a".into());
        shared.report("volume /b", "Could not read /b".into());
        // The same cause reported again replaces its message instead of piling up.
        shared.report("volume /a", "Could not read /a: again".into());
        assert_eq!(shared.status.lock().problems.len(), 2);
        shared.resolve("volume /a");
        let problems = shared.status.lock().problems.clone();
        assert_eq!(problems, vec![("volume /b".to_owned(), "Could not read /b".to_owned())]);
    }
}
