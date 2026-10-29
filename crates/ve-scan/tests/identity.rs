use std::fs;
use std::path::Path;
use ve_core::{Filter, Index, VolumeId};
use ve_scan::{Progress, SharedIndex, VolumeInfo};

fn info(mount: &Path, identity: &str) -> VolumeInfo {
    VolumeInfo {
        mount: mount.to_path_buf(),
        identity: identity.to_owned(),
        fs_type: "test".into(),
        label: String::new(),
        removable: true,
        network: false,
        read_only: false,
    }
}

fn sync(index: &SharedIndex, id: VolumeId) {
    ve_scan::sync(index, id, &Progress::default()).unwrap();
}

/// A Volume with identity `identity` holding a.txt (5 bytes) and sub/b.bin (10 000 bytes),
/// scanned, saved, and loaded again as at the next launch (so it starts offline).
fn indexed_then_restarted(root: &Path, identity: &str) -> SharedIndex {
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "hello").unwrap();
    fs::write(root.join("sub/b.bin"), vec![0u8; 10_000]).unwrap();
    let index = SharedIndex::new(Index::new());
    let (added, failed) = ve_scan::reconcile(&index, &[info(root, identity)], &[info(root, identity)]);
    assert!(failed.is_empty());
    sync(&index, added[0]);
    index.write().mark_synced(added[0], 1_700_000_000, None);
    // Identities hold `:`, which Windows file names cannot.
    let file = root.parent().unwrap().join(format!("{}.bin", identity.replace(':', "-")));
    index.read().save(&file).unwrap();
    SharedIndex::new(Index::load(&file).unwrap())
}

fn total(index: &SharedIndex, id: VolumeId) -> u64 {
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    Filter::compute(&index, None).totals(&index, root).on_disk
}

#[test]
fn a_remount_at_another_path_is_the_same_volume_and_only_catches_up() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, new) = (tmp.path().join("media-a"), tmp.path().join("media-b"));
    let index = indexed_then_restarted(&old, "uuid:1234");
    let id = index.read().volume_with_identity("uuid:1234").unwrap();
    assert!(!index.read().volume(id).unwrap().online, "a loaded Volume is offline until found");
    let before = index.read().live_count();
    let a_txt = index.read().find_path(&old.join("a.txt")).unwrap();

    // Unplugged, plugged in elsewhere, and changed meanwhile.
    fs::rename(&old, &new).unwrap();
    fs::write(new.join("c.txt"), "new").unwrap();
    let mounted = [info(&new, "uuid:1234")];
    let (added, failed) = ve_scan::reconcile(&index, &mounted, &mounted);
    assert!(failed.is_empty());
    assert_eq!(added, [id], "the same Volume, queued to catch up rather than added anew");
    assert_eq!(index.read().volumes().count(), 1, "no new Volume, so no scan from scratch");
    {
        let index = index.read();
        let volume = index.volume(id).unwrap();
        assert!(volume.online && volume.mount == new);
        assert_eq!(index.live_count(), before, "Entries kept");
        assert_eq!(index.path(a_txt), new.join("a.txt"));
        assert_eq!(index.find_path(&new.join("sub/b.bin")).map(|e| index.logical(e)), Some(10_000));
    }

    sync(&index, id);
    let index = index.read();
    assert_eq!(index.live_count(), before + 1);
    assert_eq!(
        index.find_path(&new.join("a.txt")),
        Some(a_txt),
        "unchanged Entries are not replaced"
    );
    assert!(index.find_path(&new.join("c.txt")).is_some());
    assert_eq!(index.volumes().count(), 1);
}

#[test]
fn a_different_identity_at_the_same_path_is_a_different_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("media");
    let index = indexed_then_restarted(&path, "uuid:old-stick");
    let mounted = [info(&path, "uuid:other-stick")];
    let (added, _) = ve_scan::reconcile(&index, &mounted, &mounted);
    assert_eq!(added.len(), 1);
    let index = index.read();
    assert_eq!(index.volumes().count(), 2);
    // The first stays, offline: it is not what is mounted at that path now.
    let old = index.volume(index.volume_with_identity("uuid:old-stick").unwrap()).unwrap();
    assert!(!old.online);
}

#[test]
fn an_unplugged_volume_stays_offline_searchable_and_sized() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("media");
    let index = indexed_then_restarted(&path, "uuid:1234");
    let id = index.read().volume_with_identity("uuid:1234").unwrap();
    let size = total(&index, id);
    assert!(size >= 10_000);

    // Launch with nothing mounted, then again after a settings change.
    for _ in 0..2 {
        let (added, failed) = ve_scan::reconcile(&index, &[], &[]);
        assert!(added.is_empty() && failed.is_empty());
        let index = index.read();
        assert!(!index.volume(id).unwrap().online);
        assert!(index.find_path(&path.join("sub/b.bin")).is_some());
    }
    assert_eq!(total(&index, id), size);
    assert_eq!(index.read().volume(id).unwrap().last_synced, Some(1_700_000_000));
}

#[test]
fn a_mounted_volume_the_user_unticked_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("media");
    let index = indexed_then_restarted(&path, "uuid:1234");
    // Mounted but no longer wanted: removed.
    ve_scan::reconcile(&index, &[info(&path, "uuid:1234")], &[]);
    assert_eq!(index.read().volumes().count(), 0);
    assert_eq!(index.read().live_count(), 0);
}

#[test]
fn forgetting_an_offline_volume_removes_it_from_the_index_for_good() {
    let tmp = tempfile::tempdir().unwrap();
    let keep = tmp.path().join("keep");
    let gone = tmp.path().join("gone");
    let index = indexed_then_restarted(&gone, "uuid:gone");
    // Add a second Volume that stays.
    fs::create_dir_all(&keep).unwrap();
    fs::write(keep.join("k.txt"), "k").unwrap();
    let mounted = [info(&keep, "uuid:keep")];
    let (added, _) = ve_scan::reconcile(&index, &mounted, &mounted);
    sync(&index, added[0]);
    let keep_entries = index.read().live_count();

    let mut write = index.write();
    let id = write.volume_with_identity("uuid:gone").unwrap();
    write.remove_volume(id);
    drop(write);
    {
        let index = index.read();
        assert!(index.volume_with_identity("uuid:gone").is_none());
        assert!(index.find_path(&gone.join("a.txt")).is_none());
        assert!(index.live_count() < keep_entries);
        assert_eq!(index.volumes().count(), 1);
    }
    let file = tmp.path().join("after.bin");
    index.read().save(&file).unwrap();
    let back = Index::load(&file).unwrap();
    assert!(back.volume_with_identity("uuid:gone").is_none());
    assert!(back.find_path(&keep.join("k.txt")).is_some());
}
