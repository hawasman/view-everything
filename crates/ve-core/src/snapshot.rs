//! Snapshots: the Folder Size of every folder at one moment, kept so two moments can be compared.
//!
//! A Snapshot is keyed by path, since Entry ids change on every save. Folders are stored in
//! depth-first order as a name and the position of their parent, so a path is never written out
//! in full; a Volume's root is named by its mount path. The file is the columns, bincoded and
//! gzipped.

use crate::filter::Filter;
use crate::index::{EntryId, Index};
use chrono::{Datelike, NaiveDate};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"VESNAP\0\x01";
const NO_PARENT: u32 = u32::MAX;
/// How many most recent days keep a Snapshot each; older ones keep one per month.
pub const DAILY_KEPT: i64 = 30;
const EXTENSION: &str = "snap";

#[derive(Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// Position of each folder's parent in these columns, `NO_PARENT` for a Volume's root.
    /// A parent always comes before its children.
    parent: Vec<u32>,
    /// Where each folder's name starts in `names`; one more entry than folders.
    name_start: Vec<u32>,
    names: String,
    logical: Vec<u64>,
    on_disk: Vec<u64>,
}

/// How a folder's Folder Size differs from the Snapshot's. Both are 0 and `new` is set for a
/// folder the Snapshot did not have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub logical: i64,
    pub on_disk: i64,
    pub new: bool,
}

/// A folder the Snapshot had and the Index no longer has, with the size it had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    pub name: String,
    pub logical: u64,
    pub on_disk: u64,
}

/// The Index compared with a Snapshot.
#[derive(Default)]
pub struct Comparison {
    generation: u64,
    /// Per folder slot of the Index.
    changes: Vec<Change>,
    /// Removed folders whose parent is still there, by that parent.
    removed: HashMap<EntryId, Vec<Removed>>,
    /// Volumes the Snapshot had that the Index has not.
    removed_roots: Vec<Removed>,
}

impl Snapshot {
    /// The Folder Size of every folder in `index`.
    pub fn capture(index: &Index) -> Snapshot {
        let filter = Filter::compute(index, None);
        let mut s = Snapshot::default();
        s.name_start.push(0);
        for (_, volume) in index.volumes() {
            let mut stack = vec![(volume.root, NO_PARENT)];
            while let Some((dir, parent)) = stack.pop() {
                let totals = filter.totals(index, dir);
                let at = s.parent.len() as u32;
                s.parent.push(parent);
                s.names.push_str(index.name(dir));
                s.name_start.push(s.names.len() as u32);
                s.logical.push(totals.logical);
                s.on_disk.push(totals.on_disk);
                stack.extend(index.children(dir).iter().filter(|&&c| index.is_dir(c)).map(|&c| (c, at)));
            }
        }
        s
    }

    pub fn folders(&self) -> usize {
        self.parent.len()
    }

    fn name(&self, i: usize) -> &str {
        &self.names[self.name_start[i] as usize..self.name_start[i + 1] as usize]
    }

