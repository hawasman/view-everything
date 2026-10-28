//! Background work: loading, syncing and saving the Index, and computing Views for the UI.

use crate::settings;
use crate::view::View;
use chrono::Local;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use ve_core::{CategoryMap, Index, Query, QueryContext, SyncState, VolumeId};
use ve_scan::{Progress, SharedIndex, VolumeInfo};

/// How often a Volume kept current by the NTFS change journal is polled while the app runs.
const JOURNAL_POLL: Duration = Duration::from_secs(5);
/// How often the GUI is told to redraw the progress of a running sync.
const PROGRESS_NUDGE: Duration = Duration::from_millis(250);
/// Minimum time between View recomputations while the Index is being filled.
const VIEW_THROTTLE: Duration = Duration::from_millis(800);
/// How often changes picked up by journal polls are saved; a full save rewrites the Index.
const POLL_SAVE_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Tells whichever GUI is showing the Engine that it should draw again.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// State shared between the UI and the background threads.
pub struct Shared {
    pub index: SharedIndex,
    pub progress: Progress,
    pub status: Mutex<Status>,
    /// The latest View; replaced whole when recomputed.
    pub view: Mutex<Option<Arc<View>>>,
    /// Why the current Query text could not be parsed.
    pub query_error: Mutex<Option<String>>,
    busy: AtomicBool,
    /// Held while the Index is written to disk, so two saves never share the temporary file.
    saving: Mutex<()>,
    /// Called from the background threads whenever something the GUI shows has changed.
    repaint: Repaint,
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
    SetVolumes(Vec<VolumeInfo>),
    Refresh,
    SetRefresh(Duration),
    Shutdown,
}

pub struct Engine {
    pub shared: Arc<Shared>,
    commands: Sender<Command>,
    queries: Sender<String>,
    indexer: Option<JoinHandle<()>>,
}

impl Engine {
    /// Starts loading the saved Index, then Catch-up or a first scan of `volumes`.
    pub fn start(
        repaint: Repaint,
        volumes: Vec<VolumeInfo>,
        refresh: Duration,
        categories: CategoryMap,
    ) -> Engine {
        let shared = Arc::new(Shared {
            index: SharedIndex::new(Index::new()),
            progress: Progress::default(),
            status: Mutex::new(Status {
                phase: Phase::LoadingIndex,
                problems: Vec::new(),
            }),
            view: Mutex::new(None),
            query_error: Mutex::new(None),
            busy: AtomicBool::new(true),
            saving: Mutex::new(()),
            repaint,
        });
        let (commands, command_rx) = mpsc::channel();
        let (queries, query_rx) = mpsc::channel();
        let indexer = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("ve-indexer".into())
                .spawn(move || run_indexer(&shared, volumes, refresh, &command_rx))
                .expect("spawn indexer thread")
        };
        {
            let shared = shared.clone();
            thread::Builder::new()
                .name("ve-view".into())
                .spawn(move || run_viewer(&shared, &query_rx, &categories))
                .expect("spawn view thread");
        }
        let _ = queries.send(String::new());
        Engine {
            shared,
            commands,
            queries,
            indexer: Some(indexer),
        }
    }

    pub fn set_query(&self, text: String) {
        let _ = self.queries.send(text);
    }

    pub fn set_volumes(&self, volumes: Vec<VolumeInfo>) {
        let _ = self.commands.send(Command::SetVolumes(volumes));
    }

    pub fn set_refresh(&self, every: Duration) {
        let _ = self.commands.send(Command::SetRefresh(every));
    }

    pub fn refresh(&self) {
        let _ = self.commands.send(Command::Refresh);
    }

    pub fn is_busy(&self) -> bool {
        self.shared.busy.load(Ordering::Relaxed)
    }

    /// Saves the Index and stops. If a sync is still running, the Index as filled so far is
    /// saved without waiting for it: the next launch's Catch-up completes it.
    pub fn shutdown(&mut self) {
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
        if let Some(indexer) = self.indexer.take() {
            let _ = indexer.join();
        }
    }
}

