//! Remotes against a stand-in for `ssh`: a script that skips ssh's options and host and runs the
//! command locally (the real `ssh localhost` needs interactive host-key confirmation here), or
//! prints canned `find` output. The stand-in runs the local `find` and `df`, so these need GNU
//! find (`-printf`): Linux only.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use ve_core::{Filter, Index, Remote};
use ve_scan::{Progress, SharedIndex, remote};

/// Tests that write a shell script and then run it hold this for their whole run. Otherwise a
/// fork for another test's `ssh` can inherit the still-open write handle of a script being
/// written here, and running that script fails with ETXTBSY ("Text file busy").
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    let _serial = serial();
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
    let _serial = serial();
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
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let index = SharedIndex::new(Index::new());
    let id = add(&index, "/data");
    let fail = |body: &str| {
        let ssh = script(dir.path(), "ssh-fail", body);
        remote::sync_with(ssh.to_str().unwrap(), &index, id, &Progress::default())
            .unwrap_err()
            .to_string()
    };
    assert!(fail("echo 'me@box: Permission denied (publickey,password).' >&2; exit 255").contains("key or ssh-agent"));
    // No GNU find, and the server then has no SFTP either: the fallback's failure is reported.
    assert!(fail("echo \"find: unknown predicate \\`-printf'\" >&2; exit 1").contains("SFTP on me@box failed"));
    assert!(
        remote::sync_with("/nonexistent/ssh", &index, id, &Progress::default())
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
    assert_eq!(index.read().live_count(), 1);
}

#[test]
fn df_over_the_same_ssh_gives_the_remote_a_capacity_and_a_failing_df_clears_it() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("a.txt"), "a").unwrap();
    let ssh = script(dir.path(), "ssh", LOCAL);
    let no_df = script(
        dir.path(),
        "ssh-no-df",
        &format!("case \"$*\" in *\"df -kP\"*) echo 'sh: df: not found' >&2; exit 127;; esac\n{LOCAL}"),
    );
    let index = SharedIndex::new(Index::new());
    let id = add(&index, tree.to_str().unwrap());
    assert!(index.read().volume(id).unwrap().capacity.is_none());
    remote::sync_with(ssh.to_str().unwrap(), &index, id, &Progress::default()).unwrap();
    let c = index.read().volume(id).unwrap().capacity.expect("df ran");
    // The same filesystem a local statvfs measures.
    let local = ve_scan::capacity(&tree).unwrap();
    assert_eq!(c.total, local.total);
    assert!(c.free <= c.total);
    // df failing leaves the listing in place and the lane without capacity.
    remote::sync_with(no_df.to_str().unwrap(), &index, id, &Progress::default()).unwrap();
    let index = index.read();
    assert!(index.volume(id).unwrap().capacity.is_none());
    assert_eq!(index.live_count(), 2);
}

#[test]
fn a_root_that_is_a_symlink_is_listed_through() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("a.txt"), "a").unwrap();
    let link = dir.path().join("root-link");
    std::os::unix::fs::symlink(&tree, &link).unwrap();
    let ssh = script(dir.path(), "ssh", LOCAL);
    let index = SharedIndex::new(Index::new());
    let id = add(&index, link.to_str().unwrap());
    remote::sync_with(ssh.to_str().unwrap(), &index, id, &Progress::default()).unwrap();
    let index = index.read();
    assert_eq!(names(&index, index.volume(id).unwrap().root), ["a.txt"]);
}

#[test]
fn a_remote_survives_a_save_and_is_not_swept_up_as_an_offline_volume() {
    let index = SharedIndex::new(Index::new());
    let remote = Remote::new("me@box", "/srv/backup/").unwrap();
    assert_eq!(remote.identity(), "ssh:me@box:/srv/backup");
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

#[test]
fn a_flood_of_stderr_does_not_hang_the_sync() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let ssh = script(dir.path(), "ssh", "head -c 300000 /dev/zero | tr '\\0' 'e' >&2; exit 255");
    let index = SharedIndex::new(Index::new());
    let id = add(&index, "/data");
    assert!(remote::sync_with(ssh.to_str().unwrap(), &index, id, &Progress::default()).is_err());
}

