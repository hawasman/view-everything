//! `launch.match` through the Engine handle: the Launcher's matches of Entry names in the Index.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use ve_core::CategoryMap;
use ve_engine::launch::{Kind, Match};
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

fn volume(identity: &str, mount: &Path) -> VolumeInfo {
    VolumeInfo {
        mount: mount.to_path_buf(),
        identity: identity.into(),
        fs_type: "ext4".into(),
        label: identity.into(),
        removable: true,
        network: false,
        read_only: false,
    }
}

fn start(data: &Path, mounted: Vec<VolumeInfo>) -> Engine {
    Engine::start(
        Config {
            index_path: Some(data.join("index.bin")),
            mounted: mounted.clone(),
            volumes: mounted,
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    )
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn idle(engine: &Engine) -> bool {
    !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle)
}

/// A file (or, ending in `/`, a folder) modified `days` days ago.
fn make(path: &Path, days: u64) {
    if path.as_os_str().to_string_lossy().ends_with('/') {
        fs::create_dir_all(path).unwrap();
    } else {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }
    let when = SystemTime::now() - Duration::from_secs(days * 86_400);
    fs::File::open(path).unwrap().set_modified(when).unwrap();
}

fn names(matches: &[Match]) -> Vec<&str> {
    matches.iter().map(|m| m.name.as_str()).collect()
}

#[test]
fn matches_rank_name_starts_then_word_starts_then_the_rest_newest_first_and_mark_offline_volumes() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (here, away) = (fixture.path().join("here"), fixture.path().join("away"));
    make(&here.join("notes/report.txt"), 30);
    make(&here.join("Reports/"), 2);
    make(&here.join("notes/annual-report.pdf"), 5);
    make(&here.join("notes/MyReport.doc"), 20);
    make(&here.join("notes/prereport.log"), 1);
    make(&here.join("notes/unrelated.txt"), 1);
    make(&away.join("old-report.md"), 10);
    let (here, away) = (volume("uuid:here", &here), volume("uuid:away", &away));

    // Both Volumes are indexed; then `away` is unplugged and the app started again.
    let engine = start(data.path(), vec![here.clone(), away]);
    wait_for("the first Sync", || {
        idle(&engine)
            && engine
                .shared
                .index
                .read()
                .volumes()
                .filter(|(_, v)| v.last_synced.is_some())
                .count()
                == 2
    });
    engine.shutdown();
    drop(engine);
    let engine = start(data.path(), vec![here]);
    wait_for("the saved Index", || idle(&engine));

    let found = engine.launch_match("REPORT").unwrap();
    assert_eq!(
        names(&found),
        [
            // The name starts with the text, newest first.
            "Reports",
            "report.txt",
            // A word in the name starts with it (after a separator or a capital), newest first.
            "annual-report.pdf",
            "old-report.md",
            "MyReport.doc",
            // Anywhere else in the name.
            "prereport.log",
        ]
    );
    let by_name = |name: &str| found.iter().find(|m| m.name == name).unwrap();
    assert_eq!(by_name("Reports").kind, Kind::Folder);
    assert_eq!(by_name("report.txt").kind, Kind::File);
    assert_eq!(Path::new(&by_name("report.txt").path), fixture.path().join("here/notes/report.txt"));
    assert!(by_name("old-report.md").offline, "away is not mounted");
    assert!(found.iter().filter(|m| m.name != "old-report.md").all(|m| !m.offline));
    engine.shutdown();
}

#[test]
fn matches_are_capped_at_fifty_and_empty_text_matches_nothing() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mount = fixture.path().join("volume");
    for n in 0..60 {
        // Higher numbers are newer.
        make(&mount.join(format!("clip-{n:02}.mp4")), 100 - n);
    }
    let engine = start(data.path(), vec![volume("uuid:clips", &mount)]);
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());

    let found = engine.launch_match("clip-").unwrap();
    assert_eq!(found.len(), 50);
    // The fifty newest: clip-59 down to clip-10.
    assert_eq!(found.first().unwrap().name, "clip-59.mp4");
    assert_eq!(found.last().unwrap().name, "clip-10.mp4");

    assert_eq!(engine.launch_match("").unwrap(), []);
    assert_eq!(engine.launch_match("   ").unwrap(), []);
    engine.shutdown();
}
