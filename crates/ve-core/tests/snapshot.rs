use chrono::NaiveDate;
use std::path::Path;
use ve_core::snapshot::{self, Change, Snapshot};
use ve_core::{EntryId, EntryMeta, Index, SyncState};

fn folder() -> EntryMeta {
    EntryMeta {
        is_dir: true,
        ..EntryMeta::default()
    }
}

fn file(size: u64) -> EntryMeta {
    EntryMeta {
        logical: size,
        on_disk: size * 2,
        ..EntryMeta::default()
    }
}

fn day(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

/// /v with a (a.bin 10), b (c (c.bin 5)).
fn sample() -> (Index, EntryId, EntryId, EntryId) {
    let mut index = Index::new();
    let v = index.add_volume(Path::new("/v"), "ext4", "v", folder(), SyncState::Walk);
    let root = index.volume(v).unwrap().root;
    let a = index.insert(root, "a", folder());
    index.insert(a, "a.bin", file(10));
    let b = index.insert(root, "b", folder());
    let c = index.insert(b, "c", folder());
    index.insert(c, "c.bin", file(5));
    (index, root, a, c)
}

#[test]
fn a_snapshot_survives_a_save_and_names_no_full_paths() {
    let (index, ..) = sample();
    let snap = Snapshot::capture(&index);
    assert_eq!(snap.folders(), 4);
    let dir = tempfile::tempdir().unwrap();
    let path = snapshot::path_for(dir.path(), day("2026-10-29"));
    snap.save(&path).unwrap();
    assert_eq!(snapshot::list(dir.path()), vec![day("2026-10-29")]);
    let back = Snapshot::load(&path).unwrap();
    let same = back.compare(&index);
    assert!(same.removed_roots().is_empty());
    assert!(index.volumes().all(|(_, v)| same.change(&index, v.root) == Some(Change::default())));
    std::fs::write(&path, b"junk").unwrap();
    assert!(Snapshot::load(&path).is_err());
}

#[test]
fn the_comparison_reports_growth_new_and_removed_folders() {
    let (mut index, root, a, c) = sample();
    let snap = Snapshot::capture(&index);

    index.insert(a, "more.bin", file(7));
    let fresh = index.insert(root, "fresh", folder());
    index.insert(fresh, "f.bin", file(3));
    index.remove(c);
    let b = index.child_named(root, "b").unwrap();

    let cmp = snap.compare(&index);
    let grew = cmp.change(&index, a).unwrap();
    assert_eq!((grew.logical, grew.on_disk, grew.new), (7, 14, false));
    let new = cmp.change(&index, fresh).unwrap();
    assert_eq!((new.logical, new.new), (3, true));
    // b lost c's 5 bytes, and c is listed under it as removed; a file has no Change.
    assert_eq!(cmp.change(&index, b).unwrap().logical, -5);
    let gone = cmp.removed_in(b);
    assert_eq!((gone.len(), gone[0].name.as_str(), gone[0].logical), (1, "c", 5));
    assert!(cmp.change(&index, index.child_named(a, "a.bin").unwrap()).is_none());
    assert_eq!(cmp.change(&index, root).unwrap().logical, 7 + 3 - 5);
}

#[test]
fn a_removed_subtree_lists_only_its_topmost_folder() {
    let (mut index, root, ..) = sample();
    let snap = Snapshot::capture(&index);
    let b = index.child_named(root, "b").unwrap();
    index.remove(b);
    let cmp = snap.compare(&index);
    let gone = cmp.removed_in(root);
    assert_eq!(gone.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["b"]);
}

#[test]
fn a_volume_gone_since_the_snapshot_is_removed() {
    let (index, ..) = sample();
    let snap = Snapshot::capture(&index);
    let mut other = Index::new();
    other.add_volume(Path::new("/w"), "ext4", "w", folder(), SyncState::Walk);
    let cmp = snap.compare(&other);
    assert_eq!(cmp.removed_roots().len(), 1);
    assert_eq!(cmp.removed_roots()[0].name, "/v");
}

#[test]
fn a_cancelled_comparison_stops_with_nothing() {
    let mut index = Index::new();
    let v = index.add_volume(Path::new("/v"), "ext4", "v", folder(), SyncState::Walk);
    let root = index.volume(v).unwrap().root;
    for i in 0..600 {
        index.insert(root, &format!("d{i}"), folder());
    }
    let snap = Snapshot::capture(&index);
    assert!(snap.compare_with(|| &index, || true).is_none());
    assert!(snap.compare_with(|| &index, || false).is_some());
}

#[test]
fn retention_keeps_30_days_and_the_first_snapshot_of_each_month() {
    let today = day("2026-10-29");
    let mut dates: Vec<NaiveDate> = (0..30).map(|n| today - chrono::Duration::days(n)).collect();
    // Older: two in August (first is kept), one in July, 3 Sept and 20 Sept (two in a month).
    let old = ["2026-08-05", "2026-08-20", "2026-07-31", "2026-09-03", "2026-09-20"].map(day);
    dates.extend(old);
    let mut dropped = snapshot::expired(&dates, today);
    dropped.sort();
    assert_eq!(dropped, vec![day("2026-08-20"), day("2026-09-20")]);
    // Day 29 back is still kept; day 30 back is dropped unless it is its month's first.
    assert!(!snapshot::expired(&[today - chrono::Duration::days(29)], today).contains(&(today - chrono::Duration::days(29))));
    let both = [day("2026-09-28"), day("2026-09-29")];
    assert_eq!(snapshot::expired(&both, today), vec![day("2026-09-29")]);
}

#[test]
fn prune_deletes_only_what_retention_drops() {
    let dir = tempfile::tempdir().unwrap();
    let (index, ..) = sample();
    let snap = Snapshot::capture(&index);
    for d in ["2026-08-05", "2026-08-20", "2026-10-29"] {
        snap.save(&snapshot::path_for(dir.path(), day(d))).unwrap();
    }
    std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
    snapshot::prune(dir.path(), day("2026-10-29"));
    assert_eq!(snapshot::list(dir.path()), vec![day("2026-10-29"), day("2026-08-05")]);
    assert!(dir.path().join("notes.txt").exists());
}

/// Size and speed on an Index shaped like a real one (400k folders, 6M files, path-like names):
/// `cargo test -p ve-core --release measure_synthetic -- --ignored --nocapture`
#[test]
#[ignore]
fn measure_synthetic_index_snapshot_size() {
    let mut index = Index::new();
    let v = index.add_volume(Path::new("/"), "ext4", "root", folder(), SyncState::Walk);
    let mut dirs = vec![index.volume(v).unwrap().root];
    let mut x = 12345u64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for i in 0..400_000 {
        let parent = dirs[(rnd() % dirs.len() as u64) as usize];
        let name = format!(
            "{}-{}",
            ["src", "node_modules", "target", "Documents", "cache", "photos"][(rnd() % 6) as usize],
            i
        );
        let d = index.insert(parent, &name, folder());
        dirs.push(d);
    }
    for i in 0..6_000_000 {
        let parent = dirs[(rnd() % dirs.len() as u64) as usize];
        index.insert(parent, &format!("file{i}.dat"), file(rnd() % 5_000_000));
    }
    let t = std::time::Instant::now();
    let s = Snapshot::capture(&index);
    let captured = t.elapsed();
    let out = std::env::temp_dir().join("ve-measure.snap");
    let t = std::time::Instant::now();
    s.save(&out).unwrap();
    println!(
        "folders {} capture {captured:?} save {:?} bytes {}",
        s.folders(),
        t.elapsed(),
        std::fs::metadata(&out).unwrap().len()
    );
    let _ = std::fs::remove_file(out);
}