#[test]
fn an_incomplete_listing_does_not_prune() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("a.txt"), "a").unwrap();
    fs::write(tree.join("b.txt"), "b").unwrap();
    let ssh = script(dir.path(), "ssh", LOCAL);
    let ssh = ssh.to_str().unwrap();
    let index = SharedIndex::new(Index::new());
    let id = add(&index, tree.to_str().unwrap());
    remote::sync_with(ssh, &index, id, &Progress::default()).unwrap();
    // find exits 1 (unreadable folder) with a partial listing: b.txt is not mentioned.
    let partial = script(
        dir.path(),
        "ssh-partial",
        "printf '1\\t1\\t1\\t8\\t1.0\\tf\\ta.txt\\0'; echo 'find: x: Permission denied' >&2; exit 1",
    );
    remote::sync_with(partial.to_str().unwrap(), &index, id, &Progress::default()).unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["a.txt", "b.txt"]);
}

/// The system's `sftp-server`, if installed; the tests below are skipped without it.
fn sftp_server() -> Option<&'static str> {
    [
        "/usr/lib/ssh/sftp-server",
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/libexec/sftp-server",
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
}

/// A server with BusyBox's `find` (no -printf) and an SFTP subsystem that is the real
/// `sftp-server`; each find attempt is logged to `$LOG`.
fn busybox_ssh(dir: &Path, server: &str) -> PathBuf {
    let log = dir.join("find.log");
    script(
        dir,
        "ssh",
        &format!(
            "for a; do [ \"$a\" = -s ] && exec {server}; done\ncase \"$*\" in *\"df -kP\"*) {LOCAL_CMD};; esac\necho find >> {log}\necho 'find: unrecognized: -printf' >&2; exit 1",
            log = log.display(),
            LOCAL_CMD = LOCAL
        ),
    )
}

#[test]
fn a_server_without_gnu_find_is_indexed_over_sftp_and_remembered() {
    let _serial = serial();
    let Some(server) = sftp_server() else {
        eprintln!("skipped: no sftp-server installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("a/b")).unwrap();
    fs::create_dir_all(tree.join("many")).unwrap();
    fs::write(tree.join("a/b/f.txt"), vec![b'x'; 3000]).unwrap();
    fs::write(tree.join("keep.txt"), "k").unwrap();
    fs::write(tree.join("gone.txt"), "g").unwrap();
    fs::write(tree.join("a/odd 'name\".txt"), "q").unwrap();
    std::os::unix::fs::symlink("/etc", tree.join("link")).unwrap();
    // More folders than the request window and more files than one READDIR reply holds.
    for i in 0..40 {
        fs::create_dir(tree.join(format!("many/d{i}"))).unwrap();
        fs::write(tree.join(format!("many/d{i}/f")), "z").unwrap();
    }
    for i in 0..2500 {
        fs::write(tree.join(format!("many/file-{i:04}-with-a-longish-name-to-fill-the-reply")), "").unwrap();
    }
    let ssh = busybox_ssh(dir.path(), server);
    let ssh = ssh.to_str().unwrap();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::new("sftp@box", tree.to_str().unwrap()).unwrap());
    let progress = Progress::default();
    remote::sync_with(ssh, &index, id, &progress).unwrap();
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        // Symlinks are skipped; folders are walked.
        assert_eq!(names(&index, root), ["a", "gone.txt", "keep.txt", "many"]);
        let a = index.child_named(root, "a").unwrap();
        assert_eq!(names(&index, a), ["b", "odd 'name\".txt"]);
        let f = index.child_named(index.child_named(a, "b").unwrap(), "f.txt").unwrap();
        // No allocation from SFTP: On-disk Size is the Logical Size.
        assert_eq!((index.logical(f), index.on_disk(f)), (3000, 3000));
        let mtime = fs::metadata(tree.join("a/b/f.txt")).unwrap().modified().unwrap();
        let secs = mtime.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(index.modified(f), secs * 1_000_000_000);
        let many = index.child_named(root, "many").unwrap();
        assert_eq!(index.children(many).len(), 2540);
        assert_eq!(Filter::compute(&index, None).totals(&index, root).logical, 3000 + 1 + 1 + 1 + 40);
        assert_eq!(progress.entries(), 1 + 4 + 2 + 1 + 2540 + 40);
    }
    assert_eq!(fs::read_to_string(dir.path().join("find.log")).unwrap().lines().count(), 1);
    // The host has no GNU find but runs df: its capacity is known all the same.
    {
        let index = index.read();
        let c = index.volume(id).unwrap().capacity.expect("df worked over the same ssh");
        assert!(c.total > 0 && c.free <= c.total);
    }
    assert!(remote::is_sftp_only(&Remote::new("sftp@box", tree.to_str().unwrap()).unwrap()));

    // A refresh goes straight to SFTP (find is not tried again) and prunes what is gone.
    fs::remove_file(tree.join("gone.txt")).unwrap();
    fs::remove_dir_all(tree.join("a/b")).unwrap();
    remote::sync_with(ssh, &index, id, &Progress::default()).unwrap();
    assert_eq!(fs::read_to_string(dir.path().join("find.log")).unwrap().lines().count(), 1);
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["a", "keep.txt", "many"]);
    assert_eq!(names(&index, index.child_named(root, "a").unwrap()), ["odd 'name\".txt"]);
}

