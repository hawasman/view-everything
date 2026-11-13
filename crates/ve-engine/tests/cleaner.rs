//! Drives the Cleaner through the `Engine` handle against a fixture project.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::CategoryMap;
use ve_engine::cleaner::{self, Ages};
use ve_engine::tasks::TaskKind;
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn volume(mount: &Path) -> VolumeInfo {
    VolumeInfo {
        mount: mount.to_path_buf(),
        identity: "uuid:cleaner".into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    }
}

/// A Rust project with build artefacts, indexed and looked at by the Cleaner. Kept out of the
/// temp folder, which a Cleanup Rule matches whole.
fn looked() -> (tempfile::TempDir, Engine, PathBuf) {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mount = root.path().canonicalize().unwrap();
    fs::create_dir_all(mount.join("proj/target/debug")).unwrap();
    fs::write(mount.join("proj/Cargo.toml"), "[package]").unwrap();
    fs::write(mount.join("proj/target/debug/app"), vec![b'x'; 4096]).unwrap();
    let config = Config {
        index_path: None,
        mounted: vec![volume(&mount)],
        volumes: vec![volume(&mount)],
        remotes: Vec::new(),
        intervals: Default::default(),
        categories: CategoryMap::builtin(),
        live_helper: false,
    };
    let engine = Engine::start(config, Arc::new(|| {}));
    wait_for("the first Sync", || {
        !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle)
    });
    engine.shared.cleaner.configure(None, vec![volume(&mount)], Ages::default());
    wait_for("the Cleaner's look", || {
        cleaner::refresh(&engine.shared);
        engine.cleanables().version > 0
    });
    (root, engine, mount.join("proj/target"))
}

#[test]
fn the_engine_finds_cleanables_and_publishes_them_for_is_cleanable() {
    let (_root, engine, target) = looked();
    let found = engine.cleanables();
    let keys: Vec<_> = found.own.iter().map(|c| c.key()).collect();
    assert_eq!(keys, [("Rust build artefacts".to_owned(), target.clone())]);
    assert!(found.admin.is_empty());
    let entry = engine.shared.index.read().find_path(&target).unwrap();
    assert!(engine.shared.cleanables.lock().set.contains(entry.index()));
    assert!(!engine.shared.tasks.is_running(TaskKind::CleanerSearch));
}

#[test]
fn a_clean_through_the_engine_removes_the_cleanable_from_disk_and_index() {
    let (_root, engine, target) = looked();
    let key = ("Rust build artefacts".to_owned(), target.clone());
    engine.clean(&[key]).unwrap();
    wait_for("the Clean", || !engine.shared.tasks.is_running(TaskKind::Clean) && !target.exists());
    assert!(engine.shared.index.read().find_path(&target).is_none());
    assert!(target.parent().unwrap().join("Cargo.toml").exists());
}

#[test]
fn a_clean_of_something_not_found_is_refused() {
    let (_root, engine, target) = looked();
    let other = target.parent().unwrap().join("Cargo.toml");
    let err = engine.clean(&[("Rust build artefacts".to_owned(), other.clone())]).unwrap_err();
    assert!(err.contains("is not a Cleanable"), "{err}");
    assert!(other.exists());
}
