use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Source of Index generations, shared by every Index in the process.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Identifies one Entry within an [`Index`]. Stable for the lifetime of the in-memory Index;
/// ids are reassigned when an Index is saved and loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId(pub u32);

impl EntryId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VolumeId(pub u32);

/// Everything the Index records about an Entry apart from its name and place in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EntryMeta {
    pub is_dir: bool,
    /// Logical Size in bytes.
    pub logical: u64,
    /// On-disk Size in bytes.
    pub on_disk: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: i64,
    /// Filesystem identity of the underlying file: the inode on Unix, the MFT record number on
    /// NTFS. Entries sharing a key on one Volume are hard links to one file.
    pub key: u64,
    /// More than one name refers to the underlying file.
    pub hardlinked: bool,
}

/// How a Volume's Index is kept current.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncState {
    /// Re-walk the folder tree and apply differences.
    Walk,
    /// Read the NTFS change journal from `next_usn`.
    Ntfs { journal_id: u64, next_usn: i64 },
}

/// A Volume's total and free bytes, as measured at a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    pub total: u64,
    pub free: u64,
}

/// An Indexed Volume.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Volume {
    /// Where the filesystem is mounted now, or was when last seen if it is offline.
    pub mount: PathBuf,
    /// What makes this Volume the same Volume at any mount path: the filesystem's UUID or
    /// serial, or device and mount path where the platform has none.
    pub identity: String,
    /// Mounted right now. Not saved: a loaded Volume is offline until the launch finds it.
    #[serde(skip)]
    pub online: bool,
    pub fs_type: String,
    pub root: EntryId,
    pub sync: SyncState,
    /// Last Synced: wall-clock seconds since the Unix epoch, `None` until the first sync completes.
    pub last_synced: Option<i64>,
    /// Capacity measured at that sync.
    pub capacity: Option<Capacity>,
}

impl Volume {
    /// Whether Catch-up should run at `now` (seconds since the Unix epoch): the Last Synced is
    /// older than `interval`, missing, or in the future (the clock moved back). NTFS Volumes
    /// always catch up because reading the change journal is cheap (ADR-0003).
    pub fn needs_catch_up(&self, now: i64, interval: Duration) -> bool {
        if matches!(self.sync, SyncState::Ntfs { .. }) {
            return true;
        }
        match self.last_synced {
            Some(at) => at > now || now - at >= i64::try_from(interval.as_secs()).unwrap_or(i64::MAX),
            None => true,
        }
    }
}

const DIR: u8 = 1;
const HARDLINK: u8 = 2;
const REMOVED: u8 = 4;
const NO_PARENT: u32 = u32::MAX;
const NO_SLOT: u32 = u32::MAX;

const MAGIC: &[u8; 8] = b"VEINDEX\0";
/// v2 adds Last Synced and capacity per Volume; v3 adds the Volume's identity.
const FORMAT_VERSION: u32 = 3;

/// The persistent record of every Entry on every Indexed Volume.
///
/// Stored column-wise to keep per-Entry overhead small. Removed Entries leave tombstones that
/// are dropped when the Index is saved.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    parent: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u8>,
    logical: Vec<u64>,
    on_disk: Vec<u64>,
    modified: Vec<i64>,
    key: Vec<u64>,
    /// Folder slot per Entry, or `NO_SLOT` for files.
    slot: Vec<u32>,
    /// Children of each folder, by folder slot.
    children: Vec<Vec<EntryId>>,
    names: String,
    volumes: Vec<Option<Volume>>,
    live: usize,
    #[serde(skip)]
    generation: u64,
}

impl Index {
    pub fn new() -> Self {
        let mut index = Self::default();
        index.touch();
        index
    }

    /// Changes on every change, and differs between Index instances (including one loaded from
    /// disk), so observers can tell that derived data is stale.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn touch(&mut self) {
        self.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of Entry ids ever allocated, including removed ones. Valid ids are below this.
    pub fn capacity(&self) -> usize {
        self.parent.len()
    }

    /// Number of Entries currently in the Index.
    pub fn live_count(&self) -> usize {
        self.live
    }

    pub fn add_volume(
        &mut self,
        mount: &Path,
        fs_type: &str,
        identity: &str,
        meta: EntryMeta,
        sync: SyncState,
    ) -> VolumeId {
        debug_assert!(meta.is_dir, "a Volume root is a folder");
        let root = self.push(NO_PARENT, &mount.to_string_lossy(), meta);
        let volume = Volume {
            mount: mount.to_path_buf(),
            identity: identity.to_owned(),
            online: true,
            fs_type: fs_type.to_owned(),
            root,
            sync,
            last_synced: None,
            capacity: None,
        };
        let id = VolumeId(self.volumes.len() as u32);
        self.volumes.push(Some(volume));
        id
    }

