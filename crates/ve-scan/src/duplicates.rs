//! Finding Duplicates: files on local Volumes with identical content.
//!
//! Candidates are narrowed in stages so most files are never fully read: equal Logical Size
//! (from the Index, free), equal first and last [`PARTIAL`] bytes, then equal BLAKE3 hash of
//! the whole file. Hard links to one file (same file id on one Volume) count once. Remotes and
//! Offline Volumes are never touched. Each Volume is hashed by one thread, one file at a time.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ve_core::{BitSet, EntryId};

use crate::SharedIndex;

/// Bytes hashed from each end of a file in the partial stage.
pub const PARTIAL: u64 = 64 * 1024;
/// Smallest Logical Size considered by default.
pub const DEFAULT_MIN_SIZE: u64 = 1024 * 1024;

type Hash = [u8; 32];

/// Counters a running search updates, for display.
#[derive(Debug, Default)]
pub struct HashProgress {
    /// 0 idle, 1 partial hashes, 2 full hashes.
    pub stage: AtomicU64,
    pub done: AtomicU64,
    pub total: AtomicU64,
}

/// Files with identical content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Logical Size of each copy.
    pub size: u64,
    /// One Entry per copy; hard links to one file appear once.
    pub files: Vec<EntryId>,
}

impl Group {
    /// Bytes that removing all but one copy would free.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64 - 1)
    }
}

/// What a search did, for display and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub partial_hashed: u64,
    pub full_hashed: u64,
    pub cache_hits: u64,
    /// Files of at least the minimum size left out because `excluded` named them.
    pub excluded: u64,
}

/// Hashes remembered between runs (`hashes.bin`), keyed by Volume, file id, size and modified
/// time: an entry whose size or modified time differs from the Index's is not reused.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HashCache {
    entries: HashMap<(String, u64), Cached>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Cached {
    size: u64,
    modified: i64,
    partial: Option<Hash>,
    full: Option<Hash>,
}

