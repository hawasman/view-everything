//! `ve` run as a binary: it asks a running engine over its socket, and reads the saved Index
//! file when none runs.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::{CategoryMap, EntryMeta, Index, SyncState};
use ve_engine::socket::{self, Claim};
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

const IDENTITY: &str = "uuid:fixture";

/// `ve` with `args`, its socket at `socket` and its data and config folders in `home`.
fn ve(args: &[&str], socket: &Path, home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ve"))
        .args(args)
        .env(socket::PATH_ENV, socket)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn find_answers_from_the_running_engine() {
    let (engine_data, fixture, home) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    fs::write(fixture.path().join("old-report.txt"), "x").unwrap();
    let volume = VolumeInfo {
        mount: fixture.path().to_path_buf(),
        identity: IDENTITY.into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    let engine = Arc::new(Engine::start(
        Config {
            index_path: Some(engine_data.path().join("index.bin")),
            mounted: vec![volume.clone()],
            volumes: vec![volume],
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    ));
    let idle = |engine: &Engine| !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle);
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());
    let socket_path = home.path().join("run").join("engine.sock");
    let Claim::Host(listener) = socket::claim(&socket_path).unwrap() else {
        panic!("nothing else listens there")
    };
    socket::serve(listener, engine.clone(), Arc::new(|_| {}));

    // Written after the first Sync and picked up by the next; the CLI's own data folder holds no
    // Index file, so only the engine can know it.
    let fresh = fixture.path().join("fresh-report.txt");
    fs::write(&fresh, "x").unwrap();
    engine.sync_location(IDENTITY.into());
    wait_for("Sync now", || idle(&engine) && engine.shared.tasks.finished().len() == 2);

    let out = ve(&["find", "fresh"], &socket_path, home.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), format!("{}\n", fresh.display()));
    let err = text(&out.stderr);
    assert!(err.starts_with("Last Synced: 20"), "{err}");
    assert!(!err.contains("saved Index"), "{err}");

    let bad = ve(&["find", "size:"], &socket_path, home.path());
    assert_eq!(bad.status.code(), Some(2));
    assert!(text(&bad.stderr).contains("bad Query"), "{}", text(&bad.stderr));
    engine.shutdown();
}

#[test]
fn find_reads_the_index_file_when_the_app_is_closed_and_says_so() {
    let home = tempfile::tempdir().unwrap();
    let mut index = Index::new();
    let folder = EntryMeta {
        is_dir: true,
        ..Default::default()
    };
    let vol = index.add_volume(Path::new("/w"), "ext4", "uuid:w", folder, SyncState::Walk);
    let root = index.volume(vol).unwrap().root;
    index.insert(root, "saved.txt", EntryMeta::default());
    index.mark_synced(vol, 1_791_646_413, None);
    let data = home.path().join("data").join("view-everything");
    fs::create_dir_all(&data).unwrap();
    index.save(&data.join("index.bin")).unwrap();

    let out = ve(&["find", "saved"], &home.path().join("nobody.sock"), home.path());
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "/w/saved.txt\n");
    let err = text(&out.stderr);
    assert!(err.contains("the app is not running; reading the saved Index file"), "{err}");
}
