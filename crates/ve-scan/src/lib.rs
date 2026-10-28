//! Volume discovery, scanning and Catch-up.
//!
//! A scan and a Catch-up are the same operation: [`sync`] brings one Indexed Volume in the
//! [`Index`] up to date with the filesystem, starting from whatever the Index already holds.
//! A freshly added Volume holds only its root, so its first sync is a full scan.

pub mod ntfs;
mod platform;
mod volumes;
mod walk;

#[cfg(windows)]
pub use platform::windows::{is_elevated, relaunch_elevated};
pub use volumes::{VolumeInfo, discover_volumes};

use parking_lot::RwLock;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use ve_core::{Index, SyncState, VolumeId};

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
pub fn add_volume(index: &RwLock<Index>, mount: &Path, fs_type: &str) -> io::Result<VolumeId> {
    let meta = walk::root_meta(mount)?;
    Ok(index
        .write()
        .add_volume(mount, fs_type, meta, SyncState::Walk))
}

/// Brings one Volume in the Index up to date with the filesystem.
///
/// On Windows, an NTFS Volume is read from its Master File Table and kept current through the
/// change journal when the process is elevated; otherwise, and on every other platform, the
/// folder tree is walked and differences applied.
pub fn sync(index: &RwLock<Index>, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    #[cfg(windows)]
    if ntfs::sync_windows(index, volume, progress)? {
        return Ok(());
    }
    walk::sync(index, volume, progress)
}
