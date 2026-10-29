use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;
use ve_core::{Capacity, EntryMeta, Index, SyncState};

const MINUTE: Duration = Duration::from_secs(60);

fn root() -> EntryMeta {
    EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    }
}

fn index_with(mounts: &[&str]) -> Index {
    let mut index = Index::new();
    for mount in mounts {
        index.add_volume(Path::new(mount), "ext4", mount, root(), SyncState::Walk);
    }
    index
}

#[test]
fn last_synced_and_capacity_survive_a_save() {
    let mut index = index_with(&["/a", "/b"]);
    let ids: Vec<_> = index.volumes().map(|(id, _)| id).collect();
    let capacity = Capacity { total: 1000, free: 250 };
    index.mark_synced(ids[0], 1_700_000_000, Some(capacity));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.save(&path).unwrap();

    let back = Index::load(&path).unwrap();
    let volumes: Vec<_> = back.volumes().map(|(_, v)| v).collect();
    assert_eq!(volumes[0].last_synced, Some(1_700_000_000));
    assert_eq!(volumes[0].capacity, Some(capacity));
    assert_eq!(volumes[1].last_synced, None);
    assert_eq!(volumes[1].capacity, None);
}

#[test]
fn a_v1_file_is_refused_as_needing_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    let mut v1 = b"VEINDEX\0".to_vec();
    v1.extend_from_slice(&1u32.to_le_bytes());
    v1.extend_from_slice(&[0xAB; 64]); // the old layout; never parsed
    std::fs::write(&path, v1).unwrap();
    let err = Index::load(&path).err().expect("v1 must not load");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(err.to_string().contains("needs rebuild"), "{err}");
}

#[test]
fn a_file_that_is_not_an_index_is_invalid_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    std::fs::write(&path, b"definitely not an index file").unwrap();
    assert_eq!(Index::load(&path).err().unwrap().kind(), ErrorKind::InvalidData);
}

#[test]
fn catch_up_is_needed_only_when_stale() {
    let mut index = index_with(&["/a"]);
    let id = index.volumes().next().unwrap().0;
    let now = 1_000_000;
    let volume = |index: &Index| index.volume(id).unwrap().clone();

    // Never synced: stale.
    assert!(volume(&index).needs_catch_up(now, 30 * MINUTE));

    index.mark_synced(id, now - 10 * 60, None);
    // Within the interval: no re-walk. At and beyond it: re-walk.
    assert!(!volume(&index).needs_catch_up(now, 30 * MINUTE));
    assert!(!volume(&index).needs_catch_up(now, 10 * MINUTE + Duration::from_secs(1)));
    assert!(volume(&index).needs_catch_up(now, 10 * MINUTE));
    assert!(volume(&index).needs_catch_up(now, 5 * MINUTE));

    // A Last Synced in the future means the clock moved back: don't trust it.
    index.mark_synced(id, now + 60, None);
    assert!(volume(&index).needs_catch_up(now, 30 * MINUTE));
}

#[test]
fn ntfs_volumes_always_catch_up() {
    let mut index = Index::new();
    let id = index.add_volume(
        Path::new("C:\\"),
        "NTFS",
        "guid:c",
        root(),
        SyncState::Ntfs {
            journal_id: 1,
            next_usn: 0,
        },
    );
    index.mark_synced(id, 1_000_000, None);
    assert!(index.volume(id).unwrap().needs_catch_up(1_000_000, 30 * MINUTE));
}

#[test]
fn the_overall_last_synced_is_the_oldest_volume() {
    let index = Index::new();
    assert_eq!(index.oldest_synced(), None);
    let mut index = index_with(&["/a", "/b"]);
    let ids: Vec<_> = index.volumes().map(|(id, _)| id).collect();
    index.mark_synced(ids[0], 500, None);
    // One Volume has never synced, so the app has not either.
    assert_eq!(index.oldest_synced(), Some(None));
    index.mark_synced(ids[1], 300, None);
    assert_eq!(index.oldest_synced(), Some(Some(300)));
}

#[test]
fn a_v2_file_is_refused_as_needing_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    let mut v2 = b"VEINDEX\0".to_vec();
    v2.extend_from_slice(&2u32.to_le_bytes());
    v2.extend_from_slice(&[0xAB; 64]);
    std::fs::write(&path, v2).unwrap();
    let err = Index::load(&path).err().expect("v2 must not load");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(err.to_string().contains("needs rebuild"), "{err}");
}

#[test]
fn identity_survives_a_save_and_a_remount_renames_the_root() {
    let mut index = index_with(&["/mnt/a"]);
    let id = index.volume_with_identity("/mnt/a").unwrap();
    index.set_mount(id, Some(Path::new("/run/media/a")));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.save(&path).unwrap();
    let back = Index::load(&path).unwrap();
    let id = back.volume_with_identity("/mnt/a").unwrap();
    let volume = back.volume(id).unwrap();
    assert_eq!(volume.mount, Path::new("/run/media/a"));
    assert_eq!(back.name(volume.root), "/run/media/a");
    assert!(!volume.online);
}

#[test]
fn a_v4_file_is_refused_as_needing_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    let mut v4 = b"VEINDEX\0".to_vec();
    v4.extend_from_slice(&4u32.to_le_bytes());
    v4.extend_from_slice(&[0xAB; 64]);
    std::fs::write(&path, v4).unwrap();
    let err = Index::load(&path).err().expect("v4 must not load");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(err.to_string().contains("needs rebuild"), "{err}");
}
