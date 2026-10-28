//! Windows: full MFT load from a volume handle, then Catch-up through the USN change journal.

use super::{load, mft};
use crate::Progress;
use crate::platform::windows::is_elevated;
use mft::{ROOT_RECORD, Record};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use ve_core::{EntryId, EntryMeta, Index, SyncState, VolumeId};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_CREATE_USN_JOURNAL, FSCTL_GET_NTFS_FILE_RECORD, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL,
};

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_JOURNAL_NOT_ACTIVE: i32 = 1179;
const ERROR_JOURNAL_ENTRY_DELETED: i32 = 1181;
const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
const JOURNAL_BUFFER: usize = 1 << 20;

struct Journal {
    id: u64,
    first_usn: i64,
    next_usn: i64,
}

fn ioctl(handle: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: the pointers and lengths describe the live `input` and `output` slices.
    let ok = unsafe {
        DeviceIoControl(
            handle.as_raw_handle() as HANDLE,
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(returned as usize)
    }
}

fn query_journal(handle: &File) -> io::Result<Journal> {
    let mut out = [0u8; 80];
    let n = ioctl(handle, FSCTL_QUERY_USN_JOURNAL, &[], &mut out)?;
    if n < 24 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short USN journal reply",
        ));
    }
    Ok(Journal {
        id: u64::from_le_bytes(out[0..8].try_into().unwrap()),
        first_usn: i64::from_le_bytes(out[8..16].try_into().unwrap()),
        next_usn: i64::from_le_bytes(out[16..24].try_into().unwrap()),
    })
}

/// Queries the journal, creating it when the volume has none.
fn ensure_journal(handle: &File) -> io::Result<Journal> {
    match query_journal(handle) {
        Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_NOT_ACTIVE) => {
            let mut input = [0u8; 16];
            input[0..8].copy_from_slice(&(32u64 << 20).to_le_bytes());
            input[8..16].copy_from_slice(&(8u64 << 20).to_le_bytes());
            ioctl(handle, FSCTL_CREATE_USN_JOURNAL, &input, &mut [])?;
            query_journal(handle)
        }
        other => other,
    }
}

/// Record numbers named by journal records from `start`, and the USN to resume from.
fn read_changes(handle: &File, journal: &Journal, start: i64) -> io::Result<(Vec<u64>, i64)> {
    let mut changed = Vec::new();
    let mut seen = HashSet::new();
    let mut next = start;
    let mut buf = vec![0u8; JOURNAL_BUFFER];
    while next < journal.next_usn {
        let mut input = [0u8; 40];
        input[0..8].copy_from_slice(&next.to_le_bytes());
        input[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); // every reason
        // ReturnOnlyOnClose, Timeout and BytesToWaitFor stay 0: return what exists now.
        input[32..40].copy_from_slice(&journal.id.to_le_bytes());
        let n = ioctl(handle, FSCTL_READ_USN_JOURNAL, &input, &mut buf)?;
        if n < 8 {
            break;
        }
        let reply_next = i64::from_le_bytes(buf[0..8].try_into().unwrap());
        let mut pos = 8;
        while pos + 8 <= n {
            let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            if len < 8 || pos + len > n {
                break;
            }
            let major = u16::from_le_bytes(buf[pos + 4..pos + 6].try_into().unwrap());
            // V2 and V3 both start the file reference at offset 8; V3's is 128 bits wide on
            // ReFS but NTFS keeps the 64-bit reference in the low half.
            if matches!(major, 2 | 3) && len >= 16 {
                let reference = u64::from_le_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
                let number = mft::record_number(reference);
                if seen.insert(number) {
                    changed.push(number);
                }
            }
            pos += len;
        }
        if reply_next <= next {
            break;
        }
        next = reply_next;
    }
    Ok((changed, next.max(start)))
}

