//! Live change tracking (ADR-0009): a source of change events per platform, one coalescer, and
//! the apply step that re-lists the folders that changed.
//!
//! macOS reads FSEvents ([`fsevents`]); Linux reads the paths an opt-in `fanotify` helper
//! reports ([`fanotify`]). Both send [`Event`]s; [`run`] gathers a burst and hands it over as one
//! [`Batch`], and [`apply`] brings the Index in line with what is on disk.

#[cfg(target_os = "linux")]
pub mod fanotify;
#[cfg(target_os = "macos")]
pub mod fsevents;

use crate::{Progress, SharedIndex, walk};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Wait for this long without news before applying a burst.
const QUIET: Duration = Duration::from_millis(800);
/// A constant stream of news is applied at least this often.
const LONGEST: Duration = Duration::from_secs(5);

/// Events waiting for the applier at most: a source that is faster blocks (the kernel's own
/// queue overflows, and that is reported as lost changes) instead of filling memory.
pub const QUEUE: usize = 8192;
/// More distinct folders than this in one burst (a `git checkout`, a `cargo build`) are
/// re-walked as one subtree instead of re-listed one by one.
const MAX_FOLDERS: usize = 256;
/// More subtrees than this, and the Volumes are re-walked.
const MAX_SUBTREES: usize = 64;

/// What a source reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Something at this path was created, removed, renamed, resized or touched.
    Changed(PathBuf),
    /// Changes under this folder were missed or merged: re-walk it.
    Subtree(PathBuf),
    /// Changes were missed and no folder is known (queue overflow, replay refused): re-walk
    /// every Volume.
    Lost,
    /// Everything reported up to this source position is now in the event stream.
    Position(u64),
}

/// A burst of events, coalesced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Batch {
    /// Folders to re-list, one level.
    pub folders: BTreeSet<PathBuf>,
    /// Folders to re-walk fully.
    pub subtrees: BTreeSet<PathBuf>,
    pub lost: bool,
    /// The highest source position in the burst.
    pub position: Option<u64>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        *self == Batch::default()
    }

    /// Whether a re-walk of a subtree of this batch (or of every Volume) already covers `path`.
    fn covered(&self, path: &Path) -> bool {
        self.lost || path.ancestors().any(|a| self.subtrees.contains(a))
    }

    pub fn push(&mut self, event: Event) {
        match event {
            // Re-listing the parent finds a create, remove or rename of this path as well.
            Event::Changed(path) => {
                if let Some(parent) = path.parent()
                    && !self.covered(parent)
                {
                    self.folders.insert(parent.to_path_buf());
                    if self.folders.len() > MAX_FOLDERS {
                        // Their common folder is walked once; the batch stays small.
                        if let Some(common) = common_folder(&self.folders) {
                            self.push(Event::Subtree(common));
                        }
                        self.folders.clear();
                    }
                }
            }
            Event::Subtree(path) => {
                if self.covered(&path) {
                    return;
                }
                self.subtrees.retain(|s| !s.starts_with(&path));
                self.folders.retain(|f| !f.starts_with(&path));
                self.subtrees.insert(path);
                if self.subtrees.len() > MAX_SUBTREES {
                    self.lost = true;
                    self.subtrees.clear();
                    self.folders.clear();
                }
            }
            Event::Lost => {
                self.lost = true;
                self.subtrees.clear();
                self.folders.clear();
            }
            Event::Position(at) => self.position = self.position.max(Some(at)),
        }
    }
}

/// The deepest folder that holds all of `folders`.
fn common_folder(folders: &BTreeSet<PathBuf>) -> Option<PathBuf> {
    // Sorted, so the first and last differ the most.
    let (first, last) = (folders.first()?, folders.last()?);
    let mut common = PathBuf::new();
    for (a, b) in first.components().zip(last.components()) {
        if a != b {
            break;
        }
        common.push(a);
    }
    Some(common)
}

/// Reads `events` until the sender is gone, calling `apply` with each burst once it has been
/// quiet for a moment (or has gone on for a long while).
pub fn run(events: &Receiver<Event>, mut apply: impl FnMut(Batch)) {
    run_with(events, QUIET, LONGEST, &mut apply);
}

fn run_with(events: &Receiver<Event>, quiet: Duration, longest: Duration, apply: &mut impl FnMut(Batch)) {
    loop {
        let mut batch = Batch::default();
        let Ok(first) = events.recv() else { return };
        batch.push(first);
        let started = Instant::now();
        let mut open = true;
        while open && started.elapsed() < longest {
            match events.recv_timeout(quiet.min(longest - started.elapsed())) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => open = false,
            }
        }
        apply(batch);
        if !open {
            return;
        }
    }
}

/// Re-lists the folders of `batch` and re-walks its subtrees. Returns true when the whole Index
/// needs a re-walk instead (`batch.lost`), which the caller schedules.
pub fn apply(index: &SharedIndex, batch: &Batch, progress: &Progress) -> bool {
    for dir in &batch.subtrees {
        if let Some((id, path)) = nearest_indexed(index, dir) {
            let _ = walk::sync_folder(index, id, &path, progress);
        }
    }
    for dir in &batch.folders {
        if let Some((id, path)) = nearest_indexed(index, dir) {
            let _ = walk::sync_dir(index, id, &path, progress);
        }
    }
    batch.lost
}

