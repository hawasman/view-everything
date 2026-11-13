//! Who starts a Task, through the `Engine` handle: the app's own run at background priority,
//! the user's at full speed.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::CategoryMap;
use ve_engine::tasks::{StartedBy, TaskKind};
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

const IDENTITY: &str = "uuid:fixture";

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn config(data: &Path, mount: &Path) -> Config {
    let volume = VolumeInfo {
        mount: mount.to_path_buf(),
        identity: IDENTITY.into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    Config {
        index_path: Some(data.join("index.bin")),
        mounted: vec![volume.clone()],
        volumes: vec![volume],
        remotes: Vec::new(),
        intervals: Default::default(),
        categories: CategoryMap::builtin(),
        live_helper: false,
    }
}

fn idle(engine: &Engine) -> bool {
    !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle)
}

#[test]
fn the_apps_own_sync_runs_at_background_priority_and_sync_now_at_full_speed() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    fs::write(fixture.path().join("a.txt"), "x").unwrap();
    let engine = Engine::start(config(data.path(), fixture.path()), Arc::new(|| {}));

    // A Location the Index has never held is read in full at launch, on the app's own.
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());
    let first = engine.shared.tasks.finished()[0].clone();
    assert_eq!(
        (first.kind, first.started_by, first.background),
        (TaskKind::Sync, StartedBy::App, true)
    );

    // The user's Sync now of the Location.
    engine.sync_location(IDENTITY.to_owned());
    wait_for("Sync now", || idle(&engine) && engine.shared.tasks.finished().len() == 2);
    let now = engine.shared.tasks.finished()[0].clone();
    assert_eq!((now.kind, now.started_by, now.background), (TaskKind::Sync, StartedBy::User, false));
    engine.shutdown();
}