/// Fetches record `number` through the volume's own record API. `None` means it is gone.
fn fetch_record(handle: &File, number: u64) -> io::Result<Option<Vec<u8>>> {
    let mut out = vec![0u8; 16 + 4096 * 4];
    let n = match ioctl(
        handle,
        FSCTL_GET_NTFS_FILE_RECORD,
        &(number as i64).to_le_bytes(),
        &mut out,
    ) {
        Ok(n) => n,
        // Past the end of the MFT, or a freed record.
        Err(e) if matches!(e.raw_os_error(), Some(2 | 87 | 1392 | 38)) => return Ok(None),
        Err(e) => return Err(e),
    };
    if n < 12 {
        return Ok(None);
    }
    let returned = mft::record_number(u64::from_le_bytes(out[0..8].try_into().unwrap()));
    let len = u32::from_le_bytes(out[8..12].try_into().unwrap()) as usize;
    if returned != number || 12 + len > n {
        return Ok(None); // the API yields the next lower in-use record when this one is free
    }
    Ok(Some(out[12..12 + len].to_vec()))
}

/// One changed file, merged across its extension records; `None` when deleted.
fn current_record(
    handle: &File,
    geometry: &mft::Geometry,
    number: u64,
    extra: &mut Vec<u64>,
) -> io::Result<Option<Record>> {
    let parse = |number: u64| -> io::Result<Option<mft::Parsed>> {
        Ok(fetch_record(handle, number)?
            .and_then(|mut raw| mft::parse_record(number, &mut raw, false, false)))
    };
    let Some(base) = parse(number)? else {
        return Ok(None);
    };
    if base.in_use && base.base != 0 && base.base != number {
        extra.push(base.base); // an extension record changed: the base file is what changed
        return Ok(None);
    }
    let mut reader = handle.try_clone()?;
    mft::finish_record(base, &mut reader, geometry, parse)
}

fn volume_path(mount: &std::path::Path) -> Option<String> {
    let text = mount.to_string_lossy();
    let drive = text.trim_end_matches(['\\', '/']);
    let mut chars = drive.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => {
            Some(format!(r"\\.\{drive}"))
        }
        _ => None,
    }
}

fn key_map(index: &Index, root: EntryId) -> HashMap<u64, Vec<EntryId>> {
    let mut map: HashMap<u64, Vec<EntryId>> = HashMap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if id != root {
            map.entry(index.key(id)).or_default().push(id);
        }
        stack.extend_from_slice(index.children(id));
    }
    map.insert(ROOT_RECORD, vec![root]);
    map
}

fn live(index: &Index, map: &HashMap<u64, Vec<EntryId>>, number: u64) -> Vec<EntryId> {
    map.get(&number)
        .map(|ids| ids.iter().copied().filter(|&i| index.is_live(i)).collect())
        .unwrap_or_default()
}

/// Applies the current state of one file. Returns false when a parent is not in the Index yet.
fn apply(
    index: &mut Index,
    map: &mut HashMap<u64, Vec<EntryId>>,
    number: u64,
    record: Option<&Record>,
) -> bool {
    let existing = live(index, map, number);
    let Some(record) = record else {
        for id in existing {
            index.remove(id);
        }
        map.remove(&number);
        return true;
    };
    if number == ROOT_RECORD {
        index.update(existing[0], EntryMeta::from(record));
        return true;
    }
    let meta = EntryMeta::from(record);
    // A file cannot turn into a folder in place: replace it.
    let (mut existing, replaced): (Vec<EntryId>, Vec<EntryId>) = existing
        .into_iter()
        .partition(|&id| index.is_dir(id) == record.is_dir);
    for id in replaced {
        index.remove(id);
    }
    let mut parents = Vec::with_capacity(record.names.len());
    for name in &record.names {
        match live(index, map, name.parent).first() {
            Some(&p) if index.is_dir(p) => parents.push(p),
            _ => return false,
        }
    }
    // Keep Entries whose (parent, name) still exists.
    let mut wanted: Vec<(EntryId, &str)> = record
        .names
        .iter()
        .zip(&parents)
        .map(|(n, &p)| (p, n.name.as_str()))
        .collect();
    let mut stale = Vec::new();
    for id in existing.drain(..) {
        let here = (index.parent(id), index.name(id).to_owned());
        match wanted
            .iter()
            .position(|&(p, n)| Some(p) == here.0 && n == here.1)
        {
            Some(pos) => {
                wanted.swap_remove(pos);
                index.update(id, meta);
            }
            None => stale.push(id),
        }
    }
    let mut ids: Vec<EntryId> = Vec::new();
    // Pair leftover Entries with leftover names as renames; extras are removed or inserted.
    for (parent, name) in wanted {
        match stale.pop() {
            Some(id) => {
                index.rename(id, parent, name);
                index.update(id, meta);
                ids.push(id);
            }
            None => ids.push(index.insert(parent, name, meta)),
        }
    }
    for id in stale {
        index.remove(id);
    }
    let all = map.entry(number).or_default();
    all.retain(|&id| index.is_live(id));
    all.extend(ids);
    true
}

