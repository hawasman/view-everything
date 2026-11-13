//! An agent's Proposals through the `Engine` handle: proposing is a Permission of its own, a
//! Proposal naming a Remote's or an Offline Volume's Entries is refused with the paths, applying
//! drops the rows that changed or vanished and moves only the rest to the Trash, and a Proposal
//! is pending, applied, dismissed or lapsed, also after a restart.

use parking_lot::Mutex;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::{CategoryMap, EntryMeta, Remote};
use ve_engine::agents::{Agent, Reason, Refusal};
use ve_engine::permissions::Kind;
use ve_engine::proposals::{Dropped, KEEP, LAPSE, Status, Why};
use ve_engine::tasks::TaskKind;
use ve_engine::{Config, Engine, Phase};
use ve_scan::VolumeInfo;

const T0: i64 = 1_800_000_000;

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A fixture Volume at `mount`, its Index (and Proposals) saved in `data`, synced once; the
/// Trash is the folder `trash`, and the Trash refuses any file named `locked.txt`.
fn start(data: &Path, mount: &Path, trash: &Path) -> Engine {
    let volume = VolumeInfo {
        mount: mount.to_path_buf(),
        identity: "uuid:fixture".into(),
        fs_type: "ext4".into(),
        label: "fixture".into(),
        removable: false,
        network: false,
        read_only: false,
    };
    let engine = Engine::start(
        Config {
            index_path: Some(data.join("index.bin")),
            mounted: vec![volume.clone()],
            volumes: vec![volume],
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    );
    wait_for("the first Sync", || {
        !engine.is_busy() && matches!(engine.shared.status.lock().phase, Phase::Idle) && !engine.shared.tasks.finished().is_empty()
    });
    let trash = trash.to_path_buf();
    engine.set_trash_mover(Arc::new(move |path: &Path| {
        if path.file_name().is_some_and(|n| n == "locked.txt") {
            return Err("Permission denied".into());
        }
        fs::rename(path, trash.join(path.file_name().unwrap())).map_err(|e| e.to_string())
    }));
    engine
}

fn agent(name: &str) -> Agent {
    Agent {
        name: name.into(),
        key: format!("{name}-key"),
    }
}

fn path(root: &Path, name: &str) -> String {
    root.join(name).to_string_lossy().into_owned()
}

/// Approves the request `refusal` carries, as the user does.
fn approve(engine: &Engine, refusal: &Refusal, now: i64) {
    assert_eq!(refusal.reason, Reason::ApprovalPending, "{refusal:?}");
    engine.agents().approve(refusal.code.as_deref().unwrap(), now).unwrap();
}

/// `agent` may propose in Files.
fn may_propose(engine: &Engine, agent: &Agent) {
    let refused = engine
        .agents()
        .propose(agent, "files.proposal", json!({ "id": 1 }), T0)
        .unwrap_err();
    approve(engine, &refused, T0);
}

fn propose(engine: &Engine, agent: &Agent, paths: &[String], now: i64) -> Result<Value, Refusal> {
    engine.agents().propose(
        agent,
        "files.propose_trash",
        json!({ "paths": paths, "reason": "Old\nbuild output" }),
        now,
    )
}

fn status(engine: &Engine, agent: &Agent, id: u64, now: i64) -> Result<Value, Refusal> {
    engine.agents().propose(agent, "files.proposal", json!({ "id": id }), now)
}

#[test]
fn proposing_is_a_permission_of_its_own_first_refused_with_a_code() {
    let (data, mount, trash) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    fs::write(mount.path().join("old.log"), "x").unwrap();
    let engine = start(data.path(), mount.path(), trash.path());
    let told = Arc::new(Mutex::new(Vec::new()));
    let on_proposal = told.clone();
    engine
        .proposals()
        .set_on_proposal(Box::new(move |p| on_proposal.lock().push(p.clone())));
    let claude = agent("Claude");
    let paths = [path(mount.path(), "old.log")];

    // Reading Files grants no proposing.
    let read = engine
        .agents()
        .read(ve_engine::agents::Module::Files, &claude, "files.locations", Value::Null, T0);
    approve(&engine, &read.unwrap_err(), T0);
    let refused = propose(&engine, &claude, &paths, T0 + 1).unwrap_err();
    assert_eq!(refused.reason, Reason::ApprovalPending);
    assert!(refused.message.contains("may not make Proposals in Files"), "{}", refused.message);
    let request = engine.agents().pending(T0 + 1).pop().unwrap();
    assert_eq!((request.kind, request.module.as_str()), (Kind::AgentProposals, "Files"));
    assert!(engine.proposals().list(T0 + 1).is_empty(), "a refused Proposal is not submitted");

    let granted = engine.agents().approve(refused.code.as_deref().unwrap(), T0 + 2).unwrap();
    assert_eq!(granted.kind, Kind::AgentProposals);
    let answer = propose(&engine, &claude, &paths, T0 + 3).unwrap();
    assert_eq!((answer["id"].as_u64(), answer["status"].as_str()), (Some(1), Some("pending")));
    assert_eq!(answer["reason"], "Old build output", "one line");
    assert_eq!(answer["lapses_at"], T0 + 3 + LAPSE);
    assert!(mount.path().join("old.log").exists(), "proposing changes nothing");
    assert_eq!(told.lock().iter().map(|p| p.agent.as_str()).collect::<Vec<_>>(), ["Claude"]);

    // Revoking proposing leaves reading alone, and the other way round.
    engine.agents().revoke("Claude", Kind::AgentProposals, "Files").unwrap();
    assert_eq!(
        propose(&engine, &claude, &paths, T0 + 4).unwrap_err().reason,
        Reason::ApprovalPending
    );
    assert!(
        engine
            .agents()
            .read(ve_engine::agents::Module::Files, &claude, "files.locations", Value::Null, T0 + 4)
            .is_ok()
    );
    engine.shutdown();
}

#[test]
fn remote_and_offline_entries_are_refused_with_their_paths() {
    let (data, mount, trash) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    fs::write(mount.path().join("here.txt"), "x").unwrap();
    let engine = start(data.path(), mount.path(), trash.path());
    let remote_file = {
        let mut index = engine.shared.index.write();
        let remote = index.add_remote(&Remote::new("me@backup", "/srv").unwrap());
        let root = index.volume(remote).unwrap().root;
        let file = index.insert(root, "photo.jpg", EntryMeta::default());
        index.path(file).to_string_lossy().into_owned()
    };
    let claude = agent("Claude");
    may_propose(&engine, &claude);

    let refused = propose(&engine, &claude, &[path(mount.path(), "here.txt"), remote_file.clone()], T0).unwrap_err();
    assert_eq!((refused.reason, refused.paths.clone()), (Reason::Remote, vec![remote_file]));
    assert!(refused.message.contains("read-only and has no Trash"), "{}", refused.message);

    let whole = propose(&engine, &claude, &[mount.path().to_string_lossy().into_owned()], T0).unwrap_err();
    assert_eq!(whole.reason, Reason::BadPath);
    let nowhere = propose(&engine, &claude, &["/nowhere/at/all".into()], T0).unwrap_err();
    assert_eq!(
        (nowhere.reason, nowhere.paths),
        (Reason::BadPath, vec!["/nowhere/at/all".to_owned()])
    );

    engine.set_volumes(Vec::new(), Vec::new(), Vec::new());
    wait_for("the Volume to go offline", || {
        engine.shared.index.read().volumes().any(|(_, v)| !v.is_remote() && !v.online)
    });
    let offline = propose(&engine, &claude, &[path(mount.path(), "here.txt")], T0).unwrap_err();
    assert_eq!(
        (offline.reason, offline.paths),
        (Reason::Offline, vec![path(mount.path(), "here.txt")])
    );
    assert!(engine.proposals().list(T0).is_empty());
    engine.shutdown();
}

#[test]
fn applying_drops_rows_that_changed_or_vanished_and_moves_the_rest() {
    let (data, mount, trash) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    for name in ["keep.bin", "changed.bin", "gone.bin", "deleted.bin", "locked.txt", "unticked.bin"] {
        fs::write(mount.path().join(name), "x").unwrap();
    }
    fs::create_dir(mount.path().join("build")).unwrap();
    fs::write(mount.path().join("build").join("out.o"), "x").unwrap();
    let engine = start(data.path(), mount.path(), trash.path());
    let claude = agent("Claude");
    may_propose(&engine, &claude);
    let names = [
        "keep.bin",
        "changed.bin",
        "gone.bin",
        "deleted.bin",
        "locked.txt",
        "unticked.bin",
        "build",
        "build/out.o",
    ];
    let paths: Vec<String> = names.iter().map(|n| path(mount.path(), n)).collect();
    let id = propose(&engine, &claude, &paths, T0).unwrap()["id"].as_u64().unwrap();
    let proposal = engine.proposals().list(T0).pop().unwrap();
    assert_eq!(proposal.rows.len(), 7, "build/out.o goes with build");

    // Since proposed: one grew, one left the Index, one left the disk only.
    {
        let mut index = engine.shared.index.write();
        let changed = index.find_path(&mount.path().join("changed.bin")).unwrap();
        let grown = EntryMeta {
            logical: 1 << 20,
            on_disk: 1 << 20,
            ..index.meta(changed)
        };
        index.update(changed, grown);
        let gone = index.find_path(&mount.path().join("gone.bin")).unwrap();
        index.remove(gone);
    }
    fs::remove_file(mount.path().join("deleted.bin")).unwrap();

    let check = engine.proposals().check(id, T0 + 10).unwrap();
    assert_eq!(
        check.dropped,
        [
            Dropped {
                path: mount.path().join("changed.bin"),
                why: Why::Changed
            },
            Dropped {
                path: mount.path().join("gone.bin"),
                why: Why::Vanished
            },
        ]
    );
    let confirmed: Vec<PathBuf> = check
        .keep
        .iter()
        .map(|r| r.path.clone())
        .filter(|p| !p.ends_with("unticked.bin"))
        .collect();
    let applied = engine.proposals().apply(id, &confirmed, T0 + 20).unwrap();
    assert_eq!((applied.status, applied.answered_at), (Status::Applied, Some(T0 + 20)));
    let outcome = applied.outcome.unwrap();
    assert_eq!(outcome.trashed, [mount.path().join("keep.bin"), mount.path().join("build")]);
    assert_eq!(
        outcome.dropped,
        [
            Dropped {
                path: mount.path().join("changed.bin"),
                why: Why::Changed
            },
            Dropped {
                path: mount.path().join("gone.bin"),
                why: Why::Vanished
            },
            Dropped {
                path: mount.path().join("deleted.bin"),
                why: Why::Vanished
            },
        ]
    );
    assert_eq!(
        outcome
            .failed
            .iter()
            .map(|f| (f.path.clone(), f.error.as_str()))
            .collect::<Vec<_>>(),
        [(mount.path().join("locked.txt"), "Permission denied")]
    );
    assert_eq!(outcome.left_out, [mount.path().join("unticked.bin")]);

    // Moved to the Trash and out of the Index; the rest stays.
    assert!(trash.path().join("keep.bin").exists() && trash.path().join("build").join("out.o").exists());
    for name in ["changed.bin", "gone.bin", "locked.txt", "unticked.bin"] {
        assert!(mount.path().join(name).exists(), "{name} stays");
    }
    let index = engine.shared.index.read();
    for name in ["keep.bin", "build", "deleted.bin"] {
        assert!(index.find_path(&mount.path().join(name)).is_none(), "{name} left the Index");
    }
    drop(index);
    let task = &engine.shared.tasks.finished()[0];
    assert_eq!((task.kind, task.label.as_str()), (TaskKind::Trash, "Move 4 item(s) to the Trash"));

    // The agent reads the outcome; a second answer is refused.
    let read = status(&engine, &claude, id, T0 + 30).unwrap();
    assert_eq!(read["status"], "applied");
    assert_eq!(
        read["outcome"]["dropped"][0],
        json!({ "path": mount.path().join("changed.bin"), "why": "changed" })
    );
    assert!(
        engine
            .proposals()
            .apply(id, &confirmed, T0 + 31)
            .unwrap_err()
            .contains("already applied")
    );
    assert!(engine.proposals().dismiss(id, T0 + 31).unwrap_err().contains("already applied"));
    engine.shutdown();
}

#[test]
fn a_proposal_is_dismissed_or_lapses_and_its_status_survives_a_restart() {
    let (data, mount, trash) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    fs::write(mount.path().join("a.tmp"), "x").unwrap();
    let paths = [path(mount.path(), "a.tmp")];
    let (claude, other) = (agent("Claude"), agent("Other"));
    let engine = start(data.path(), mount.path(), trash.path());
    may_propose(&engine, &claude);
    may_propose(&engine, &other);

    let dismissed = propose(&engine, &claude, &paths, T0).unwrap()["id"].as_u64().unwrap();
    let lapsing = propose(&engine, &claude, &paths, T0 + 1).unwrap()["id"].as_u64().unwrap();
    assert_eq!(status(&engine, &claude, lapsing, T0 + 2).unwrap()["status"], "pending");
    // Another agent does not see it.
    assert_eq!(status(&engine, &other, lapsing, T0 + 2).unwrap_err().reason, Reason::NoProposal);
    assert_eq!(engine.proposals().dismiss(dismissed, T0 + 5).unwrap().status, Status::Dismissed);
    assert!(mount.path().join("a.tmp").exists());
    let granted = engine.permissions().list();
    engine.shutdown();
    drop(engine);
    // It holds the agents' keys: readable by the user alone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(data.path().join("proposals.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Kept beside the Index: a restart forgets nothing. (The host loads the Permissions.)
    let engine = start(data.path(), mount.path(), trash.path());
    engine.permissions().load(granted);
    assert_eq!(status(&engine, &claude, dismissed, T0 + 6).unwrap()["status"], "dismissed");
    assert_eq!(status(&engine, &claude, lapsing, T0 + 1 + LAPSE - 1).unwrap()["status"], "pending");
    let lapsed = status(&engine, &claude, lapsing, T0 + 1 + LAPSE).unwrap();
    assert_eq!(
        (lapsed["status"].as_str(), lapsed["answered_at"].as_i64()),
        (Some("lapsed"), Some(T0 + 1 + LAPSE))
    );
    let refused = engine.proposals().apply(lapsing, &[mount.path().join("a.tmp")], T0 + 2 + LAPSE);
    assert!(refused.unwrap_err().contains("lapsed"));
    assert!(mount.path().join("a.tmp").exists());

    // Ids are never reused; answered Proposals are forgotten a while after.
    let next = propose(&engine, &claude, &paths, T0 + 3 + LAPSE).unwrap()["id"].as_u64().unwrap();
    assert_eq!(next, 3);
    let later = T0 + 1 + LAPSE + KEEP;
    assert_eq!(engine.proposals().list(later).iter().map(|p| p.id).collect::<Vec<_>>(), [3]);
    assert_eq!(status(&engine, &claude, lapsing, later).unwrap_err().reason, Reason::NoProposal);
    engine.shutdown();
}

#[test]
fn an_agent_may_have_only_so_many_proposals_waiting() {
    let (data, mount, trash) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    fs::write(mount.path().join("a.tmp"), "x").unwrap();
    let engine = start(data.path(), mount.path(), trash.path());
    let claude = agent("Claude");
    may_propose(&engine, &claude);
    let paths = [path(mount.path(), "a.tmp")];
    for _ in 0..ve_engine::proposals::MAX_PENDING {
        propose(&engine, &claude, &paths, T0).unwrap();
    }
    assert_eq!(propose(&engine, &claude, &paths, T0).unwrap_err().reason, Reason::TooManyPending);
    engine.proposals().dismiss(1, T0).unwrap();
    assert!(propose(&engine, &claude, &paths, T0).is_ok(), "an answer makes room");
    engine.shutdown();
}