impl HashCache {
    pub fn load(path: &Path) -> HashCache {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = bincode::serialize(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct Candidate {
    id: EntryId,
    /// Position in the scanned Volumes' identities.
    volume: usize,
    key: u64,
    size: u64,
    modified: i64,
    cached: Option<Cached>,
}

/// Finds the Duplicates of every online local Volume in `index`, largest wasted space first.
/// Entries in `excluded` are treated as absent: never candidates, and their cached hashes are
/// kept. `cache` is updated in place (entries of scanned Volumes not seen this run are dropped).
/// Returns early with what is known if `cancel` is set.
pub fn find(
    index: &SharedIndex,
    cache: &mut HashCache,
    min_size: u64,
    excluded: &BitSet,
    progress: &HashProgress,
    cancel: &AtomicBool,
) -> (Vec<Group>, Stats) {
    let mut stats = Stats::default();
    let mut scanned: Vec<String> = Vec::new();
    // Cache keys of excluded files: not searched this run, but not forgotten either.
    let mut kept: std::collections::HashSet<(usize, u64)> = std::collections::HashSet::new();
    // Stage 1: equal Logical Size, hard links collapsed. The Index is read one Volume at a
    // time, so a sync is never held off for the whole walk.
    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();
    let volumes: Vec<_> = index
        .read()
        .volumes()
        .filter(|(_, v)| v.online && !v.is_remote())
        .map(|(_, v)| (v.identity.clone(), v.root))
        .collect();
    for (identity, root) in volumes {
        let volume = scanned.len();
        scanned.push(identity);
        let index = index.read();
        let mut links = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for &child in index.children(dir) {
                if index.is_dir(child) {
                    stack.push(child);
                    continue;
                }
                let (size, key) = (index.logical(child), index.key(child));
                if size < min_size.max(1) || (index.is_hardlinked(child) && !links.insert(key)) {
                    continue;
                }
                if excluded.contains(child.index()) {
                    stats.excluded += 1;
                    kept.insert((volume, key));
                    continue;
                }
                by_size.entry(size).or_default().push(Candidate {
                    id: child,
                    volume,
                    key,
                    size,
                    modified: index.modified(child),
                    cached: None,
                });
            }
        }
    }
    by_size.retain(|_, files| files.len() > 1);
    let mut files: Vec<Candidate> = by_size.into_values().flatten().collect();
    for file in &mut files {
        file.cached = cache.entries.get(&(scanned[file.volume].clone(), file.key)).cloned();
    }

    // Stage 2: equal first and last PARTIAL bytes.
    progress.stage.store(1, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    progress.total.store(files.len() as u64, Ordering::Relaxed);
    let partial = hash_stage(index, &mut files, progress, cancel, &mut stats, Stage::Partial);
    let mut groups: HashMap<(u64, Hash), Vec<usize>> = HashMap::new();
    for (i, hash) in partial.iter().enumerate() {
        if let Some(hash) = hash {
            groups.entry((files[i].size, *hash)).or_default().push(i);
        }
    }
    groups.retain(|_, members| members.len() > 1);

    // Stage 3: equal hash of the whole file.
    let mut survivors: Vec<Candidate> = Vec::new();
    for i in groups.into_values().flatten() {
        survivors.push(std::mem::replace(&mut files[i], placeholder()));
    }
    progress.stage.store(2, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    progress.total.store(survivors.len() as u64, Ordering::Relaxed);
    let full = hash_stage(index, &mut survivors, progress, cancel, &mut stats, Stage::Full);
    let mut identical: HashMap<(u64, Hash), Vec<EntryId>> = HashMap::new();
    for (file, hash) in survivors.iter().zip(&full) {
        if let Some(hash) = hash {
            identical.entry((file.size, *hash)).or_default().push(file.id);
        }
    }

    // Refresh the cache: drop what this run did not see on the scanned Volumes.
    // A cancelled run saw only part of them: keep the cache as it was.
    if !cancel.load(Ordering::Relaxed) {
        cache
            .entries
            .retain(|(volume, key), _| scanned.iter().position(|s| s == volume).is_none_or(|v| kept.contains(&(v, *key))));
    }
    for file in files.iter().chain(&survivors) {
        if let Some(cached) = &file.cached {
            cache.entries.insert((scanned[file.volume].clone(), file.key), cached.clone());
        }
    }

    let mut result: Vec<Group> = identical
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|((size, _), mut ids)| {
            ids.sort();
            Group { size, files: ids }
        })
        .collect();
    result.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then(a.files[0].cmp(&b.files[0])));
    progress.stage.store(0, Ordering::Relaxed);
    (result, stats)
}

fn placeholder() -> Candidate {
    Candidate {
        id: EntryId(0),
        volume: 0,
        key: 0,
        size: 0,
        modified: 0,
        cached: None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Partial,
    Full,
}

/// Hashes every file for `stage` (or takes the cached hash), one thread per Volume.
fn hash_stage(
    index: &SharedIndex,
    files: &mut [Candidate],
    progress: &HashProgress,
    cancel: &AtomicBool,
    stats: &mut Stats,
    stage: Stage,
) -> Vec<Option<Hash>> {
    let mut results: Vec<Option<Hash>> = vec![None; files.len()];
    let hashed = AtomicU64::new(0);
    let hits = AtomicU64::new(0);
    let mut by_volume: HashMap<usize, Vec<(&mut Candidate, &mut Option<Hash>)>> = HashMap::new();
    for (file, slot) in files.iter_mut().zip(results.iter_mut()) {
        by_volume.entry(file.volume).or_default().push((file, slot));
    }
    std::thread::scope(|scope| {
        for (_, batch) in by_volume {
            let (hashed, hits) = (&hashed, &hits);
            scope.spawn(move || {
                for (file, slot) in batch {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    if file
                        .cached
                        .as_ref()
                        .is_some_and(|c| c.size != file.size || c.modified != file.modified)
                    {
                        file.cached = None;
                    }
                    let known = file
                        .cached
                        .as_ref()
                        .and_then(|c| if stage == Stage::Partial { c.partial } else { c.full });
                    *slot = match known {
                        Some(hash) => {
                            hits.fetch_add(1, Ordering::Relaxed);
                            Some(hash)
                        }
                        None => {
                            // Paths are resolved one at a time so the Index is not held while files are read.
                            let path = {
                                let index = index.read();
                                index.is_live(file.id).then(|| index.path(file.id))
                            };
                            let hash = path.and_then(|p| hash_file(&p, file.size, file.modified, stage == Stage::Partial, cancel).ok());
                            hashed.fetch_add(1, Ordering::Relaxed);
                            if let Some(hash) = hash {
                                let cached = file.cached.get_or_insert(Cached {
                                    size: file.size,
                                    modified: file.modified,
                                    partial: None,
                                    full: None,
                                });
                                match stage {
                                    Stage::Partial => cached.partial = Some(hash),
                                    Stage::Full => cached.full = Some(hash),
                                }
                            }
                            hash
                        }
                    };
                    progress.done.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    let (hashed, hits) = (hashed.into_inner(), hits.into_inner());
    stats.cache_hits += hits;
    match stage {
        Stage::Partial => stats.partial_hashed += hashed,
        Stage::Full => stats.full_hashed += hashed,
    }
    results
}

/// Opens `path` only if it is a regular file: a symlink is never followed (a path whose last
/// component is one is refused) and a FIFO, socket or device is never opened, so nothing blocks
/// or has side effects. The type is checked again on the open handle, so a file swapped for
/// something else since the Index was read is refused too.
fn open_regular(path: &Path) -> io::Result<(File, std::fs::Metadata)> {
    let not_regular = || io::Error::other("not a regular file");
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK keeps open() from waiting on a FIFO swapped in meanwhile.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            return Err(not_regular());
        }
        // FILE_FLAG_OPEN_REPARSE_POINT: a link swapped in meanwhile is opened as itself.
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(not_regular());
    }
    Ok((file, meta))
}

fn modified_ns(meta: &std::fs::Metadata, fallback: i64) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(fallback, |d| d.as_nanos() as i64)
}

/// Feeds `len` bytes of `file` to `hasher` in 1 MiB steps, so a huge file needs no more memory
/// than a small one and a cancel lands within a step. Fewer bytes (the file shrank) is an error.
fn feed(file: &mut File, len: u64, hasher: &mut blake3::Hasher, cancel: &AtomicBool) -> io::Result<()> {
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::other("cancelled"));
        }
        let want = buf.len().min(left as usize);
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::other("file shrank while it was hashed"));
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(())
}

/// BLAKE3 of a whole file, or of its first and last [`PARTIAL`] bytes when `partial` and the
/// file is longer than both together. Only the length the Index recorded is read, so a file
/// that grows cannot make a read unbounded. A file whose length or modified time differs from
/// the Index's, before or after the read, is stale and reported as an error. (A second of
/// slack: filesystems keep time at different precisions.)
fn hash_file(path: &Path, size: u64, modified: i64, partial: bool, cancel: &AtomicBool) -> io::Result<Hash> {
    let stale = || io::Error::other("file changed since it was indexed");
    let (mut file, meta) = open_regular(path)?;
    if meta.len() != size || (modified_ns(&meta, modified) - modified).abs() > 1_000_000_000 {
        return Err(stale());
    }
    let mut hasher = blake3::Hasher::new();
    if partial && size > 2 * PARTIAL {
        feed(&mut file, PARTIAL, &mut hasher, cancel)?;
        file.seek(SeekFrom::End(-(PARTIAL as i64)))?;
        feed(&mut file, PARTIAL, &mut hasher, cancel)?;
    } else {
        feed(&mut file, size, &mut hasher, cancel)?;
    }
    let after = file.metadata()?;
    if after.len() != size || (modified_ns(&after, modified) - modified_ns(&meta, modified)).abs() > 1_000_000_000 {
        return Err(stale());
    }
    Ok(*hasher.finalize().as_bytes())
}
