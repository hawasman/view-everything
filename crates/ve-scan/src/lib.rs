//! Volume discovery, scanning and Catch-up.
//!
//! A scan and a Catch-up are the same operation: [`sync`] brings one Indexed Volume in the
//! [`Index`] up to date with the filesystem, starting from whatever the Index already holds.
//! A freshly added Volume holds only its root, so its first sync is a full scan.

pub mod azure;
mod capacity;
pub mod duplicates;
pub mod ftp;
pub mod live;
pub mod ntfs;
mod platform;
mod reconcile;
pub mod remote;
pub mod s3;
mod sftp;
mod volumes;
mod walk;
pub mod watch;
pub mod webdav;
mod xml;

pub use capacity::capacity;
#[cfg(windows)]
pub use platform::windows::{is_elevated, relaunch_elevated};
pub use reconcile::reconcile;
pub use remote::reconcile_remotes;
pub use volumes::{VolumeInfo, discover_volumes};

use parking_lot::{Mutex, MutexGuard, RecursiveRwLock, RecursiveRwLockReadGuard, RecursiveRwLockWriteGuard};
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ve_core::{Index, SyncState, VolumeId};

/// The Index as shared between scanners, the View computation and the GUI.
///
/// Reads are recursive, so a new reader is admitted while a writer waits: the GUI's per-frame
/// read must not queue behind a scanner that is itself waiting for a long View computation to
/// finish. Writers queue on a gate before the lock, so at most one is ever waiting at the lock
/// itself: a frame that arrives while a scanner writes waits for that one write, not for every
/// walker thread that has a folder listing ready (measured at up to 0.7 s per frame on a
/// 24-thread machine during the launch Catch-up).
pub struct SharedIndex {
    index: RecursiveRwLock<Index>,
    writers: Mutex<()>,
}

/// Exclusive access to the Index; also holds the writers' gate until dropped.
pub struct IndexWriteGuard<'a> {
    index: RecursiveRwLockWriteGuard<'a, Index>,
    _gate: MutexGuard<'a, ()>,
}

impl SharedIndex {
    pub fn new(index: Index) -> Self {
        SharedIndex {
            index: RecursiveRwLock::new(index),
            writers: Mutex::new(()),
        }
    }

    pub fn read(&self) -> RecursiveRwLockReadGuard<'_, Index> {
        self.index.read()
    }

    pub fn write(&self) -> IndexWriteGuard<'_> {
        let gate = self.writers.lock();
        IndexWriteGuard {
            index: self.index.write(),
            _gate: gate,
        }
    }

    pub fn into_inner(self) -> Index {
        self.index.into_inner()
    }
}

impl Deref for IndexWriteGuard<'_> {
    type Target = Index;
    fn deref(&self) -> &Index {
        &self.index
    }
}

impl DerefMut for IndexWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Index {
        &mut self.index
    }
}

/// Counters a running sync updates, for display.
#[derive(Debug, Default)]
pub struct Progress {
    /// Entries read from the filesystem so far.
    pub entries: AtomicU64,
    /// Folders that could not be read (usually permission denied).
    pub unreadable: AtomicU64,
    /// Set to ask a running walk to stop; folders not reached keep what the Index knows.
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    pub fn unreadable(&self) -> u64 {
        self.unreadable.load(Ordering::Relaxed)
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The error every cancelled walk returns.
pub(crate) fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// Adds the Volume mounted at `mount` to the Index with only its root, ready for [`sync`].
pub fn add_volume(index: &SharedIndex, mount: &Path, fs_type: &str, identity: &str) -> io::Result<VolumeId> {
    let meta = walk::root_meta(mount)?;
    Ok(index.write().add_volume(mount, fs_type, identity, meta, SyncState::Walk))
}

/// Brings one Volume in the Index up to date with the filesystem.
///
/// On Windows, an NTFS Volume is read from its Master File Table and kept current through the
/// change journal when the process is elevated; otherwise, and on every other platform, the
/// folder tree is walked and differences applied.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    sync_with_mode(index, volume, progress, false)
}

/// [`sync`] that never relies on what the Index already knows: an NTFS Volume's whole Master
/// File Table is read again instead of replaying the change journal. The folder walk of every
/// other Volume is already a full re-walk.
pub fn sync_full(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    sync_with_mode(index, volume, progress, true)
}

#[cfg_attr(not(windows), allow(unused_variables))]
fn sync_with_mode(index: &SharedIndex, volume: VolumeId, progress: &Progress, full: bool) -> io::Result<()> {
    if index.read().volume(volume).is_some_and(|v| v.is_remote()) {
        return remote::sync(index, volume, progress);
    }
    #[cfg(windows)]
    if ntfs::sync_windows(index, volume, progress, full)? {
        return Ok(());
    }
    walk::sync(index, volume, progress)
}

/// Brings the folder `dir` of a local Volume, at `path`, and what is below it up to date, without
/// walking the rest of the Volume. Meant for a change known to be confined to that folder.
pub fn sync_folder(index: &SharedIndex, dir: ve_core::EntryId, path: &Path, progress: &Progress) -> io::Result<()> {
    walk::sync_folder(index, dir, path, progress)
}
