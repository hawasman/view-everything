//! Remotes against a stand-in for `ssh`: a script that skips ssh's options and host and runs the
//! command locally (the real `ssh localhost` needs interactive host-key confirmation here), or
//! prints canned `find` output.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use ve_core::{Filter, Index, Remote};
use ve_scan::{Progress, SharedIndex, remote};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Runs the remote command locally: everything after `--` and the host.
const LOCAL: &str = r#"while [ "$1" != "--" ]; do shift; done; shift; shift; exec sh -c "$*""#;

fn add(index: &SharedIndex, path: &str) -> ve_core::VolumeId {
    let id = index.write().add_remote(&Remote::new("me@box", path).unwrap());
    assert!(index.read().volume(id).unwrap().is_remote());
    id
}

fn names(index: &Index, dir: ve_core::EntryId) -> Vec<String> {
    let mut v: Vec<_> = index.children(dir).iter().map(|&c| index.name(c).to_owned()).collect();
    v.sort();
    v
}

#[test]
fn captured_find_output_is_indexed_with_hard_links_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/find-output.bin");
    let ssh = script(dir.path(), "ssh", &format!("cat {}", fixture.display()));
    let index = SharedIndex::new(Index::new());
    let id = add(&index, "/data");
    let progress = Progress::default();
    remote::sync_with(ssh.to_str().unwrap(), &index, id, &progress).unwrap();

    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["a.txt", "docs", "empty", "link"]);
    let docs = index.child_named(root, "docs").unwrap();
    assert_eq!(names(&index, docs), ["big.bin", "sub", "tab\tname"]);
    let a = index.child_named(root, "a.txt").unwrap();
    assert_eq!((index.logical(a), index.on_disk(a)), (5, 4096));
    assert_eq!(index.modified(a), 1_704_164_645_678_901_234);
    assert_eq!(progress.entries(), 9);
    // big.bin has two names (inode 553036, 2 links): its 10000 bytes and 24 blocks count once.
    let big = index.child_named(docs, "big.bin").unwrap();
    assert!(index.is_hardlinked(big));
    let totals = Filter::compute(&index, None).totals(&index, root);
    assert_eq!(totals.logical, 5 + 5 + 1 + 10_000);
    assert_eq!(totals.on_disk, 4096 + 4096 + 24 * 512);
    assert_eq!(index.path(big).to_string_lossy(), "ssh:me@box:/data/docs/big.bin");
}

#[test]
fn a_real_find_is_indexed_and_a_refresh_applies_differences() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("a/b")).unwrap();
    fs::write(tree.join("a/b/f.txt"), vec![b'x'; 3000]).unwrap();
    fs::write(tree.join("keep.txt"), "k").unwrap();
    fs::write(tree.join("gone.txt"), "g").unwrap();
    fs::write(tree.join("a/odd 'name\".txt"), "q").unwrap();
    let ssh = script(dir.path(), "ssh", LOCAL);
    let ssh = ssh.to_str().unwrap();
    let index = SharedIndex::new(Index::new());
    let id = add(&index, tree.to_str().unwrap());
    remote::sync_with(ssh, &index, id, &Progress::default()).unwrap();
    let (keep, root) = {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        assert_eq!(names(&index, root), ["a", "gone.txt", "keep.txt"]);
        let a = index.child_named(root, "a").unwrap();
        assert_eq!(names(&index, a), ["b", "odd 'name\".txt"]);
        (index.child_named(root, "keep.txt").unwrap(), root)
    };

    fs::remove_file(tree.join("gone.txt")).unwrap();
    fs::remove_dir_all(tree.join("a/b")).unwrap();
    fs::write(tree.join("new.txt"), "n").unwrap();
    remote::sync_with(ssh, &index, id, &Progress::default()).unwrap();
    let index = index.read();
    assert_eq!(names(&index, root), ["a", "keep.txt", "new.txt"]);
    assert_eq!(names(&index, index.child_named(root, "a").unwrap()), ["odd 'name\".txt"]);
    // An Entry that is still there keeps its id.
    assert_eq!(index.child_named(root, "keep.txt"), Some(keep));
    assert_eq!(index.live_count(), 5);
}

#[test]
fn failures_are_reported_and_leave_the_index_alone() {
    let dir = tempfile::tempdir().unwrap();
    let index = SharedIndex::new(Index::new());
    let id = add(&index, "/data");
    let fail = |body: &str| {
        let ssh = script(dir.path(), "ssh-fail", body);
        remote::sync_with(ssh.to_str().unwrap(), &index, id, &Progress::default()).unwrap_err().to_string()
    };
    assert!(fail("echo 'me@box: Permission denied (publickey,password).' >&2; exit 255").contains("key or ssh-agent"));
    assert!(fail("echo \"find: unknown predicate \\`-printf'\" >&2; exit 1").contains("SFTP fallback (issue 16)"));
    assert!(remote::sync_with("/nonexistent/ssh", &index, id, &Progress::default()).unwrap_err().to_string().contains("not found"));
    assert_eq!(index.read().live_count(), 1);
}

#[test]
fn a_remote_survives_a_save_and_is_not_swept_up_as_an_offline_volume() {
    let index = SharedIndex::new(Index::new());
    let remote = Remote::new("me@box", "/srv/backup/").unwrap();
    assert_eq!(remote.path, "/srv/backup");
    assert_eq!(ve_scan::reconcile_remotes(&index, std::slice::from_ref(&remote)).len(), 1);
    // Already known: nothing to add.
    assert!(ve_scan::reconcile_remotes(&index, std::slice::from_ref(&remote)).is_empty());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.read().save(&path).unwrap();
    let loaded = SharedIndex::new(Index::load(&path).unwrap());
    // Volumes reconcile leaves Remotes alone; Remotes reconcile brings it back online.
    ve_scan::reconcile(&loaded, &[], &[]);
    ve_scan::reconcile_remotes(&loaded, std::slice::from_ref(&remote));
    let (_, v) = loaded.read().volumes().next().map(|(i, v)| (i, v.clone())).unwrap();
    assert!(v.online && v.is_remote() && v.identity == "ssh:me@box:/srv/backup");
    // Removed from the settings: gone from the Index.
    ve_scan::reconcile_remotes(&loaded, &[]);
    assert_eq!(loaded.read().volumes().count(), 0);
}

#[test]
fn a_manual_remote_is_never_caught_up_and_one_with_an_interval_is() {
    use std::time::Duration;
    let mut index = Index::new();
    let id = index.add_remote(&Remote::new("me@box", "/srv").unwrap());
    let v = index.volume(id).unwrap();
    assert!(!v.needs_catch_up(1_000_000, ve_core::MANUAL_REFRESH));
    assert!(v.needs_catch_up(1_000_000, Duration::from_secs(3600)));
    index.mark_synced(id, 999_000, None);
    assert!(!index.volume(id).unwrap().needs_catch_up(1_000_000, Duration::from_secs(3600)));
}