/// `path`, or the nearest folder above it that exists on disk and is in the Index (a folder
/// that was just created, or deleted, is covered by re-listing the one above).
fn nearest_indexed(index: &SharedIndex, path: &Path) -> Option<(ve_core::EntryId, PathBuf)> {
    let index = index.read();
    path.ancestors()
        .filter(|p| p.is_dir())
        .find_map(|p| index.find_path(p).filter(|&id| index.is_dir(id)).map(|id| (id, p.to_path_buf())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use ve_core::{Index, SyncState};

    #[test]
    fn a_burst_becomes_one_batch_of_distinct_parent_folders() {
        let (tx, rx) = mpsc::channel();
        for p in ["/v/a/1", "/v/a/2", "/v/b/x"] {
            tx.send(Event::Changed(p.into())).unwrap();
        }
        tx.send(Event::Subtree("/v/c".into())).unwrap();
        tx.send(Event::Position(7)).unwrap();
        tx.send(Event::Position(5)).unwrap();
        drop(tx);
        let mut batches = Vec::new();
        run_with(&rx, Duration::from_millis(50), Duration::from_secs(1), &mut |b| batches.push(b));
        assert_eq!(batches.len(), 1);
        let b = &batches[0];
        assert_eq!(b.folders.iter().map(|p| p.to_str().unwrap()).collect::<Vec<_>>(), ["/v/a", "/v/b"]);
        assert_eq!(b.subtrees.len(), 1);
        assert_eq!(b.position, Some(7));
        assert!(!b.lost);
    }

    #[test]
    fn a_storm_collapses_to_one_subtree_and_stays_small() {
        let mut batch = Batch::default();
        for i in 0..5000 {
            batch.push(Event::Changed(format!("/v/repo/d{}/sub{}/f", i % 50, i).into()));
        }
        batch.push(Event::Changed("/v/other/f".into()));
        assert!(batch.folders.len() <= MAX_FOLDERS + 1, "{}", batch.folders.len());
        assert!(!batch.lost);
        assert!(batch.subtrees.iter().all(|s| s.starts_with("/v")), "{:?}", batch.subtrees);
        // Nothing under a walked subtree is listed on top of it.
        for f in &batch.folders {
            assert!(!f.ancestors().any(|a| batch.subtrees.contains(a)));
        }
    }

    #[test]
    fn many_unrelated_subtrees_become_a_full_re_walk() {
        let mut batch = Batch::default();
        for i in 0..=MAX_SUBTREES {
            batch.push(Event::Subtree(format!("/v{i}/x").into()));
        }
        assert!(batch.lost && batch.subtrees.is_empty());
    }

    #[test]
    fn bursts_separated_by_quiet_are_applied_separately() {
        let (tx, rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            tx.send(Event::Changed("/v/a/1".into())).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            tx.send(Event::Lost).unwrap();
        });
        let mut batches = Vec::new();
        run_with(&rx, Duration::from_millis(50), Duration::from_secs(5), &mut |b| batches.push(b));
        sender.join().unwrap();
        assert_eq!(batches.len(), 2);
        assert!(batches[1].lost);
    }

    #[test]
    fn a_constant_stream_is_still_applied_every_so_often() {
        let (tx, rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            for _ in 0..30 {
                if tx.send(Event::Changed("/v/a/1".into())).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let mut batches = 0;
        run_with(&rx, Duration::from_millis(100), Duration::from_millis(120), &mut |_| batches += 1);
        sender.join().unwrap();
        assert!(batches >= 2, "{batches}");
    }

    #[test]
    fn applying_finds_created_removed_resized_and_new_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("old")).unwrap();
        std::fs::write(root.join("keep"), b"1").unwrap();
        std::fs::write(root.join("gone"), b"1").unwrap();
        let index = SharedIndex::new(Index::new());
        let progress = Progress::default();
        let meta = walk::root_meta(&root).unwrap();
        let id = index.write().add_volume(&root, "ext4", "t", meta, SyncState::Walk);
        walk::sync(&index, id, &progress).unwrap();

        std::fs::remove_file(root.join("gone")).unwrap();
        std::fs::write(root.join("keep"), b"123456").unwrap();
        std::fs::create_dir_all(root.join("new/deep")).unwrap();
        std::fs::write(root.join("new/deep/f"), b"12").unwrap();
        std::fs::rename(root.join("old"), root.join("renamed")).unwrap();
        let mut batch = Batch::default();
        for name in ["gone", "keep", "new", "old", "renamed"] {
            batch.push(Event::Changed(root.join(name)));
        }
        assert!(!apply(&index, &batch, &progress));

        let index = index.read();
        assert!(index.find_path(&root.join("gone")).is_none());
        assert!(index.find_path(&root.join("old")).is_none());
        assert!(index.find_path(&root.join("renamed")).is_some());
        assert_eq!(index.logical(index.find_path(&root.join("keep")).unwrap()), 6);
        assert!(index.find_path(&root.join("new/deep/f")).is_some());
    }

    #[test]
    fn a_deleted_folder_is_covered_by_the_nearest_folder_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let index = SharedIndex::new(Index::new());
        let progress = Progress::default();
        let id = index
            .write()
            .add_volume(&root, "ext4", "t", walk::root_meta(&root).unwrap(), SyncState::Walk);
        walk::sync(&index, id, &progress).unwrap();
        std::fs::remove_dir_all(root.join("a")).unwrap();
        let mut batch = Batch::default();
        batch.push(Event::Changed(root.join("a/b/file")));
        apply(&index, &batch, &progress);
        assert!(index.read().find_path(&root.join("a")).is_none());
    }
}