impl Shared {
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
    shared: &Shared,
    mut wanted: Vec<VolumeInfo>,
    mut refresh: Duration,
    commands: &Receiver<Command>,
) {
    let index_path = settings::index_path();
    if let Some(path) = &index_path {
        match Index::load(path) {
            Ok(index) => *shared.index.write() = index,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => shared.report("save", format!(
                "The saved Index could not be read ({e}); building a new one"
            )),
        }
    }
    (shared.repaint)();

    let mut saved_generation = shared.index.read().generation();
    let mut last_save = Instant::now();
    let mut last_walk: HashMap<VolumeId, Instant> = HashMap::new();
    reconcile_volumes(shared, &wanted);
    let mut pending: Vec<VolumeId> = shared.index.read().volumes().map(|(id, _)| id).collect();
    // Journal polls run every few seconds; they neither show as busy nor save every time.
    let mut quiet = false;
    loop {
        if !pending.is_empty() {
            sync_volumes(shared, &pending, quiet);
            let now = Instant::now();
            if !quiet {
                last_walk.extend(pending.iter().map(|&id| (id, now)));
            }
            pending.clear();
            if !quiet || last_save.elapsed() >= POLL_SAVE_INTERVAL {
                save_if_changed(shared, index_path.as_deref(), &mut saved_generation);
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
            .filter(|(_, v)| matches!(v.sync, SyncState::Ntfs { .. }))
            .map(|(id, _)| id)
            .collect();
        let timeout = if journaled.is_empty() {
            refresh
        } else {
            JOURNAL_POLL.min(refresh)
        };
        quiet = false;
        match commands.recv_timeout(timeout) {
            Ok(Command::SetVolumes(volumes)) => {
                wanted = volumes;
                pending = reconcile_volumes(shared, &wanted);
            }
            Ok(Command::Refresh) => {
                pending = shared.index.read().volumes().map(|(id, _)| id).collect()
            }
            Ok(Command::SetRefresh(every)) => {
                refresh = every;
                pending = Vec::new();
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                save_if_changed(shared, index_path.as_deref(), &mut saved_generation);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                let due = |id: &VolumeId| last_walk.get(id).is_none_or(|t| t.elapsed() >= refresh);
                pending = shared
                    .index
                    .read()
                    .volumes()
                    .map(|(id, _)| id)
                    .filter(|id| journaled.contains(id) || due(id))
                    .collect();
                quiet = pending.iter().all(|id| journaled.contains(id));
            }
        }
        if !pending.is_empty() && !quiet {
            shared.busy.store(true, Ordering::Relaxed);
        }
    }
}

/// Makes the Index hold exactly the `wanted` Volumes. Returns the ones newly added.
fn reconcile_volumes(shared: &Shared, wanted: &[VolumeInfo]) -> Vec<VolumeId> {
    {
        let mut index = shared.index.write();
        let unwanted: Vec<VolumeId> = index
            .volumes()
            .filter(|(_, v)| !wanted.iter().any(|w| w.mount == v.mount))
            .map(|(id, _)| id)
            .collect();
        for id in unwanted {
            index.remove_volume(id);
        }
    }
    let mut added = Vec::new();
    for volume in wanted {
        let present = shared
            .index
            .read()
            .volumes()
            .any(|(_, v)| v.mount == volume.mount);
        if present {
            continue;
        }
        let cause = format!("volume {}", volume.mount.display());
        match ve_scan::add_volume(&shared.index, &volume.mount, &volume.fs_type) {
            Ok(id) => {
                shared.resolve(&cause);
                added.push(id);
            }
            Err(e) => shared.report(cause, format!("Cannot index {}: {e}", volume.mount.display())),
        }
    }
    added
}

fn sync_volumes(shared: &Shared, volumes: &[VolumeId], quiet: bool) {
    // Measuring (examples/gen_index.rs): the synthetic Index's Volume does not exist on disk.
    #[cfg(feature = "bench")]
    if std::env::var_os("VE_BENCH_MOUNT").is_some() {
        return;
    }
    let (mounts, catching_up) = {
        let index = shared.index.read();
        let mounts: Vec<(VolumeId, PathBuf)> = volumes
            .iter()
            .filter_map(|&id| Some((id, index.volume(id)?.mount.clone())))
            .collect();
        let catching_up = volumes
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
    thread::scope(|outer| {
        // The entry count moves without a notice, so the GUI is nudged while a sync runs.
        if !quiet {
            outer.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    (shared.repaint)();
                    thread::sleep(PROGRESS_NUDGE);
                }
            });
        }
        thread::scope(|scope| {
            for (id, mount) in &mounts {
                scope.spawn(move || {
                    let cause = format!("volume {}", mount.display());
                    match ve_scan::sync(&shared.index, *id, &shared.progress) {
                        Ok(_) => shared.resolve(&cause),
                        Err(e) => shared
                            .report(cause, format!("Could not read {}: {e}", mount.display())),
                    }
                });
            }
        });
        done.store(true, Ordering::Relaxed);
    });
}

fn save_if_changed(shared: &Shared, path: Option<&std::path::Path>, saved_generation: &mut u64) {
    let Some(path) = path else { return };
    let _saving = shared.saving.lock();
    let index = shared.index.read();
    if index.generation() == *saved_generation {
        return;
    }
    match index.save(path) {
        Ok(()) => {
            *saved_generation = index.generation();
            shared.resolve("save");
        }
        Err(e) => shared.report("save", format!(
            "Could not save the Index to {}: {e}",
            path.display()
        )),
    }
}

fn run_viewer(shared: &Shared, queries: &Receiver<String>, categories: &CategoryMap) {
    let mut text = String::new();
    let mut query: Option<Query> = None;
    let mut computed: Option<(String, u64)> = None;
    let mut last_compute = Instant::now() - VIEW_THROTTLE;
    loop {
        match queries.recv_timeout(Duration::from_millis(200)) {
            Ok(latest) => {
                text = queries.try_iter().last().unwrap_or(latest);
                let parsed = if text.trim().is_empty() {
                    Ok(None)
                } else {
                    Query::parse(&text, &QueryContext::new(categories.clone(), Local::now()))
                        .map(Some)
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
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if shared.query_error.lock().is_some() {
            continue;
        }
        let generation = shared.index.read().generation();
        let up_to_date = computed
            .as_ref()
            .is_some_and(|(t, g)| *t == text && *g == generation);
        let only_index_moved = computed.as_ref().is_some_and(|(t, _)| *t == text);
        if up_to_date || (only_index_moved && last_compute.elapsed() < VIEW_THROTTLE) {
            continue;
        }
        let view = {
            let index = shared.index.read();
            View::compute(&index, &text, query.as_ref(), categories)
        };
        *shared.view.lock() = Some(Arc::new(view));
        computed = Some((text.clone(), generation));
        last_compute = Instant::now();
        (shared.repaint)();
    }
}