#[test]
fn an_unreadable_folder_over_sftp_keeps_stale_entries() {
    let _serial = serial();
    let Some(server) = sftp_server() else {
        eprintln!("skipped: no sftp-server installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("locked")).unwrap();
    fs::write(tree.join("locked/secret"), "s").unwrap();
    fs::write(tree.join("a.txt"), "a").unwrap();
    let ssh = busybox_ssh(dir.path(), server);
    let ssh = ssh.to_str().unwrap();
    let index = SharedIndex::new(Index::new());
    let id = index
        .write()
        .add_remote(&Remote::new("sftp-locked@box", tree.to_str().unwrap()).unwrap());
    remote::sync_with(ssh, &index, id, &Progress::default()).unwrap();
    fs::set_permissions(tree.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let progress = Progress::default();
    let result = remote::sync_with(ssh, &index, id, &progress);
    fs::set_permissions(tree.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    result.unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    if progress.unreadable() == 0 {
        return; // running as root: nothing is unreadable
    }
    assert_eq!(progress.unreadable(), 1);
    // The folder's contents were not listed, so they are not pruned.
    assert_eq!(names(&index, index.child_named(root, "locked").unwrap()), ["secret"]);
}

#[test]
fn sftp_failures_are_reported_and_a_file_root_is_refused() {
    let _serial = serial();
    let Some(server) = sftp_server() else {
        eprintln!("skipped: no sftp-server installed");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "f").unwrap();
    let ssh = busybox_ssh(dir.path(), server);
    let ssh = ssh.to_str().unwrap();
    let index = SharedIndex::new(Index::new());
    let missing = index
        .write()
        .add_remote(&Remote::new("sftp-missing@box", "/no/such/folder").unwrap());
    let err = remote::sync_with(ssh, &index, missing, &Progress::default()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    let file = index
        .write()
        .add_remote(&Remote::new("sftp-file@box", dir.path().join("file").to_str().unwrap()).unwrap());
    let err = remote::sync_with(ssh, &index, file, &Progress::default()).unwrap_err();
    assert!(err.to_string().contains("not a folder"), "{err}");
    assert_eq!(index.read().live_count(), 2);
}
