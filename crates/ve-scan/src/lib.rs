//! Volume discovery, scanning and Catch-up.
//!
//! A scan and a Catch-up are the same operation: [`sync`] brings one Indexed Volume in the
//! [`Index`] up to date with the filesystem, starting from whatever the Index already holds.
//! A freshly added Volume holds only its root, so its first sync is a full scan.

mod capacity;
pub mod ntfs;
mod platform;
mod reconcile;
pub mod remote;
pub mod s3;
mod volumes;
mod walk;
pub mod watch;

#[cfg(windows)]
pub use platform::windows::{is_elevated, relaunch_elevated};
pub use capacity::capacity;
pub use reconcile::reconcile;
pub use remote::reconcile_remotes;
pub use volumes::{VolumeInfo, discover_volumes};

use parking_lot::RecursiveRwLock;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use ve_core::{Index, SyncState, VolumeId};

/// The Index as shared between scanners, the View computation and the GUI.
///
/// Recursive, so a new reader is admitted while a writer waits: the GUI's per-frame read must
/// not queue behind a scanner that is itself waiting for a long View computation to finish.
pub type SharedIndex = RecursiveRwLock<Index>;

/// Counters a running sync updates, for display.
#[derive(Debug, Default)]
pub struct Progress {
    /// Entries read from the filesystem so far.
    pub entries: AtomicU64,
    /// Folders that could not be read (usually permission denied).
    pub unreadable: AtomicU64,
}

impl Progress {
    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    pub fn unreadable(&self) -> u64 {
        self.unreadable.load(Ordering::Relaxed)
    }
}

/// Adds the Volume mounted at `mount` to the Index with only its root, ready for [`sync`].
pub fn add_volume(
    index: &SharedIndex,
    mount: &Path,
    fs_type: &str,
    identity: &str,
) -> io::Result<VolumeId> {
    let meta = walk::root_meta(mount)?;
    Ok(index
        .write()
        .add_volume(mount, fs_type, identity, meta, SyncState::Walk))
}

/// Brings one Volume in the Index up to date with the filesystem.
///
/// On Windows, an NTFS Volume is read from its Master File Table and kept current through the
/// change journal when the process is elevated; otherwise, and on every other platform, the
/// folder tree is walked and differences applied.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    if index.read().volume(volume).is_some_and(|v| v.is_remote()) {
        return remote::sync(index, volume, progress);
    }
    #[cfg(windows)]
    if ntfs::sync_windows(index, volume, progress)? {
        return Ok(());
    }
    walk::sync(index, volume, progress)
}
