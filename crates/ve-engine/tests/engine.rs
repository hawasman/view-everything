//! Drives the `Engine` handle the way its clients do, against fixture folders.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::CategoryMap;
use ve_engine::tasks::{Outcome, TaskKind};
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

const IDENTITY: &str = "uuid:fixture";

fn write(path: &Path, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; len]).unwrap();
}

/// Waits up to ten seconds for `ready`, the time a few Entries take with room to spare.
fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An Engine indexing the one fixture folder as a Volume, saving its Index in `data`.
fn start(data: &Path, mount: &Path) -> Engine {
    let volume = VolumeInfo {
        mount: mount.to_path_buf(),
        identity: IDENTITY.into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    let config = Config {
        index_path: Some(data.join("index.bin")),
        mounted: vec![volume.clone()],
        volumes: vec![volume],
        remotes: Vec::new(),
        intervals: Default::default(),
        categories: CategoryMap::builtin(),
        live_helper: false,
    };
    Engine::start(config, Arc::new(|| {}))
}

fn idle(engine: &Engine) -> bool {
    !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle)
}

/// Every Location as (identity, mount, online, synced).
fn locations(engine: &Engine) -> Vec<(String, PathBuf, bool, bool)> {
    let index = engine.shared.index.read();
    index
        .volumes()
        .map(|(_, v)| (v.identity.clone(), v.mount.clone(), v.online, v.last_synced.is_some()))
        .collect()
}

/// The names of the Entries the Engine's View of `query` matches, once that View is of the Index
/// as it is now (a View of the same text made before a Sync is stale).
fn names(engine: &Engine, query: &str) -> BTreeSet<String> {
    engine.set_query(query.to_owned());
    wait_for("the View of the Query", || {
        let generation = engine.shared.index.read().generation();
        let view = engine.shared.view.lock();
        view.as_ref()
            .is_some_and(|v| v.query == query && v.filter.generation() == generation)
    });
    let view = engine.shared.view.lock().clone().unwrap();
    let index = engine.shared.index.read();
    view.flat.iter().map(|&id| index.name(id).to_owned()).collect()
}

/// Two files in two folders, and an Engine that has finished its first Sync of them.
fn synced(data: &Path, fixture: &Path) -> Engine {
    write(&fixture.join("docs/quarterly-report.pdf"), 3000);
    write(&fixture.join("music/song.mp3"), 7000);
    let engine = start(data, fixture);
    // A Volume the Index has never held is read in full at launch.
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());
    engine
}

#[test]
fn a_synced_location_answers_queries_and_reports_its_task() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let engine = synced(data.path(), fixture.path());
    let location = (IDENTITY.to_owned(), fixture.path().to_path_buf(), true, true);
    assert_eq!(locations(&engine), [location]);
    assert_eq!(names(&engine, "report"), BTreeSet::from(["quarterly-report.pdf".to_owned()]));

    // Sync now of the Location reads what was written since.
    write(&fixture.path().join("docs/annual-report.pdf"), 5000);
    engine.sync_location(IDENTITY.to_owned());
    wait_for("Sync now", || idle(&engine) && engine.shared.tasks.finished().len() == 2);
    let task = engine.shared.tasks.finished()[0].clone();
    assert_eq!(task.kind, TaskKind::Sync);
    // docs, music and the three files under them.
    assert_eq!(task.done, 5);
    assert_eq!(task.outcome, Some(Outcome::Done("5 Entries read".into())));
    assert_eq!(
        names(&engine, "report"),
        BTreeSet::from(["annual-report.pdf".to_owned(), "quarterly-report.pdf".to_owned()])
    );
    engine.shutdown();
}

#[test]
fn a_restart_catches_the_saved_location_up() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let engine = synced(data.path(), fixture.path());
    // Quitting saves the Index; the next start loads it and catches the Location up.
    engine.shutdown();
    drop(engine);
    fs::remove_file(fixture.path().join("music/song.mp3")).unwrap();
    let again = start(data.path(), fixture.path());
    wait_for("the Catch-up", || idle(&again) && !again.shared.tasks.finished().is_empty());
    assert_eq!(again.shared.tasks.finished()[0].kind, TaskKind::CatchUp);
    let location = (IDENTITY.to_owned(), fixture.path().to_path_buf(), true, true);
    assert_eq!(locations(&again), [location]);
    assert!(names(&again, "song").is_empty());
    assert_eq!(names(&again, "report"), BTreeSet::from(["quarterly-report.pdf".to_owned()]));
    again.shutdown();
}
