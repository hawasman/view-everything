//! An agent's access to a Module through the `Engine` handle: first contact is refused with a
//! code, approving it grants that agent's key, codes expire, and revoking refuses again.

use parking_lot::Mutex;
use std::sync::Arc;
use ve_core::CategoryMap;
use ve_engine::agents::{Access, Agent, CODE_LIFETIME, FILES, Module, Reason, Request};
use ve_engine::permissions::{Hooks, Kind, Permission};
use ve_engine::{Config, Engine};

const T0: i64 = 1_800_000_000;

/// Each list the host was asked to save, and each request it was told of.
type Saved = Arc<Mutex<Vec<Vec<Permission>>>>;
type Told = Arc<Mutex<Vec<Request>>>;

/// An engine whose host records each saved list and each request it was told of.
fn start() -> (Engine, Saved, Told) {
    let engine = Engine::start(
        Config {
            index_path: None,
            mounted: Vec::new(),
            volumes: Vec::new(),
            remotes: Vec::new(),
            intervals: Default::default(),
            categories: CategoryMap::builtin(),
            live_helper: false,
        },
        Arc::new(|| {}),
    );
    let (saved, told) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    let on_save = saved.clone();
    engine.permissions().set_hooks(Hooks {
        remove_login_entry: Box::new(|| Ok(())),
        save: Box::new(move |list| {
            on_save.lock().push(list.to_vec());
            Ok(())
        }),
    });
    let on_request = told.clone();
    engine
        .agents()
        .set_on_request(Box::new(move |request| on_request.lock().push(request.clone())));
    (engine, saved, told)
}

fn agent(name: &str, key: &str) -> Agent {
    Agent {
        name: name.into(),
        key: key.into(),
    }
}

fn pending(access: Access) -> Request {
    match access {
        Access::Pending(request) => request,
        Access::Allowed => panic!("allowed without approval"),
    }
}

#[test]
fn first_contact_is_pending_until_its_code_is_approved() {
    let (engine, saved, told) = start();
    let claude = agent("Claude", "k1");

    let request = pending(engine.agents().access(&claude, FILES, T0));
    assert_eq!(request.code.len(), 4);
    assert!(
        request.code.chars().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()),
        "{}",
        request.code
    );
    assert_eq!((request.agent.as_str(), request.module.as_str()), ("Claude", FILES));
    assert_eq!(request.expires_at, T0 + CODE_LIFETIME);
    // Asking again while pending is the same request, and the host is told once.
    let again = pending(engine.agents().access(&claude, FILES, T0 + 30));
    assert_eq!(again.code, request.code);
    assert_eq!(*told.lock(), vec![request.clone()]);
    assert_eq!(engine.agents().pending(T0 + 30), vec![request.clone()]);

    let granted = engine.agents().approve(&request.code.to_lowercase(), T0 + 60).unwrap();

    assert_eq!(
        granted,
        Permission {
            kind: Kind::Agent,
            subject: "Claude".into(),
            granted_at: Some(T0 + 60),
            module: Some(FILES.into()),
            key: Some("k1".into()),
        }
    );
    assert_eq!(engine.agents().access(&claude, FILES, T0 + 61), Access::Allowed);
    assert!(engine.agents().pending(T0 + 61).is_empty());
    assert_eq!(*saved.lock(), vec![vec![granted]]);
}

#[test]
fn a_code_expires_after_ten_minutes() {
    let (engine, saved, _) = start();
    let claude = agent("Claude", "k1");
    let request = pending(engine.agents().access(&claude, FILES, T0));

    assert_eq!(engine.agents().pending(T0 + CODE_LIFETIME - 1).len(), 1);
    let refused = engine.agents().approve(&request.code, T0 + CODE_LIFETIME).unwrap_err();

    assert!(refused.contains("expired"), "{refused}");
    assert!(engine.agents().pending(T0 + CODE_LIFETIME).is_empty());
    assert!(saved.lock().is_empty());
    // Asking again after expiry is a new request with a new deadline.
    let renewed = pending(engine.agents().access(&claude, FILES, T0 + CODE_LIFETIME));
    assert_eq!(renewed.expires_at, T0 + 2 * CODE_LIFETIME);
    assert!(engine.agents().approve("ZZZZ", T0).unwrap_err().contains("No request"));
}

#[test]
fn another_key_under_an_approved_name_is_refused() {
    let (engine, _, told) = start();
    let request = pending(engine.agents().access(&agent("Claude", "k1"), FILES, T0));
    engine.agents().approve(&request.code, T0).unwrap();

    let impostor = pending(engine.agents().access(&agent("Claude", "k2"), FILES, T0 + 1));
    let renamed = pending(engine.agents().access(&agent("Claude Desktop", "k1"), FILES, T0 + 1));

    assert_ne!(impostor.code, request.code);
    assert_ne!(renamed.code, impostor.code);
    assert_eq!(told.lock().len(), 3);
    assert_eq!(engine.agents().access(&agent("Claude", "k1"), FILES, T0 + 2), Access::Allowed);
}

#[test]
fn revoking_refuses_the_agent_again() {
    let (engine, saved, _) = start();
    let claude = agent("Claude", "k1");
    let request = pending(engine.agents().access(&claude, FILES, T0));
    engine.agents().approve(&request.code, T0).unwrap();

    engine.permissions().revoke(Kind::Agent, "Claude").unwrap();

    assert!(engine.permissions().list().is_empty());
    assert_eq!(saved.lock().last(), Some(&Vec::new()));
    let refused = engine
        .agents()
        .read(Module::Files, &claude, "files.find", serde_json::json!({ "query": "x" }), T0 + 5)
        .unwrap_err();
    assert_eq!(refused.reason, Reason::ApprovalPending);
    let code = refused.code.clone().unwrap();
    assert!(
        refused.message.starts_with(&format!("approval pending, code {code}")),
        "{}",
        refused.message
    );

    // Approved again, then revoked for the one Module.
    engine.agents().approve(&code, T0 + 6).unwrap();
    assert_eq!(engine.agents().access(&claude, FILES, T0 + 7), Access::Allowed);
    engine.agents().revoke("Claude", FILES).unwrap();
    assert!(matches!(engine.agents().access(&claude, FILES, T0 + 8), Access::Pending(_)));
    assert!(engine.agents().revoke("Claude", FILES).is_err());
}

#[test]
fn an_agent_is_never_granted_like_the_other_kinds() {
    let (engine, _, _) = start();
    assert!(engine.permissions().grant(Kind::Agent, T0).is_err());
    assert!(engine.permissions().list().is_empty());
}
