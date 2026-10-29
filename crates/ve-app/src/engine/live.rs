//! Live change tracking in the Engine (ADR-0009): FSEvents on macOS, the opt-in `fanotify`
//! helper on Linux. Changes the source reports are re-listed into the Index on a thread of
//! their own; where a source is running, its Volumes need no periodic re-walk.

use super::{Command, Shared};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use ve_core::{SyncState, VolumeId};
use ve_scan::live::{self, Event};

/// Where the macOS event id is kept, beside the saved Index.
#[cfg(target_os = "macos")]
const POSITION_FILE: &str = "fsevents-id";

pub(super) struct Live {
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    events: Sender<Event>,
    /// Whether a source is running now, so the Volumes in `watched` are followed live.
    active: Arc<AtomicBool>,
    /// The local Volumes' mount points a source was given.
    watched: Vec<PathBuf>,
    /// For the Refresh a source's failure calls for.
    #[cfg(target_os = "linux")]
    commands: Sender<Command>,
    shared: Arc<Shared>,
    #[cfg(target_os = "macos")]
    stream: Option<live::fsevents::Stream>,
    #[cfg(target_os = "linux")]
    helper: Option<live::fanotify::Helper>,
    /// Set when the helper ended on its own (authentication refused, say): it is started again
    /// only when the setting is switched on again, or at the next launch.
    #[cfg(target_os = "linux")]
    helper_ended: Arc<AtomicBool>,
}

impl Live {
    /// Starts the thread that applies what sources report.
    pub(super) fn new(shared: &Arc<Shared>, commands: Sender<Command>) -> Live {
        let (events, received) = mpsc::channel();
        let ignored = super::settings::index_path().and_then(|p| p.parent().map(Path::to_path_buf));
        let applier = {
            let (shared, commands) = (shared.clone(), commands.clone());
            move || {
                live::run(&received, |mut batch| {
                    // The Index's own saves are changes in its folder too: not news.
                    if let Some(own) = &ignored {
                        batch.folders.retain(|f| !f.starts_with(own));
                        batch.subtrees.retain(|f| !f.starts_with(own));
                    }
                    let lost = live::apply(&shared.index, &batch, &shared.progress);
                    if let Some(position) = batch.position {
                        shared.live_position.store(position, Ordering::Relaxed);
                    }
                    if lost {
                        let _ = commands.send(Command::Refresh);
                    }
                    (shared.repaint)();
                })
            }
        };
        if let Err(e) = thread::Builder::new().name("ve-live".into()).spawn(applier) {
            eprintln!("Cannot apply live changes: {e}");
        }
        Live {
            events,
            active: Arc::new(AtomicBool::new(false)),
            watched: Vec::new(),
            #[cfg(target_os = "linux")]
            commands,
            shared: shared.clone(),
            #[cfg(target_os = "macos")]
            stream: None,
            #[cfg(target_os = "linux")]
            helper: None,
            #[cfg(target_os = "linux")]
            helper_ended: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The Volumes a running source keeps current. They have been walked once, so only changes
    /// since then are needed.
    pub(super) fn covered(&self) -> HashSet<VolumeId> {
        if !self.active.load(Ordering::Relaxed) {
            return HashSet::new();
        }
        let index = self.shared.index.read();
        index
            .volumes()
            .filter(|(_, v)| v.online && v.last_synced.is_some() && matches!(v.sync, SyncState::Walk) && self.watched.contains(&v.mount))
            .map(|(id, _)| id)
            .collect()
    }

    /// The local Volumes to follow changes on are now `mounts`.
    pub(super) fn set_mounts(&mut self, mounts: Vec<PathBuf>) {
        if mounts == self.watched {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(helper) = &self.helper {
            let new: Vec<PathBuf> = mounts.iter().filter(|m| !self.watched.contains(m)).cloned().collect();
            helper.watch(&new);
        }
        self.watched = mounts;
        #[cfg(target_os = "macos")]
        self.restart_stream();
    }

    /// Starts following the changes after the saved event id, or after now when there is none.
    #[cfg(target_os = "macos")]
    pub(super) fn start_stream(&mut self, mounts: Vec<PathBuf>, since: Option<u64>) {
        self.watched = mounts;
        let at = since.unwrap_or_else(live::fsevents::current_id);
        // The Index reflects everything up to here, once the catch-up has run.
        self.shared.live_position.store(at, Ordering::Relaxed);
        self.open_stream(at);
    }

    #[cfg(target_os = "macos")]
    fn restart_stream(&mut self) {
        // What has been applied so far; the new set of Volumes replays from there.
        let at = self.shared.live_position.load(Ordering::Relaxed);
        self.open_stream(if at == 0 { live::fsevents::current_id() } else { at });
    }

    #[cfg(target_os = "macos")]
    fn open_stream(&mut self, since: u64) {
        self.stream = None;
        self.active.store(false, Ordering::Relaxed);
        if self.watched.is_empty() {
            return;
        }
        match live::fsevents::Stream::start(&self.watched, Some(since), self.events.clone()) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.active.store(true, Ordering::Relaxed);
                self.shared.resolve("live");
            }
            Err(e) => self.shared.report(
                "live",
                format!("Live updates are off ({e}); changes are found by the periodic re-walk"),
            ),
        }
    }

    /// Turns the Linux helper on or off. Asking for authentication happens here, on and only on
    /// the user's request.
    #[cfg(target_os = "linux")]
    pub(super) fn set_helper(&mut self, on: bool) {
        if !on {
            self.helper = None;
            self.active.store(false, Ordering::Relaxed);
            self.shared.resolve("live");
            return;
        }
        if self.helper.is_some() && !self.helper_ended.load(Ordering::Relaxed) {
            return;
        }
        self.helper = None;
        self.helper_ended.store(false, Ordering::Relaxed);
        let Some(program) = live::fanotify::find_helper() else {
            self.shared.report(
                "live",
                format!(
                    "Live updates need the {} helper, which was not found; changes are found by the periodic re-walk",
                    live::fanotify::HELPER
                ),
            );
            return;
        };
        let (ready, ended) = (self.active.clone(), self.active.clone());
        let (shared, commands, gone) = (self.shared.clone(), self.commands.clone(), self.helper_ended.clone());
        let on_ready = self.shared.clone();
        let started = {
            live::fanotify::Helper::start(
                &program,
                &self.watched,
                self.events.clone(),
                move || {
                    ready.store(true, Ordering::Relaxed);
                    on_ready.resolve("live");
                },
                move |why| {
                    ended.store(false, Ordering::Relaxed);
                    gone.store(true, Ordering::Relaxed);
                    shared.report("live", format!("{why}; changes are found by the periodic re-walk"));
                    // Whatever changed while the helper was not watching is found now.
                    let _ = commands.send(Command::Refresh);
                },
            )
        };
        match started {
            Ok(helper) => self.helper = Some(helper),
            Err(e) => self.shared.report(
                "live",
                format!("Could not start pkexec ({e}); changes are found by the periodic re-walk"),
            ),
        }
    }
}

/// The macOS event id saved beside the Index, if any.
#[cfg(target_os = "macos")]
pub(super) fn saved_position(index: &Path) -> Option<u64> {
    std::fs::read_to_string(index.with_file_name(POSITION_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&p| p != 0)
}

/// Saves the event id the Index now reflects.
#[cfg(target_os = "macos")]
pub(super) fn save_position(index: &Path, position: u64) {
    if position != 0 {
        let _ = std::fs::write(index.with_file_name(POSITION_FILE), position.to_string());
    }
}
