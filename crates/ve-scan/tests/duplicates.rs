use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use ve_core::{Index, Remote};
use ve_scan::duplicates::{HashCache, HashProgress, find};
use ve_scan::{Progress, SharedIndex, VolumeInfo};

fn scanned(root: &Path) -> SharedIndex {
    let info = VolumeInfo {
        mount: root.to_path_buf(),
        identity: "uuid:dupes".into(),
        fs_type: "test".into(),
        label: String::new(),
        removable: true,
        network: false,
        read_only: false,
    };
    let index = SharedIndex::new(Index::new());
    let (added, failed) = ve_scan::reconcile(&index, std::slice::from_ref(&info), std::slice::from_ref(&info));
    assert!(failed.is_empty());
    ve_scan::sync(&index, added[0], &Progress::default()).unwrap();
    index
}

fn run(index: &SharedIndex, cache: &mut HashCache) -> (Vec<ve_scan::duplicates::Group>, ve_scan::duplicates::Stats) {
    find(index, cache, 1, &HashProgress::default(), &AtomicBool::new(false))
}

fn body(byte: u8, len: usize) -> Vec<u8> {
    vec![byte; len]
}

#[test]
fn staging_narrows_by_size_then_partial_then_full() {
    let tmp = tempfile::tempdir().unwrap();
    let big = 300 * 1024;
    fs::write(tmp.path().join("a.bin"), body(1, big)).unwrap();
    fs::write(tmp.path().join("b.bin"), body(1, big)).unwrap();
    // Same size, differs at the start: stopped by the partial stage.
    let mut head = body(1, big);
    head[0] = 9;
    fs::write(tmp.path().join("head.bin"), head).unwrap();
    // Same size and ends, differs in the middle: only the full hash tells it apart.
    let mut middle = body(1, big);
    middle[big / 2] = 9;
    fs::write(tmp.path().join("middle.bin"), middle).unwrap();
    // Unique size: never read.
    fs::write(tmp.path().join("alone.bin"), body(1, big + 1)).unwrap();

    let index = scanned(tmp.path());
    let (groups, stats) = run(&index, &mut HashCache::default());
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].files.len(), 2);
    assert_eq!(groups[0].wasted(), big as u64);
    assert_eq!(stats.partial_hashed, 4, "alone.bin is ruled out by size alone");
    assert_eq!(stats.full_hashed, 3, "head.bin is ruled out by the partial hash");
}

#[test]
fn a_second_run_reuses_the_cache_and_a_changed_file_is_rehashed() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.bin"), body(1, 200_000)).unwrap();
    fs::write(tmp.path().join("b.bin"), body(1, 200_000)).unwrap();
    let index = scanned(tmp.path());
    let mut cache = HashCache::default();
    let (first, _) = run(&index, &mut cache);
    assert_eq!(first.len(), 1);

    // Through hashes.bin, as between launches.
    let file = tmp.path().parent().unwrap().join("hashes-test.bin");
    cache.save(&file).unwrap();
    let mut cache = HashCache::load(&file);
    let (again, stats) = run(&index, &mut cache);
    assert_eq!(again, first);
    assert_eq!((stats.partial_hashed, stats.full_hashed), (0, 0));
    assert_eq!(stats.cache_hits, 4);

    // b.bin changes content (same size) and the Index notices: its cached hash is not reused.
    fs::write(tmp.path().join("b.bin"), body(2, 200_000)).unwrap();
    let id = index.read().find_path(&tmp.path().join("b.bin")).unwrap();
    let mut meta = index.read().meta(id);
    meta.modified += 1_000_000_000;
    index.write().update(id, meta);
    let (changed, stats) = run(&index, &mut cache);
    assert!(changed.is_empty(), "the changed file is no longer a Duplicate");
    assert_eq!(stats.partial_hashed, 1);
    assert_eq!(stats.cache_hits, 1);
}

#[test]
fn hard_links_are_not_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.bin"), body(1, 10_000)).unwrap();
    fs::hard_link(tmp.path().join("a.bin"), tmp.path().join("link.bin")).unwrap();
    let index = scanned(tmp.path());
    let (groups, stats) = run(&index, &mut HashCache::default());
    assert!(groups.is_empty());
    assert_eq!(stats.partial_hashed, 0, "one file, so nothing to compare");

    // A real copy beside the links is a Duplicate of the file, once.
    fs::write(tmp.path().join("copy.bin"), body(1, 10_000)).unwrap();
    let index = scanned(tmp.path());
    let (groups, _) = run(&index, &mut HashCache::default());
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].files.len(), 2);
}

#[test]
fn remotes_and_offline_volumes_are_never_hashed() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.bin"), body(1, 10_000)).unwrap();
    fs::write(tmp.path().join("b.bin"), body(1, 10_000)).unwrap();
    let index = scanned(tmp.path());
    let id = index.read().volume_with_identity("uuid:dupes").unwrap();

    // The same two files under a Remote: the paths could be read, but must not be.
    let remote = Remote::new("example.invalid", "/data").unwrap();
    {
        let mut index = index.write();
        let rid = index.add_remote(&remote);
        let root = index.volume(rid).unwrap().root;
        for name in ["x.bin", "y.bin"] {
            let meta = ve_core::EntryMeta {
                logical: 10_000,
                ..Default::default()
            };
            index.insert(root, name, meta);
        }
    }
    let (groups, stats) = run(&index, &mut HashCache::default());
    assert_eq!(groups.len(), 1);
    assert_eq!(stats.partial_hashed, 2, "only the local pair was read");

    index.write().set_mount(id, None);
    let (groups, stats) = run(&index, &mut HashCache::default());
    assert!(groups.is_empty());
    assert_eq!(stats.partial_hashed, 0);
}

/// A file swapped for a symlink or a FIFO after it was indexed is never opened through: the
/// symlink is not followed (its target would pass for a copy) and the FIFO would block the
/// open for ever.
#[cfg(unix)]
#[test]
fn symlinks_and_fifos_are_never_opened() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    for name in ["a.bin", "link.bin", "fifo.bin"] {
        fs::write(tmp.path().join(name), body(1, 200 * 1024)).unwrap();
    }
    let index = scanned(tmp.path());
    fs::remove_file(tmp.path().join("link.bin")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("a.bin"), tmp.path().join("link.bin")).unwrap();
    fs::remove_file(tmp.path().join("fifo.bin")).unwrap();
    let fifo = std::ffi::CString::new(tmp.path().join("fifo.bin").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let (done, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (groups, _) = run(&index, &mut HashCache::default());
        done.send(groups).ok();
    });
    let groups = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("hashing blocked on a FIFO");
    assert!(groups.is_empty(), "a symlink or FIFO was read as a copy: {groups:?}");
}

#[test]
fn a_file_that_grew_after_it_was_indexed_is_not_a_duplicate() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.bin"), body(1, 200 * 1024)).unwrap();
    fs::write(tmp.path().join("b.bin"), body(1, 200 * 1024)).unwrap();
    let index = scanned(tmp.path());
    fs::write(tmp.path().join("b.bin"), body(1, 300 * 1024)).unwrap();
    let (groups, _) = run(&index, &mut HashCache::default());
    assert!(groups.is_empty());
}
