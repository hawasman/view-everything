//! One round-trip over the engine socket: handshake, echo, and a refused version.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use ve_core::CategoryMap;
use ve_engine::socket::{self, Claim, Client, VERSION};
use ve_engine::{Config, Engine};

#[test]
fn handshake_echo_and_a_refused_version() {
    #[cfg(unix)]
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let path = dir.path().join("view-everything").join("engine.sock");
    #[cfg(windows)]
    let path = std::path::PathBuf::from(format!(r"\\.\pipe\view-everything-test-{}-engine", std::process::id()));
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
    socket::serve(listener, Arc::new(engine), Arc::new(|_| {}));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(path.parent().unwrap()), mode(&path)), (0o700, 0o600));
    }

    let mut client = Client::connect(&path).unwrap();
    let params = json!({ "text": "hello", "n": [1, 2] });
    assert_eq!(client.call("echo", params.clone()).unwrap(), params);
    match socket::claim(&path).unwrap() {
        Claim::Running(_) => {}
        Claim::Host(_) => panic!("a second host while the first runs"),
        Claim::Incompatible(reason) => panic!("the first host refused: {reason}"),
    }

    #[cfg(unix)]
    let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
    #[cfg(windows)]
    let stream = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
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

#[test]
fn an_oversized_request_line_is_refused_and_a_silent_host_times_out() {
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
    #[cfg(unix)]
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let path = dir.path().join("view-everything").join("engine.sock");
    #[cfg(windows)]
    let path = std::path::PathBuf::from(format!(r"\\.\pipe\view-everything-cap-{}-engine", std::process::id()));
    let Claim::Host(listener) = socket::claim(&path).unwrap() else {
        panic!("nothing else listens there")
    };
    socket::serve(listener, Arc::new(engine), Arc::new(|_| {}));

    #[cfg(unix)]
    let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
    #[cfg(windows)]
    let stream = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    let mut lines = BufReader::new(stream.try_clone().unwrap());
    let mut writer = &stream;
    writeln!(writer, "{}", json!({ "version": VERSION })).unwrap();
    let mut reply = String::new();
    lines.read_line(&mut reply).unwrap();
    // More than the 1 MiB cap with no newline: the engine answers with an error and hangs up.
    let junk = vec![b'x'; 1024 * 1024 + 16];
    let _ = writer.write_all(&junk);
    let mut rest = String::new();
    lines.read_line(&mut rest).unwrap();
    let rest: Value = serde_json::from_str(&rest).unwrap();
    assert!(rest["error"].as_str().unwrap().contains("too long"), "{rest}");
}
