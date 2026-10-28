//! NTFS support: the Master File Table is parsed directly, and on Windows the change journal
//! drives Catch-up. The parser is portable so it can be tested against image files.

pub mod mft;
#[cfg(windows)]
mod sync;

#[cfg(windows)]
pub(crate) use sync::sync_windows;

use crate::Progress;
use mft::{ROOT_RECORD, Record};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Seek};
use std::sync::atomic::Ordering;
use ve_core::{EntryId, EntryMeta, Index, VolumeId};

/// Entries inserted per write-lock acquisition, so readers get in between.
const BATCH: usize = 4096;

pub(crate) fn entry_meta(record: &Record) -> EntryMeta {
    EntryMeta {
        is_dir: record.is_dir,
        logical: record.logical,
        on_disk: record.on_disk,
        modified: record.modified,
        key: record.number,
        hardlinked: record.names.len() > 1,
    }
}

/// Replaces the contents of `volume` with the tree found in the MFT read from `reader`.
pub fn load<R: Read + Seek>(
    index: &RwLock<Index>,
    volume: VolumeId,
    mut reader: R,
    progress: &Progress,
) -> io::Result<()> {
    let records = mft::read_all(&mut reader)?;
    let root = index
        .read()
        .volume(volume)
        .map(|v| v.root)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown Volume"))?;

    // (record index, name index) of children, by parent record number.
    let mut children: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
    for (r, record) in records.iter().enumerate() {
        if record.number == ROOT_RECORD {
            continue;
        }
        for (n, name) in record.names.iter().enumerate() {
            children.entry(name.parent).or_default().push((r, n));
        }
    }

    {
        let mut w = index.write();
        let old = w.children(root).to_vec();
        for id in old {
            w.remove(id);
        }
        if let Some(root_record) = records.iter().find(|r| r.number == ROOT_RECORD) {
            let mut meta = entry_meta(root_record);
            meta.hardlinked = false;
            w.update(root, meta);
        }
    }

    let mut queue: VecDeque<(u64, EntryId)> = VecDeque::from([(ROOT_RECORD, root)]);
    let mut visited: HashSet<u64> = HashSet::from([ROOT_RECORD]);
    while let Some((number, id)) = queue.pop_front() {
        let Some(kids) = children.get(&number) else {
            continue;
        };
        // Folders among the children are queued as they appear.
        let mut w = index.write();
        let mut inserted = 0usize;
        for &(r, n) in kids {
            let record = &records[r];
            let child = w.insert(id, &record.names[n].name, entry_meta(record));
            inserted += 1;
            if record.is_dir && visited.insert(record.number) {
                queue.push_back((record.number, child));
            }
            if inserted.is_multiple_of(BATCH) {
                drop(w);
                w = index.write();
            }
        }
        drop(w);
        progress
            .entries
            .fetch_add(inserted as u64, Ordering::Relaxed);
    }
    Ok(())
}
