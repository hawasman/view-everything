//! The portable sync: walk a Volume's folder tree in parallel and apply every difference to
//! the Index, folder by folder. A freshly added Volume holds only its root, so the same walk is
//! both the first scan and every later Catch-up.

use crate::Progress;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;
use ve_core::{EntryId, EntryMeta, Index, VolumeId};

/// Metadata of a Volume's mount point, for its root Entry.
pub(crate) fn root_meta(mount: &Path) -> io::Result<EntryMeta> {
    let metadata = fs::metadata(mount)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a folder", mount.display()),
        ));
    }
    Ok(entry_meta(&metadata, mount))
}

pub(crate) fn sync(index: &RwLock<Index>, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    let (root, mount) = {
        let index = index.read();
        let volume = index
            .volume(volume)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Volume"))?;
        (volume.root, volume.mount.clone())
    };
    let metadata = fs::metadata(&mount)?;
    index.write().update(root, entry_meta(&metadata, &mount));
    let walk = Walk {
        index,
        progress,
        device: device(&metadata),
    };
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get() * 2)
        .min(32);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("ve-scan-{i}"))
        .build()
        .map_err(io::Error::other)?;
    pool.scope(|scope| walk.visit(scope, root, mount));
    Ok(())
}

struct Walk<'a> {
    index: &'a RwLock<Index>,
    progress: &'a Progress,
    /// Entries on another device belong to another Volume and are skipped.
    device: u64,
}

struct Listed {
    name: String,
    meta: EntryMeta,
}

impl<'a> Walk<'a> {
    fn visit<'s>(&'s self, scope: &rayon::Scope<'s>, dir: EntryId, path: PathBuf) {
        let listing = self.list(&path);
        let subfolders = {
            let mut index = self.index.write();
            if !index.is_live(dir) {
                return;
            }
            apply(&mut index, dir, listing.unwrap_or_default())
        };
        for (id, name) in subfolders {
            let child = path.join(name);
            scope.spawn(move |scope| self.visit(scope, id, child));
        }
    }

    /// The folder's children as the filesystem has them now; `None` if it can't be read.
    fn list(&self, path: &Path) -> Option<Vec<Listed>> {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(_) => {
                self.progress.unreadable.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let mut listed = Vec::new();
        for entry in entries.flatten() {
            // The entry can vanish between listing and stat; it simply isn't there.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if device(&metadata) != self.device {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry_meta(&metadata, &entry.path());
            listed.push(Listed { name, meta });
        }
        self.progress
            .entries
            .fetch_add(listed.len() as u64, Ordering::Relaxed);
        Some(listed)
    }
}

/// Makes the Index children of `dir` equal to `listing`. Returns the folders to visit next.
fn apply(index: &mut Index, dir: EntryId, listing: Vec<Listed>) -> Vec<(EntryId, String)> {
    let mut existing: HashMap<String, EntryId> = index
        .children(dir)
        .iter()
        .map(|&c| (index.name(c).to_owned(), c))
        .collect();
    let mut subfolders = Vec::new();
    for Listed { name, meta } in listing {
        let id = match existing.remove(&name) {
            Some(id) if index.is_dir(id) == meta.is_dir => {
                index.update(id, meta);
                id
            }
            Some(id) => {
                index.remove(id);
                index.insert(dir, &name, meta)
            }
            None => index.insert(dir, &name, meta),
        };
        if meta.is_dir {
            subfolders.push((id, name));
        }
    }
    for (_, gone) in existing {
        index.remove(gone);
    }
    subfolders
}

/// Index metadata for one filesystem object, without following symlinks. Folders carry no size
/// of their own: Folder Size is the sum of their contents.
fn entry_meta(metadata: &Metadata, path: &Path) -> EntryMeta {
    let is_dir = metadata.is_dir();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| match t.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_nanos()).ok(),
            Err(before) => i64::try_from(before.duration().as_nanos()).ok().map(|n| -n),
        })
        .unwrap_or(0);
    let mut meta = EntryMeta {
        is_dir,
        modified,
        ..EntryMeta::default()
    };
    if !is_dir {
        meta.logical = metadata.len();
        fill_platform_meta(&mut meta, metadata, path);
    }
    meta
}

#[cfg(unix)]
fn fill_platform_meta(meta: &mut EntryMeta, metadata: &Metadata, _path: &Path) {
    use std::os::unix::fs::MetadataExt;
    meta.on_disk = metadata.blocks() * 512;
    meta.key = metadata.ino();
    meta.hardlinked = metadata.nlink() > 1;
}

#[cfg(windows)]
fn fill_platform_meta(meta: &mut EntryMeta, metadata: &Metadata, path: &Path) {
    meta.on_disk = if metadata.file_type().is_symlink() {
        0
    } else {
        crate::platform::windows::compressed_size(path).unwrap_or(metadata.len())
    };
}

#[cfg(unix)]
fn device(metadata: &Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::dev(metadata)
}

/// Windows has no device number in `std`; folder walks skip junctions and symlinks instead,
/// which is what keeps them on one Volume.
#[cfg(windows)]
fn device(_metadata: &Metadata) -> u64 {
    0
}
