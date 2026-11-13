//! Who starts a Task, through the `Engine` handle: the app's own run at background priority
//! and pause while the machine is busy; the user's run at full speed and never pause.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use ve_core::CategoryMap;
use ve_engine::perf::{Backend, Counters, CpuCounters, CpuTimes, Reading};
use ve_engine::tasks::{Outcome, StartedBy, TaskInfo, TaskKind};
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

/// One CPU whose busy share each second is `load` percent.
struct Load {
    load: Arc<AtomicU64>,
    busy: u64,
    total: u64,
}

impl Backend for Load {
    fn read(&mut self) -> Counters {
        self.busy += self.load.load(Ordering::Relaxed);
        self.total += 100;
        let times = CpuTimes {
            busy: self.busy,
            total: self.total,
        };
        Counters {
            cpu: Reading::Value(CpuCounters {
                total: times,
                cores: vec![(0, times)],
            }),
            core_kinds: Arc::from([]),
            memory: Reading::Unsupported,
            disks: Reading::Unsupported,
            network: Reading::Unsupported,
        }
    }
}

/// An Engine that has synced a fixture holding two copies of one 2 MiB file, with its
/// Performance samples taken from a CPU whose load the returned handle sets.
fn engine_with_duplicates(data: &Path, fixture: &Path) -> (Engine, Arc<AtomicU64>) {
    for name in ["a.bin", "b.bin"] {
        fs::write(fixture.join(name), vec![7u8; 2 << 20]).unwrap();
    }
    let load = Arc::new(AtomicU64::new(0));
    let backend = Load {
        load: load.clone(),
        busy: 0,
        total: 0,
    };
    let engine = Engine::start_with_perf(config(data, fixture), Arc::new(|| {}), Box::new(backend));
    wait_for("the first Sync", || idle(&engine) && !engine.shared.tasks.finished().is_empty());
    (engine, load)
}

/// Records the seconds `from` to `to` at `percent` CPU load.
fn seconds(engine: &Engine, load: &AtomicU64, percent: u64, from: i64, to: i64) {
    load.store(percent, Ordering::Relaxed);
    for t in from..=to {
        engine.perf.tick(t);
    }
}

fn running(engine: &Engine, kind: TaskKind) -> Option<TaskInfo> {
    engine.shared.tasks.running().into_iter().find(|t| t.kind == kind)
}

fn finished(engine: &Engine, kind: TaskKind, by: StartedBy) -> Option<TaskInfo> {
    engine
        .shared
        .tasks
        .finished()
        .into_iter()
        .find(|t| t.kind == kind && t.started_by == by)
}

/// The app's own Duplicate search starts (as the `dupe:` Query asks for it) and is Paused.
fn paused_search(engine: &Engine) -> TaskInfo {
    engine.set_query("dupe:".into());
    wait_for("a Paused Duplicate search", || {
        running(engine, TaskKind::DuplicateSearch).is_some_and(|t| t.paused)
    });
    let search = running(engine, TaskKind::DuplicateSearch).unwrap();
    assert_eq!(search.started_by, StartedBy::App);
    search
}

#[test]
fn the_apps_own_task_pauses_after_30_busy_seconds_and_resumes_after_2_quiet_minutes() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (engine, load) = engine_with_duplicates(data.path(), fixture.path());

    // Heavy from the sample of second 2 (the first reading only primes the next): 29 seconds
    // later the machine is not yet busy, 30 seconds later it is.
    seconds(&engine, &load, 95, 1, 31);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!engine.shared.tasks.machine_busy());
    seconds(&engine, &load, 95, 32, 32);
    wait_for("a busy machine", || engine.shared.tasks.machine_busy());

    // The search reads no file while Paused, and keeps its place.
    let search = paused_search(&engine);
    std::thread::sleep(Duration::from_millis(800));
    let still = running(&engine, TaskKind::DuplicateSearch).expect("still running");
    assert_eq!((still.id, still.paused, still.done), (search.id, true, 0));
    assert_eq!(engine.shared.hash_progress.done.load(Ordering::Relaxed), 0);

    // The user's Sync now runs meanwhile, never Paused.
    engine.sync_location(IDENTITY.to_owned());
    wait_for("Sync now", || finished(&engine, TaskKind::Sync, StartedBy::User).is_some());
    assert!(running(&engine, TaskKind::DuplicateSearch).is_some_and(|t| t.paused));

    // A heavy second inside two quiet minutes starts them over; then two quiet minutes.
    seconds(&engine, &load, 10, 33, 100);
    seconds(&engine, &load, 95, 101, 101);
    seconds(&engine, &load, 10, 102, 220);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(running(&engine, TaskKind::DuplicateSearch).is_some_and(|t| t.paused));
    seconds(&engine, &load, 10, 221, 222);
    wait_for("the search to finish", || {
        finished(&engine, TaskKind::DuplicateSearch, StartedBy::App).is_some()
    });
    let done = finished(&engine, TaskKind::DuplicateSearch, StartedBy::App).unwrap();
    assert!(!done.paused);
    assert!(matches!(done.outcome, Some(Outcome::Done(_))));
    assert_eq!(engine.shared.duplicates.lock().groups.len(), 1);
    engine.shutdown();
}

#[test]
fn run_anyway_resumes_a_paused_task_at_once() {
    let (data, fixture) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (engine, load) = engine_with_duplicates(data.path(), fixture.path());
    seconds(&engine, &load, 95, 1, 40);
    wait_for("a busy machine", || engine.shared.tasks.machine_busy());
    let search = paused_search(&engine);

    engine.shared.tasks.run_anyway(search.id);
    wait_for("the search to finish", || {
        finished(&engine, TaskKind::DuplicateSearch, StartedBy::App).is_some()
    });
    // Still busy, and it was not Paused again.
    assert!(engine.shared.tasks.machine_busy());
    assert_eq!(engine.shared.duplicates.lock().groups.len(), 1);
    engine.shutdown();
}