    /// Writes the Snapshot to `path` atomically, readable only by the current user.
    pub fn save(&self, path: &Path) -> io::Result<()> {
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
            let mut packed = GzEncoder::new(out, Compression::fast());
            bincode::serialize_into(&mut packed, self).map_err(io::Error::other)?;
            packed.finish()?.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> io::Result<Snapshot> {
        let mut input = BufReader::new(fs::File::open(path)?);
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a Snapshot"));
        }
        let s: Snapshot = bincode::deserialize_from(GzDecoder::new(input)).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let n = s.parent.len();
        let sound = s.name_start.len() == n + 1
            && s.logical.len() == n
            && s.on_disk.len() == n
            && s.name_start.last().is_none_or(|&end| end as usize == s.names.len())
            && s.name_start
                .windows(2)
                .all(|w| w[0] <= w[1] && s.names.is_char_boundary(w[1] as usize))
            && s.parent.iter().enumerate().all(|(i, &p)| p == NO_PARENT || (p as usize) < i);
        if !sound {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "damaged Snapshot"));
        }
        Ok(s)
    }

    /// Compares the Index's current Folder Sizes with this Snapshot, matching folders by path.
    pub fn compare(&self, index: &Index) -> Comparison {
        self.compare_with(|| index)
    }

    /// Like [`Snapshot::compare`], taking the Index from `read` only once the work that does not
    /// need it is done, so a read lock is held for as short a time as possible.
    pub fn compare_with<I: Deref<Target = Index>>(&self, read: impl FnOnce() -> I) -> Comparison {
        let by_parent: HashMap<(u32, &str), u32> = (0..self.folders()).map(|i| ((self.parent[i], self.name(i)), i as u32)).collect();
        let index = &*read();
        let filter = Filter::compute(index, None);
        let mut out = Comparison {
            generation: index.generation(),
            changes: vec![Change::default(); index.folder_slots()],
            ..Comparison::default()
        };
        let mut matched = HashSet::new();
        // Current folder, its twin in the Snapshot, and the twin's current id.
        let mut present: HashMap<u32, EntryId> = HashMap::new();
        for (_, volume) in index.volumes() {
            let root_twin = by_parent.get(&(NO_PARENT, index.name(volume.root))).copied();
            let mut stack = vec![(volume.root, root_twin)];
            while let Some((dir, twin)) = stack.pop() {
                let totals = filter.totals(index, dir);
                let slot = index.folder_slot(dir).expect("only folders are stacked");
                out.changes[slot] = match twin {
                    Some(t) => {
                        matched.insert(t);
                        present.insert(t, dir);
                        Change {
                            logical: totals.logical as i64 - self.logical[t as usize] as i64,
                            on_disk: totals.on_disk as i64 - self.on_disk[t as usize] as i64,
                            new: false,
                        }
                    }
                    None => Change {
                        logical: totals.logical as i64,
                        on_disk: totals.on_disk as i64,
                        new: true,
                    },
                };
                for &child in index.children(dir) {
                    if index.is_dir(child) {
                        let child_twin = twin.and_then(|t| by_parent.get(&(t, index.name(child))).copied());
                        stack.push((child, child_twin));
                    }
                }
            }
        }
        // Only the topmost folder of a removed subtree is listed; what is inside went with it.
        for i in 0..self.folders() {
            if matched.contains(&(i as u32)) {
                continue;
            }
            let gone = Removed {
                name: self.name(i).to_owned(),
                logical: self.logical[i],
                on_disk: self.on_disk[i],
            };
            match self.parent[i] {
                NO_PARENT => out.removed_roots.push(gone),
                p => {
                    if let Some(&parent) = present.get(&p) {
                        out.removed.entry(parent).or_default().push(gone);
                    }
                }
            }
        }
        out
    }
}

impl Comparison {
    /// The Index generation this was computed from; a folder added since reads as unchanged.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// How a folder's Folder Size changed; `None` for files and folders added since.
    pub fn change(&self, index: &Index, id: EntryId) -> Option<Change> {
        self.changes.get(index.folder_slot(id)?).copied()
    }

    /// Folders directly inside `parent` that the Snapshot had and the Index no longer has.
    pub fn removed_in(&self, parent: EntryId) -> &[Removed] {
        self.removed.get(&parent).map_or(&[], Vec::as_slice)
    }

    /// Volumes the Snapshot had and the Index no longer has.
    pub fn removed_roots(&self) -> &[Removed] {
        &self.removed_roots
    }
}

pub fn path_for(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("{date}.{EXTENSION}"))
}

/// The dates that have a Snapshot in `dir`, newest first.
pub fn list(dir: &Path) -> Vec<NaiveDate> {
    let mut dates: Vec<NaiveDate> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = Path::new(&name);
            (name.extension()? == EXTENSION).then_some(())?;
            name.file_stem()?.to_str()?.parse().ok()
        })
        .collect();
    dates.sort_unstable_by(|a, b| b.cmp(a));
    dates
}

/// Which of `dates` to delete: those older than [`DAILY_KEPT`] days that are not the first of
/// their month. The first of a month is stable, so what survives does not change as days pass.
pub fn expired(dates: &[NaiveDate], today: NaiveDate) -> Vec<NaiveDate> {
    let mut firsts: HashMap<(i32, u32), NaiveDate> = HashMap::new();
    for &d in dates {
        let first = firsts.entry((d.year(), d.month())).or_insert(d);
        *first = (*first).min(d);
    }
    dates
        .iter()
        .copied()
        .filter(|&d| (today - d).num_days() >= DAILY_KEPT && firsts[&(d.year(), d.month())] != d)
        .collect()
}

/// Deletes the Snapshots in `dir` that retention no longer keeps.
pub fn prune(dir: &Path, today: NaiveDate) {
    for date in expired(&list(dir), today) {
        let _ = fs::remove_file(path_for(dir, date));
    }
}