fn catch_up(
    index: &RwLock<Index>,
    handle: &File,
    root: EntryId,
    journal: &Journal,
    from: i64,
) -> io::Result<Option<i64>> {
    let (mut changed, next) = read_changes(handle, journal, from)?;
    if changed.is_empty() {
        return Ok(Some(next));
    }
    let geometry = mft::read_geometry(&mut handle.try_clone()?)?;
    let mut work: Vec<(u64, Option<Record>)> = Vec::new();
    let mut done = HashSet::new();
    while let Some(number) = changed.pop() {
        if done.insert(number) {
            let mut extra = Vec::new();
            let record = current_record(handle, &geometry, number, &mut extra)?;
            changed.extend(extra);
            work.push((number, record));
        }
    }
    let mut w = index.write();
    let mut map = key_map(&w, root);
    // New folders may be listed after their children; retry until nothing more applies.
    loop {
        let before = work.len();
        work.retain(|(number, record)| !apply(&mut w, &mut map, *number, record.as_ref()));
        if work.is_empty() || work.len() == before {
            break;
        }
    }
    // A change whose parent is unknown cannot be applied; the caller reloads instead of
    // advancing past it.
    Ok(work.is_empty().then_some(next))
}

/// Brings an NTFS Volume up to date. `Ok(false)` leaves the Volume to the portable walk.
pub(crate) fn sync_windows(
    index: &RwLock<Index>,
    volume: VolumeId,
    progress: &Progress,
) -> io::Result<bool> {
    let (root, mount, state) = {
        let r = index.read();
        let Some(v) = r.volume(volume) else {
            return Ok(false);
        };
        if !v.fs_type.eq_ignore_ascii_case("NTFS") {
            return Ok(false);
        }
        (v.root, v.mount.clone(), v.sync.clone())
    };
    if !is_elevated() {
        return Ok(false);
    }
    let Some(path) = volume_path(&mount) else {
        return Ok(false);
    };
    let handle = match OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .open(&path)
    {
        Ok(h) => h,
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => return Ok(false),
        Err(e) => return Err(e),
    };
    let journal = match ensure_journal(&handle) {
        Ok(j) => j,
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => return Ok(false),
        Err(e) => return Err(e),
    };

    if let SyncState::Ntfs {
        journal_id,
        next_usn,
    } = state
        && journal_id == journal.id
        && journal.first_usn <= next_usn
    {
        if next_usn >= journal.next_usn {
            return Ok(true); // nothing changed since the last sync
        }
        match catch_up(index, &handle, root, &journal, next_usn) {
            Ok(Some(next)) => {
                index.write().set_sync_state(
                    volume,
                    SyncState::Ntfs {
                        journal_id: journal.id,
                        next_usn: next,
                    },
                );
                return Ok(true);
            }
            Ok(None) => {}
            Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_ENTRY_DELETED) => {}
            Err(e) => return Err(e),
        }
    }

    // Full load. The journal position is taken first so changes made during the load replay.
    let reader = handle.try_clone()?;
    load(index, volume, reader, progress)?;
    index.write().set_sync_state(
        volume,
        SyncState::Ntfs {
            journal_id: journal.id,
            next_usn: journal.next_usn,
        },
    );
    Ok(true)
}
