//! Permissions through the `Engine` handle: what is listed, and what a revoke undoes.

use parking_lot::Mutex;
use std::sync::Arc;
use ve_core::CategoryMap;
use ve_engine::permissions::{Hooks, Kind, Permission};
use ve_engine::{Config, Engine};

fn start() -> Engine {
    let config = Config {
        index_path: None,
        mounted: Vec::new(),
        volumes: Vec::new(),
        remotes: Vec::new(),
        intervals: Default::default(),
        categories: CategoryMap::builtin(),
        live_helper: false,
    };
    Engine::start(config, Arc::new(|| {}))
}

/// What the host was asked to do: login entries removed, and each list saved.
#[derive(Default)]
struct Calls {
    removed: u32,
    saved: Vec<Vec<Permission>>,
}

/// Hooks that record their calls; `removal` is what removing the login entry returns.
fn hooks(removal: Result<(), String>) -> (Hooks, Arc<Mutex<Calls>>) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (on_remove, on_save) = (calls.clone(), calls.clone());
    let hooks = Hooks {
        remove_login_entry: Box::new(move || {
            on_remove.lock().removed += 1;
            removal.clone()
        }),
        save: Box::new(move |list| {
            on_save.lock().saved.push(list.to_vec());
            Ok(())
        }),
    };
    (hooks, calls)
}

fn granted_before(kind: Kind) -> Permission {
    Permission::of(kind, None)
}

#[test]
fn revoking_start_at_login_removes_the_login_entry_and_forgets_the_permission() {
    let engine = start();
    let (hooks, calls) = hooks(Ok(()));
    engine.permissions().set_hooks(hooks);
    engine
        .permissions()
        .load(vec![granted_before(Kind::LiveHelper), granted_before(Kind::StartAtLogin)]);

    engine.permissions().revoke(Kind::StartAtLogin, "view-everything").unwrap();

    assert_eq!(engine.permissions().list(), vec![granted_before(Kind::LiveHelper)]);
    let calls = calls.lock();
    assert_eq!(calls.removed, 1);
    assert_eq!(calls.saved, vec![vec![granted_before(Kind::LiveHelper)]]);
}

#[test]
fn a_login_entry_that_cannot_be_removed_keeps_the_permission_and_says_why() {
    let engine = start();
    let (hooks, calls) = hooks(Err("Access denied".into()));
    engine.permissions().set_hooks(hooks);
    engine.permissions().load(vec![granted_before(Kind::StartAtLogin)]);

    let refused = engine.permissions().revoke(Kind::StartAtLogin, "view-everything");

    assert_eq!(refused, Err("Access denied".to_owned()));
    assert_eq!(engine.permissions().list(), vec![granted_before(Kind::StartAtLogin)]);
    assert!(calls.lock().saved.is_empty());
}

#[test]
fn revoking_the_live_helper_forgets_it_and_a_second_revoke_finds_nothing() {
    let engine = start();
    let (hooks, calls) = hooks(Ok(()));
    engine.permissions().set_hooks(hooks);
    engine.permissions().load(vec![granted_before(Kind::LiveHelper)]);

    engine.permissions().revoke(Kind::LiveHelper, "ve-fanotify").unwrap();

    assert!(!engine.permissions().has(Kind::LiveHelper));
    assert!(engine.permissions().revoke(Kind::LiveHelper, "ve-fanotify").is_err());
    let calls = calls.lock();
    assert_eq!(calls.saved, vec![Vec::new()]);
    assert_eq!(calls.removed, 0);
}

#[test]
fn granting_records_the_time_once_and_saves() {
    let engine = start();
    let (hooks, calls) = hooks(Ok(()));
    engine.permissions().set_hooks(hooks);

    engine.permissions().grant(Kind::LiveHelper, 1_700_000_000).unwrap();
    engine.permissions().grant(Kind::LiveHelper, 1_800_000_000).unwrap();

    let granted = vec![Permission {
        kind: Kind::LiveHelper,
        subject: "ve-fanotify".into(),
        granted_at: Some(1_700_000_000),
        module: None,
        key: None,
    }];
    assert_eq!(engine.permissions().list(), granted);
    assert_eq!(calls.lock().saved, vec![granted]);
}