    pub fn remove_volume(&mut self, id: VolumeId) {
        if let Some(volume) = self.volumes.get_mut(id.0 as usize).and_then(Option::take) {
            self.remove(volume.root);
        }
    }

    pub fn volumes(&self) -> impl Iterator<Item = (VolumeId, &Volume)> {
        self.volumes
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (VolumeId(i as u32), v)))
    }

    pub fn volume(&self, id: VolumeId) -> Option<&Volume> {
        self.volumes.get(id.0 as usize)?.as_ref()
    }

    /// The Volume with filesystem identity `identity`.
    pub fn volume_with_identity(&self, identity: &str) -> Option<VolumeId> {
        self.volumes()
            .find(|(_, v)| v.identity == identity)
            .map(|(id, _)| id)
    }

    /// Records that `id` is mounted at `mount` (a remount keeps every Entry), or not mounted.
    pub fn set_mount(&mut self, id: VolumeId, mount: Option<&Path>) {
        let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) else {
            return;
        };
        volume.online = mount.is_some();
        let Some(mount) = mount.filter(|m| *m != volume.mount) else {
            return;
        };
        volume.mount = mount.to_path_buf();
        let root = volume.root;
        let (start, len) = self.store_name(&mount.to_string_lossy());
        self.name_start[root.index()] = start;
        self.name_len[root.index()] = len;
        self.touch();
    }

    pub fn set_sync_state(&mut self, id: VolumeId, sync: SyncState) {
        if let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) {
            volume.sync = sync;
            self.touch();
        }
    }

    /// Records that `id` was brought fully up to date at `at` (seconds since the Unix epoch).
    /// Does not change [`Index::generation`]: no Entry moved, so derived data stays valid.
    pub fn mark_synced(&mut self, id: VolumeId, at: i64, capacity: Option<Capacity>) {
        if let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) {
            volume.last_synced = Some(at);
            volume.capacity = capacity;
        }
    }

    /// The Last Synced of the least recently synced Volume. The outer `None` means there are no
    /// Volumes; the inner `None` means some Volume has never finished a sync.
    pub fn oldest_synced(&self) -> Option<Option<i64>> {
        self.volumes().map(|(_, v)| v.last_synced).min()
    }

    /// The Volume whose root is `root`.
    pub fn volume_with_root(&self, root: EntryId) -> Option<VolumeId> {
        self.volumes()
            .find(|(_, v)| v.root == root)
            .map(|(id, _)| id)
    }

    /// The Volume containing `id`.
    pub fn volume_of(&self, id: EntryId) -> Option<VolumeId> {
        self.volume_with_root(self.root_of(id))
    }

    pub fn root_of(&self, mut id: EntryId) -> EntryId {
        while let Some(parent) = self.parent(id) {
            id = parent;
        }
        id
    }

    /// Adds a new Entry under the folder `parent`.
    pub fn insert(&mut self, parent: EntryId, name: &str, meta: EntryMeta) -> EntryId {
        debug_assert!(self.is_dir(parent) && self.is_live(parent));
        let id = self.push(parent.0, name, meta);
        let slot = self.slot[parent.index()] as usize;
        self.children[slot].push(id);
        id
    }

    fn push(&mut self, parent: u32, name: &str, meta: EntryMeta) -> EntryId {
        let id = EntryId(u32::try_from(self.parent.len()).expect("more than 4 billion Entries"));
        self.parent.push(parent);
        let (start, len) = self.store_name(name);
        self.name_start.push(start);
        self.name_len.push(len);
        self.flags.push(flags_for(&meta));
        self.logical.push(meta.logical);
        self.on_disk.push(meta.on_disk);
        self.modified.push(meta.modified);
        self.key.push(meta.key);
        if meta.is_dir {
            self.slot.push(self.children.len() as u32);
            self.children.push(Vec::new());
        } else {
            self.slot.push(NO_SLOT);
        }
        self.live += 1;
        self.touch();
        id
    }

    fn store_name(&mut self, name: &str) -> (u32, u16) {
        let start = u32::try_from(self.names.len()).expect("Index name storage exceeds 4 GiB");
        let len = u16::try_from(name.len()).expect("file name longer than 65535 bytes");
        self.names.push_str(name);
        (start, len)
    }

    /// Replaces an Entry's sizes, time and identity. Whether it is a folder cannot change;
    /// remove and re-insert instead.
    pub fn update(&mut self, id: EntryId, meta: EntryMeta) {
        let i = id.index();
        debug_assert_eq!(self.flags[i] & DIR != 0, meta.is_dir);
        let flags = (self.flags[i] & !HARDLINK) | if meta.hardlinked { HARDLINK } else { 0 };
        if self.flags[i] == flags
            && self.logical[i] == meta.logical
            && self.on_disk[i] == meta.on_disk
            && self.modified[i] == meta.modified
            && self.key[i] == meta.key
        {
            return;
        }
        self.flags[i] = flags;
        self.logical[i] = meta.logical;
        self.on_disk[i] = meta.on_disk;
        self.modified[i] = meta.modified;
        self.key[i] = meta.key;
        self.touch();
    }

    /// Moves an Entry (and everything beneath it) to a new parent folder and name.
    pub fn rename(&mut self, id: EntryId, new_parent: EntryId, new_name: &str) {
        debug_assert!(self.is_dir(new_parent) && self.is_live(new_parent));
        self.detach(id);
        self.parent[id.index()] = new_parent.0;
        let slot = self.slot[new_parent.index()] as usize;
        self.children[slot].push(id);
        if self.name(id) != new_name {
            let (start, len) = self.store_name(new_name);
            self.name_start[id.index()] = start;
            self.name_len[id.index()] = len;
        }
        self.touch();
    }

    /// Removes an Entry and everything beneath it.
    pub fn remove(&mut self, id: EntryId) {
        if !self.is_live(id) {
            return;
        }
        self.detach(id);
        self.tombstone(id);
        self.touch();
    }

    /// Removes several children of `dir` and everything beneath them, detaching them in one
    /// pass rather than one search per child.
    pub fn remove_children(&mut self, dir: EntryId, ids: &[EntryId]) {
        if ids.is_empty() {
            return;
        }
        let gone: std::collections::HashSet<EntryId> = ids.iter().copied().collect();
        let slot = self.slot[dir.index()] as usize;
        self.children[slot].retain(|c| !gone.contains(c));
        for &id in ids {
            if self.is_live(id) {
                self.tombstone(id);
            }
        }
        self.touch();
    }

    fn tombstone(&mut self, id: EntryId) {
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            let i = current.index();
            self.flags[i] |= REMOVED;
            self.live -= 1;
            if self.slot[i] != NO_SLOT {
                stack.append(&mut self.children[self.slot[i] as usize]);
            }
        }
    }

    fn detach(&mut self, id: EntryId) {
        let parent = self.parent[id.index()];
        if parent != NO_PARENT {
            let siblings = &mut self.children[self.slot[parent as usize] as usize];
            if let Some(pos) = siblings.iter().position(|&c| c == id) {
                siblings.swap_remove(pos);
            }
        }
    }

    #[inline]
    pub fn is_live(&self, id: EntryId) -> bool {
        self.flags.get(id.index()).is_some_and(|f| f & REMOVED == 0)
    }

    #[inline]
    pub fn is_dir(&self, id: EntryId) -> bool {
        self.flags[id.index()] & DIR != 0
    }

    #[inline]
    pub fn is_hardlinked(&self, id: EntryId) -> bool {
        self.flags[id.index()] & HARDLINK != 0
    }

    #[inline]
    pub fn name(&self, id: EntryId) -> &str {
        let start = self.name_start[id.index()] as usize;
        &self.names[start..start + self.name_len[id.index()] as usize]
    }

    #[inline]
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        let p = self.parent[id.index()];
        (p != NO_PARENT).then_some(EntryId(p))
    }

    #[inline]
    pub fn logical(&self, id: EntryId) -> u64 {
        self.logical[id.index()]
    }

    #[inline]
    pub fn on_disk(&self, id: EntryId) -> u64 {
        self.on_disk[id.index()]
    }

    #[inline]
    pub fn modified(&self, id: EntryId) -> i64 {
        self.modified[id.index()]
    }

    #[inline]
    pub fn key(&self, id: EntryId) -> u64 {
        self.key[id.index()]
    }

    pub fn meta(&self, id: EntryId) -> EntryMeta {
        EntryMeta {
            is_dir: self.is_dir(id),
            logical: self.logical(id),
            on_disk: self.on_disk(id),
            modified: self.modified(id),
            key: self.key(id),
            hardlinked: self.is_hardlinked(id),
        }
    }

    /// Children of a folder; empty for files.
    #[inline]
    pub fn children(&self, id: EntryId) -> &[EntryId] {
        match self.slot[id.index()] {
            NO_SLOT => &[],
            slot => &self.children[slot as usize],
        }
    }

    /// Dense number of a folder, below [`Index::folder_slots`]; `None` for files.
    #[inline]
    pub fn folder_slot(&self, id: EntryId) -> Option<usize> {
        match self.slot[id.index()] {
            NO_SLOT => None,
            slot => Some(slot as usize),
        }
    }

    pub fn folder_slots(&self) -> usize {
        self.children.len()
    }

    pub fn child_named(&self, dir: EntryId, name: &str) -> Option<EntryId> {
        self.children(dir)
            .iter()
            .copied()
            .find(|&c| self.name(c) == name)
    }

    /// Full path of an Entry.
    pub fn path(&self, id: EntryId) -> PathBuf {
        let mut chain = vec![id];
        let mut current = id;
        while let Some(parent) = self.parent(current) {
            chain.push(parent);
            current = parent;
        }
        let mut path = PathBuf::from(self.name(current));
        for &component in chain.iter().rev().skip(1) {
            path.push(self.name(component));
        }
        path
    }

    /// Writes the full path of an Entry into `out` with `/` separators, for matching.
    /// `chain` is scratch space reused between calls.
    pub fn write_match_path(&self, id: EntryId, out: &mut String, chain: &mut Vec<EntryId>) {
        out.clear();
        chain.clear();
        let mut current = id;
        chain.push(current);
        while let Some(parent) = self.parent(current) {
            chain.push(parent);
            current = parent;
        }
        let (root, rest) = chain.split_last().expect("chain holds at least the Entry");
        out.extend(
            self.name(*root)
                .chars()
                .map(|c| if c == '\\' { '/' } else { c }),
        );
        for &component in rest.iter().rev() {
            if !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(self.name(component));
        }
    }

    /// The Entry at `path`, if indexed.
    pub fn find_path(&self, path: &Path) -> Option<EntryId> {
        let (_, volume) = self
            .volumes()
            .filter(|(_, v)| path.starts_with(&v.mount))
            .max_by_key(|(_, v)| v.mount.as_os_str().len())?;
        let rest = path.strip_prefix(&volume.mount).ok()?;
        let mut current = volume.root;
        for component in rest.components() {
            current = self.child_named(current, component.as_os_str().to_str()?)?;
        }
        Some(current)
    }

    /// A copy containing only live Entries, with ids renumbered in depth-first order.
    pub fn compacted(&self) -> Index {
        let mut out = Index::new();
        for volume in self.volumes.iter() {
            let Some(volume) = volume else {
                out.volumes.push(None);
                continue;
            };
            let root = out.push(NO_PARENT, self.name(volume.root), self.meta(volume.root));
            out.volumes.push(Some(Volume {
                root,
                ..volume.clone()
            }));
            let mut stack = vec![(volume.root, root)];
            while let Some((old, new)) = stack.pop() {
                for &child in self.children(old) {
                    let copy = out.insert(new, self.name(child), self.meta(child));
                    if self.is_dir(child) {
                        stack.push((child, copy));
                    }
                }
            }
        }
        out
    }

    /// Writes the Index to `path` atomically, readable only by the current user.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let compact = self.compacted();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut out = BufWriter::new(options.open(&tmp)?);
            out.write_all(MAGIC)?;
            out.write_all(&FORMAT_VERSION.to_le_bytes())?;
            bincode::serialize_into(&mut out, &compact).map_err(io::Error::other)?;
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// Reads an Index written by [`Index::save`].
    pub fn load(path: &Path) -> io::Result<Index> {
        let mut input = BufReader::new(fs::File::open(path)?);
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        let mut version = [0u8; 4];
        input.read_exact(&mut version)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a saved Index"));
        }
        match u32::from_le_bytes(version) {
            FORMAT_VERSION => {}
            // Older layouts are not migrated: the Index is cheap to rebuild by a scan.
            v if v < FORMAT_VERSION => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("saved by an older version (format {v}); needs rebuild"),
                ));
            }
            v => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("saved by a newer version (format {v})"),
                ));
            }
        }
        let mut index: Index = bincode::deserialize_from(input)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        index.touch();
        Ok(index)
    }
}

fn flags_for(meta: &EntryMeta) -> u8 {
    (if meta.is_dir { DIR } else { 0 }) | (if meta.hardlinked { HARDLINK } else { 0 })
}
