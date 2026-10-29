use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use ve_core::{EntryId, Filter, Index, VolumeId};
use ve_scan::Progress;
use ve_scan::SharedIndex;

/// Every Entry under a Volume root, keyed by its path relative to the root.
fn snapshot(index: &Index, volume: VolumeId) -> BTreeMap<String, (bool, u64, u64, i64)> {
    let root = index.volume(volume).unwrap().root;
    let mut out = BTreeMap::new();
    let mut stack: Vec<(EntryId, String)> = vec![(root, String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for &child in index.children(dir) {
            let path = format!("{prefix}/{}", index.name(child));
            let m = index.meta(child);
            // NTFS updates a folder's time in its parent's listing lazily, so a scan moments
            // apart can read an older one; folder times are compared only where that cannot happen.
            let modified = if m.is_dir && cfg!(windows) { 0 } else { m.modified };
            out.insert(path.clone(), (m.is_dir, m.logical, m.on_disk, modified));
            if m.is_dir {
                stack.push((child, path));
            }
        }
    }
    out
}

fn scan(root: &Path) -> (SharedIndex, VolumeId) {
    let index = SharedIndex::new(Index::new());
    let volume = ve_scan::add_volume(&index, root, "test", "uuid:test").unwrap();
    ve_scan::sync(&index, volume, &Progress::default()).unwrap();
    (index, volume)
}

fn write(path: &Path, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; len]).unwrap();
}

fn root_totals(index: &Index, volume: VolumeId) -> ve_core::Totals {
    let root = index.volume(volume).unwrap().root;
    Filter::compute(index, None).totals(index, root)
}

#[test]
fn first_sync_indexes_every_entry_with_its_sizes() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.txt"), 5);
    write(&dir.path().join("sub/b.bin"), 10_000);
    fs::create_dir_all(dir.path().join("sub/empty")).unwrap();

    let (index, volume) = scan(dir.path());
    let index = index.read();
    let entries = snapshot(&index, volume);
    let summary: Vec<(&str, bool, u64)> = entries.iter().map(|(p, e)| (p.as_str(), e.0, e.1)).collect();
    assert_eq!(
        summary,
        [
            ("/a.txt", false, 5),
            ("/sub", true, 0),
            ("/sub/b.bin", false, 10_000),
            ("/sub/empty", true, 0)
        ]
    );
    assert!(entries["/sub/b.bin"].2 >= 10_000, "On-disk Size covers the data");
    let totals = root_totals(&index, volume);
    assert_eq!((totals.logical, totals.files), (10_005, 2));
}

#[test]
fn a_hard_linked_file_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("one/data.bin"), 4096);
    fs::create_dir_all(dir.path().join("two")).unwrap();
    fs::hard_link(dir.path().join("one/data.bin"), dir.path().join("two/link.bin")).unwrap();

    let (index, volume) = scan(dir.path());
    let index = index.read();
    assert!(snapshot(&index, volume).contains_key("/two/link.bin"));
    let totals = root_totals(&index, volume);
    assert_eq!((totals.logical, totals.files), (4096, 1));
}

#[cfg(unix)]
#[test]
fn symlinks_are_indexed_but_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(&outside.path().join("big.bin"), 50_000);
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();

    let (index, volume) = scan(dir.path());
    let index = index.read();
    let entries = snapshot(&index, volume);
    assert_eq!(entries.keys().collect::<Vec<_>>(), ["/link"]);
    assert!(!entries["/link"].0, "a symlink is not a folder");
    assert!(root_totals(&index, volume).logical < 50_000);
}

/// Makes every kind of change Catch-up has to notice.
fn change_everything(root: &Path) {
    write(&root.join("new/added.txt"), 7);
    fs::remove_dir_all(root.join("doomed")).unwrap();
    // Appending changes a file without touching its folder's modification time.
    let mut grown = fs::OpenOptions::new().append(true).open(root.join("keep/grows.log")).unwrap();
    std::io::Write::write_all(&mut grown, &[b'y'; 3000]).unwrap();
    fs::rename(root.join("old-name"), root.join("keep/new-name")).unwrap();
    fs::remove_file(root.join("becomes-dir")).unwrap();
    write(&root.join("becomes-dir/inner.txt"), 11);
}

fn populate(root: &Path) {
    write(&root.join("keep/grows.log"), 100);
    write(&root.join("doomed/a/b.txt"), 20);
    write(&root.join("old-name/moved.txt"), 30);
    write(&root.join("becomes-dir"), 40);
}

#[test]
fn catch_up_brings_the_index_level_with_a_fresh_scan() {
    let dir = tempfile::tempdir().unwrap();
    populate(dir.path());
    let (index, volume) = scan(dir.path());

    change_everything(dir.path());
    ve_scan::sync(&index, volume, &Progress::default()).unwrap();

    let (fresh, fresh_volume) = scan(dir.path());
    assert_eq!(snapshot(&index.read(), volume), snapshot(&fresh.read(), fresh_volume));
    assert_eq!(root_totals(&index.read(), volume), root_totals(&fresh.read(), fresh_volume));
}

#[test]
fn catch_up_works_on_an_index_saved_and_loaded_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    let file = saved.path().join("index.bin");
    populate(dir.path());
    let (index, volume) = scan(dir.path());
    index.read().save(&file).unwrap();

    change_everything(dir.path());
    let loaded = SharedIndex::new(Index::load(&file).unwrap());
    ve_scan::sync(&loaded, volume, &Progress::default()).unwrap();

    let (fresh, fresh_volume) = scan(dir.path());
    assert_eq!(snapshot(&loaded.read(), volume), snapshot(&fresh.read(), fresh_volume));
}

#[cfg(unix)]
#[test]
fn unreadable_folders_are_kept_and_counted() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("locked/secret.txt"), 9);
    fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(dir.path().join("locked")).is_ok() {
        // Running as root: permissions are not enforced, nothing to check.
        fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let index = SharedIndex::new(Index::new());
    let volume = ve_scan::add_volume(&index, dir.path(), "test", "uuid:test").unwrap();
    let progress = Progress::default();
    ve_scan::sync(&index, volume, &progress).unwrap();
    fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(progress.unreadable(), 1);
    assert_eq!(snapshot(&index.read(), volume).keys().collect::<Vec<_>>(), ["/locked"]);
}
