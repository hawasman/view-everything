//! One round-trip over the engine socket: handshake, echo, and a refused version.
#![cfg(unix)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use ve_core::CategoryMap;
use ve_engine::socket::{self, Claim, Client, VERSION};
use ve_engine::{Config, Engine};

#[test]
fn handshake_echo_and_a_refused_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("view-everything").join("engine.sock");
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
    let Claim::Host(listener) = socket::claim(&path).unwrap() else {
        panic!("nothing else listens there")
    };
    socket::serve(listener, Arc::new(engine), Arc::new(|| {}));
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(path.parent().unwrap()), mode(&path)), (0o700, 0o600));

    let mut client = Client::connect(&path).unwrap();
    let params = json!({ "text": "hello", "n": [1, 2] });
    assert_eq!(client.call("echo", params.clone()).unwrap(), params);
    assert!(
        matches!(socket::claim(&path).unwrap(), Claim::Running(_)),
        "a second host finds the first"
    );

    let stream = UnixStream::connect(&path).unwrap();
    let mut lines = BufReader::new(stream.try_clone().unwrap());
    writeln!(&stream, "{}", json!({ "version": VERSION + 1 })).unwrap();
    let mut reply = String::new();
    lines.read_line(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["expected"], VERSION);
    assert!(reply["error"].as_str().unwrap().contains(&format!("version {}", VERSION + 1)));
    let mut rest = String::new();
    assert_eq!(lines.read_line(&mut rest).unwrap(), 0, "the refused connection is closed");
}
