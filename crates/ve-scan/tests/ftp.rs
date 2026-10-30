//! FTP Remotes against an in-process FTP server (control channel and passive data channels on
//! 127.0.0.1, ephemeral ports): MLSD and LIST (unix and DOS) variants, explicit FTPS with a
//! generated certificate, login, failures part-way. It records every command it is sent, so the
//! tests can show the app only ever reads.

use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use ve_core::{EntryId, Filter, FtpRemote, FtpTls, Index, Remote};
use ve_scan::ftp::{client_config, sync_with};
use ve_scan::{Progress, SharedIndex};

#[derive(Clone)]
enum Node {
    File(u64, &'static str),
    Dir,
    Link,
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    /// FEAT offers MLST and MLSD works.
    Mlsd,
    /// FEAT offers MLST but MLSD answers 500.
    MlsdBroken,
    /// No MLST; LIST in `ls -l` format.
    Unix,
    /// No MLST; LIST in IIS format.
    Dos,
}

struct State {
    /// Absolute folder -> its entries, by name.
    tree: Mutex<HashMap<String, Vec<(String, Node)>>>,
    style: Mutex<Style>,
    /// Folders whose CWD is refused.
    forbidden: Mutex<Vec<String>>,
    /// Accepts AUTH TLS with this configuration; `None` answers 534.
    tls: Option<Arc<ServerConfig>>,
    password: &'static str,
    /// Every command received (PASS is logged without its argument).
    log: Mutex<Vec<String>>,
    /// The password the server was actually sent.
    passwords: Mutex<Vec<String>>,
    stop: AtomicBool,
}

struct Server {
    port: u16,
    state: Arc<State>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

fn say(control: &mut BufReader<Stream>, text: &str) {
    let _ = write!(control.get_mut(), "{text}\r\n");
    let _ = control.get_mut().flush();
}

fn mlsd_line(name: &str, node: &Node) -> String {
    match node {
        Node::File(size, modify) => format!("type=file;size={size};modify={modify}; {name}"),
        Node::Dir => format!("type=dir;modify=20240101000000; {name}"),
        Node::Link => format!("type=OS.unix=slink:/elsewhere;size=9;modify=20240101000000; {name}"),
    }
}

/// `20240102030405` as `Jan  2  2024` for `ls`, and `01-02-24  03:04AM` for IIS.
fn list_line(style: Style, name: &str, node: &Node) -> String {
    let (size, modify) = match node {
        Node::File(size, modify) => (*size, *modify),
        _ => (4096, "20240101000000"),
    };
    let (y, mo, d) = (&modify[0..4], modify[4..6].parse::<usize>().unwrap(), &modify[6..8]);
    let (h, mi) = (modify[8..10].parse::<u32>().unwrap(), &modify[10..12]);
    match style {
        Style::Dos => {
            let kind = if matches!(node, Node::Dir) {
                "<DIR>".to_owned()
            } else {
                size.to_string()
            };
            format!(
                "{:02}-{d}-{}  {:02}:{mi}{} {kind:>10} {name}",
                mo,
                &y[2..],
                if h % 12 == 0 { 12 } else { h % 12 },
                if h >= 12 { "PM" } else { "AM" }
            )
        }
        _ => {
            let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][mo - 1];
            let (kind, name) = match node {
                Node::Dir => ('d', name.to_owned()),
                Node::Link => ('l', format!("{name} -> /elsewhere")),
                Node::File(..) => ('-', name.to_owned()),
            };
            format!("{kind}rwxr-xr-x 1 ftp ftp {size:>8} {month} {d:>2}  {y} {name}")
        }
    }
}

fn serve(state: Arc<State>, stream: TcpStream) {
    let mut control = BufReader::new(Stream::Plain(stream.try_clone().unwrap()));
    let local = stream.local_addr().unwrap();
    say(&mut control, "220 mock ftp ready");
    let (mut user_ok, mut logged_in, mut cwd, mut secure_data) = (false, false, "/".to_owned(), false);
    let mut pasv: Option<TcpListener> = None;
    loop {
        let mut line = String::new();
        if control.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end().to_owned();
        let (verb, arg) = line.split_once(' ').unwrap_or((&line, ""));
        let verb = verb.to_ascii_uppercase();
        state
            .log
            .lock()
            .unwrap()
            .push(if verb == "PASS" { "PASS".into() } else { line.clone() });
        let style = *state.style.lock().unwrap();
        match verb.as_str() {
            "AUTH" => match &state.tls {
                Some(config) => {
                    say(&mut control, "234 AUTH TLS OK");
                    let Stream::Plain(tcp) = std::mem::replace(control.get_mut(), Stream::Plain(stream.try_clone().unwrap())) else {
                        unreachable!()
                    };
                    let conn = ServerConnection::new(config.clone()).unwrap();
                    control = BufReader::new(Stream::Tls(Box::new(StreamOwned::new(conn, tcp))));
                }
                None => say(&mut control, "534 no TLS here"),
            },
            "PBSZ" => say(&mut control, "200 PBSZ=0"),
            "PROT" => {
                secure_data = arg == "P";
                say(&mut control, "200 PROT ok");
            }
            "USER" => {
                user_ok = arg == "me" || arg == "anonymous";
                say(&mut control, "331 password please");
            }
            "PASS" => {
                state.passwords.lock().unwrap().push(arg.to_owned());
                logged_in = user_ok && arg == state.password;
                say(&mut control, if logged_in { "230 welcome" } else { "530 Login incorrect" });
            }
            "FEAT" => {
                let mlst = if matches!(style, Style::Mlsd | Style::MlsdBroken) {
                    " MLST type*;size*;modify*;\r\n"
                } else {
                    ""
                };
                say(&mut control, &format!("211-Features:\r\n UTF8\r\n{mlst} PASV\r\n211 End"));
            }
            "TYPE" => say(&mut control, "200 type set"),
            "CWD" if !logged_in => say(&mut control, "530 log in first"),
            "CWD" => {
                let target = if arg.starts_with('/') {
                    arg.to_owned()
                } else {
                    format!("{}/{arg}", cwd.trim_end_matches('/'))
                };
                if state.tree.lock().unwrap().contains_key(&target) && !state.forbidden.lock().unwrap().contains(&target) {
                    cwd = target;
                    say(&mut control, "250 ok");
                } else {
                    say(&mut control, "550 no such folder or permission denied");
                }
            }
            "PASV" => {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let port = listener.local_addr().unwrap().port();
                pasv = Some(listener);
                // The address announced is deliberately wrong: the client must dial the control address.
                say(
                    &mut control,
                    &format!("227 Entering Passive Mode (10,255,255,1,{},{})", port / 256, port % 256),
                );
                let _ = local;
            }
            "MLSD" | "LIST" => {
                let Some(listener) = pasv.take() else {
                    say(&mut control, "425 use PASV first");
                    continue;
                };
                if verb == "MLSD" && style != Style::Mlsd {
                    say(&mut control, "500 MLSD not understood");
                    continue;
                }
                let tree = state.tree.lock().unwrap();
                let lines: Vec<String> = tree[&cwd]
                    .iter()
                    // IIS has no symlinks.
                    .filter(|(_, node)| !(style == Style::Dos && matches!(node, Node::Link)))
                    .map(|(name, node)| {
                        if verb == "MLSD" {
                            mlsd_line(name, node)
                        } else {
                            list_line(style, name, node)
                        }
                    })
                    .chain((verb == "LIST" && style != Style::Dos).then(|| "drwxr-xr-x 2 ftp ftp 4096 Jan  1  2024 .".to_owned()))
                    .collect();
                drop(tree);
                say(&mut control, "150 here it comes");
                let (data, _) = listener.accept().unwrap();
                let mut out = match (&state.tls, secure_data) {
                    (Some(config), true) => Stream::Tls(Box::new(StreamOwned::new(
                        ServerConnection::new(config.clone()).unwrap(),
                        data.try_clone().unwrap(),
                    ))),
                    _ => Stream::Plain(data.try_clone().unwrap()),
                };
                for line in lines.iter().filter(|l| !l.is_empty()) {
                    let _ = write!(out, "{line}\r\n");
                }
                if let Stream::Tls(tls) = &mut out {
                    // An empty folder sends nothing, so the handshake has not happened yet.
                    while tls.conn.is_handshaking() && tls.conn.complete_io(&mut tls.sock).is_ok() {}
                    tls.conn.send_close_notify();
                    let _ = tls.conn.complete_io(&mut tls.sock);
                }
                let _ = out.flush();
                let _ = data.shutdown(std::net::Shutdown::Both);
                say(&mut control, "226 done");
            }
            "QUIT" => {
                say(&mut control, "221 bye");
                return;
            }
            _ => say(&mut control, "502 not implemented"),
        }
    }
}

fn start_with(tls: Option<Arc<ServerConfig>>, style: Style) -> Server {
    let tree = HashMap::from([
        (
            "/pub".to_owned(),
            vec![
                ("docs".to_owned(), Node::Dir),
                ("movie one.mkv".to_owned(), Node::File(5000, "20240102030405")),
                ("shortcut".to_owned(), Node::Link),
                ("empty".to_owned(), Node::Dir),
            ],
        ),
        (
            "/pub/docs".to_owned(),
            vec![
                ("a.txt".to_owned(), Node::File(10, "20230630235959")),
                ("ünï.md".to_owned(), Node::File(20, "20250601120000")),
            ],
        ),
        ("/pub/empty".to_owned(), vec![]),
    ]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(State {
        tree: Mutex::new(tree),
        style: Mutex::new(style),
        forbidden: Mutex::default(),
        tls,
        password: "hunter2",
        log: Mutex::default(),
        passwords: Mutex::default(),
        stop: AtomicBool::new(false),
    });
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(stream) = stream {
                let state = shared.clone();
                std::thread::spawn(move || serve(state, stream));
            }
        }
    });
    Server { port, state }
}

fn start(style: Style) -> Server {
    start_with(None, style)
}

fn remote(server: &Server, tls: FtpTls) -> FtpRemote {
    FtpRemote::new("127.0.0.1", server.port, "me", "/pub", tls).unwrap()
}

fn good(_: &FtpRemote) -> Result<String, String> {
    Ok("hunter2".into())
}

fn no_trust() -> Arc<rustls::ClientConfig> {
    client_config(RootCertStore::empty())
}

fn names(index: &Index, dir: EntryId) -> Vec<String> {
    let mut v: Vec<_> = index.children(dir).map(|c| index.name(c).to_owned()).collect();
    v.sort();
    v
}

fn sync(index: &SharedIndex, id: ve_core::VolumeId, remote: &FtpRemote, progress: &Progress) -> std::io::Result<()> {
    let _ = remote;
    sync_with(index, id, progress, &good, no_trust())
}

fn add(server: &Server, tls: FtpTls) -> (SharedIndex, ve_core::VolumeId, FtpRemote) {
    let remote = remote(server, tls);
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Ftp(remote.clone()));
    (index, id, remote)
}

/// Same tree for every listing style: 2 folders with a file each side, one empty folder.
fn assert_indexed(index: &SharedIndex, id: ve_core::VolumeId, server: &Server) {
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    // The symlink `shortcut` is not an Entry.
    assert_eq!(names(&index, root), ["docs", "empty", "movie one.mkv"]);
    let movie = index.child_named(root, "movie one.mkv").unwrap();
    // Logical Size from the listing, On-disk Size equal to it.
    assert_eq!((index.logical(movie), index.on_disk(movie)), (5000, 5000));
    let docs = index.child_named(root, "docs").unwrap();
    assert_eq!(names(&index, docs), ["a.txt", "ünï.md"]);
    assert_eq!(names(&index, index.child_named(root, "empty").unwrap()), Vec::<String>::new());
    let totals = Filter::compute(&index, None).totals(&index, root);
    assert_eq!((totals.logical, totals.on_disk), (5030, 5030));
    assert_eq!(
        index.path(movie).to_string_lossy(),
        format!("ftp://me@127.0.0.1:{}/pub/movie one.mkv", server.port)
    );
}

/// Nothing but reading commands, in passive mode, and the password only after USER.
fn assert_read_only(server: &Server) {
    let log = server.state.log.lock().unwrap();
    let allowed = [
        "AUTH", "PBSZ", "PROT", "USER", "PASS", "FEAT", "TYPE", "CWD", "PASV", "MLSD", "LIST", "QUIT", "OPTS",
    ];
    for line in log.iter() {
        let verb = line.split(' ').next().unwrap().to_ascii_uppercase();
        assert!(allowed.contains(&verb.as_str()), "unexpected command {line}");
    }
    assert!(
        log.iter().any(|l| l == "PASV") && !log.iter().any(|l| l.starts_with("PORT") || l.starts_with("EPRT")),
        "passive mode only"
    );
}

#[test]
fn an_mlsd_server_is_walked_with_mlsd_and_a_refresh_applies_differences() {
    let server = start(Style::Mlsd);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    let progress = Progress::default();
    sync(&index, id, &remote, &progress).unwrap();
    assert_indexed(&index, id, &server);
    assert_eq!(progress.unreadable(), 0);
    // modify=20240102030405 as nanoseconds, UTC.
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        let movie = index.child_named(root, "movie one.mkv").unwrap();
        assert_eq!(index.modified(movie), 1_704_164_645_000_000_000);
    }
    {
        let log = server.state.log.lock().unwrap();
        assert!(log.iter().any(|l| l == "FEAT") && log.iter().any(|l| l == "MLSD") && !log.iter().any(|l| l == "LIST"));
        assert!(!log.iter().any(|l| l == "AUTH TLS"), "plain FTP was chosen");
    }
    assert_read_only(&server);
    assert_eq!(*server.state.passwords.lock().unwrap(), ["hunter2"]);

    // A later listing drops what is gone and picks up what changed.
    {
        let mut tree = server.state.tree.lock().unwrap();
        tree.get_mut("/pub/docs").unwrap().retain(|(n, _)| n != "a.txt");
        tree.get_mut("/pub")
            .unwrap()
            .push(("new.bin".into(), Node::File(7, "20260101000000")));
        tree.get_mut("/pub").unwrap().retain(|(n, _)| n != "empty");
        tree.remove("/pub/empty");
    }
    sync(&index, id, &remote, &Progress::default()).unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["docs", "movie one.mkv", "new.bin"]);
    assert_eq!(names(&index, index.child_named(root, "docs").unwrap()), ["ünï.md"]);
}

#[test]
fn a_cancelled_ftp_walk_is_interrupted_and_indexes_nothing() {
    let server = start(Style::Mlsd);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    let progress = Progress::default();
    progress.cancel.store(true, Ordering::Relaxed);
    let err = sync(&index, id, &remote, &progress).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    let index = index.read();
    assert!(names(&index, index.volume(id).unwrap().root).is_empty());
}

#[test]
fn a_server_without_mlsd_is_walked_by_parsing_unix_list_output() {
    let server = start(Style::Unix);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    sync(&index, id, &remote, &Progress::default()).unwrap();
    assert_indexed(&index, id, &server);
    let log = server.state.log.lock().unwrap();
    assert!(!log.iter().any(|l| l == "MLSD"), "FEAT did not offer MLST");
    assert!(log.iter().any(|l| l == "LIST"));
    drop(log);
    assert_read_only(&server);
    // The LIST time carries the year here, so it is exact (to the minute, midnight when only a date).
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    let movie = index.child_named(root, "movie one.mkv").unwrap();
    assert_eq!(index.modified(movie), 1_704_153_600_000_000_000);
}

#[test]
fn a_server_without_mlsd_is_walked_by_parsing_dos_list_output() {
    let server = start(Style::Dos);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    sync(&index, id, &remote, &Progress::default()).unwrap();
    assert_indexed(&index, id, &server);
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    let movie = index.child_named(root, "movie one.mkv").unwrap();
    // IIS lists to the minute: 2024-01-02 03:04.
    assert_eq!(index.modified(movie), 1_704_164_640_000_000_000);
}

#[test]
fn a_server_that_advertises_mlst_but_refuses_mlsd_falls_back_to_list() {
    let server = start(Style::MlsdBroken);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    sync(&index, id, &remote, &Progress::default()).unwrap();
    assert_indexed(&index, id, &server);
    let log = server.state.log.lock().unwrap();
    assert_eq!(log.iter().filter(|l| *l == "MLSD").count(), 1, "MLSD is tried once, then given up");
    assert!(log.iter().any(|l| l == "LIST"));
}

#[test]
fn an_unreadable_folder_is_counted_and_nothing_is_pruned_after_it() {
    let server = start(Style::Mlsd);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    sync(&index, id, &remote, &Progress::default()).unwrap();
    assert_eq!(index.read().live_count(), 6, "root, docs, empty, the movie and docs' two files");
    // docs becomes unreadable at the same time as a file elsewhere is deleted.
    server.state.forbidden.lock().unwrap().push("/pub/docs".into());
    server
        .state
        .tree
        .lock()
        .unwrap()
        .get_mut("/pub")
        .unwrap()
        .retain(|(n, _)| n != "movie one.mkv");
    let progress = Progress::default();
    sync(&index, id, &remote, &progress).unwrap();
    assert_eq!(progress.unreadable(), 1);
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        assert_eq!(
            names(&index, root),
            ["docs", "empty", "movie one.mkv"],
            "an incomplete listing prunes nothing"
        );
        assert_eq!(names(&index, index.child_named(root, "docs").unwrap()), ["a.txt", "ünï.md"]);
    }
    // Once the folder reads again the next complete listing prunes.
    server.state.forbidden.lock().unwrap().clear();
    sync(&index, id, &remote, &Progress::default()).unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["docs", "empty"]);
}

#[test]
fn a_wrong_password_a_missing_keyring_entry_and_a_missing_folder_say_what_to_do() {
    let server = start(Style::Mlsd);
    let (index, id, _) = add(&server, FtpTls::Plain);
    let err = sync_with(&index, id, &Progress::default(), &|_| Ok("wrong".into()), no_trust()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("rejected the login"), "{err}");
    let err = sync_with(
        &index,
        id,
        &Progress::default(),
        &|_| Err("the password is not in the OS keyring".into()),
        no_trust(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("keyring"), "{err}");
    assert_eq!(index.read().live_count(), 1);
    // The root must exist.
    let mut missing = remote(&server, FtpTls::Plain);
    missing.path = "/nope".into();
    let id = index.write().add_remote(&Remote::Ftp(missing));
    let err = sync_with(&index, id, &Progress::default(), &good, no_trust()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    // Nobody listening.
    let mut gone = remote(&server, FtpTls::Plain);
    gone.port = 1;
    let id = index.write().add_remote(&Remote::Ftp(gone));
    assert!(sync_with(&index, id, &Progress::default(), &good, no_trust()).is_err());
}

#[test]
fn a_check_logs_in_and_enters_the_folder_without_listing_it() {
    let server = start(Style::Mlsd);
    let good = remote(&server, FtpTls::Plain);
    ve_scan::ftp::check(&good, "hunter2", no_trust()).unwrap();
    assert!(
        !server
            .state
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("MLSD") || l.starts_with("LIST")),
        "a check lists nothing"
    );
    let err = ve_scan::ftp::check(&good, "wrong", no_trust()).unwrap_err();
    assert!(err.to_string().contains("rejected the login"), "{err}");
    let mut missing = good.clone();
    missing.path = "/nope".into();
    let err = ve_scan::ftp::check(&missing, "hunter2", no_trust()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
}

fn certified() -> (Arc<ServerConfig>, RootCertStore) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let der = key.cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.signing_key.serialize_der())),
        )
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(der).unwrap();
    (Arc::new(config), roots)
}

#[test]
fn explicit_ftps_secures_the_control_and_data_channels_and_is_refused_where_it_cannot_be() {
    let (server_config, roots) = certified();
    let server = start_with(Some(server_config), Style::Mlsd);
    let mut tls_remote = remote(&server, FtpTls::Explicit);
    // The certificate is for `localhost`.
    tls_remote.host = "localhost".into();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Ftp(tls_remote.clone()));
    // A certificate no root vouches for is a failure that names FTPS, and nothing was sent in the clear.
    let err = sync_with(&index, id, &Progress::default(), &good, no_trust()).unwrap_err();
    assert!(err.to_string().contains("FTPS handshake"), "{err}");
    assert!(
        server.state.passwords.lock().unwrap().is_empty(),
        "the password never went out unprotected"
    );
    // Trusted, it lists through TLS on both channels.
    let r = sync_with(&index, id, &Progress::default(), &good, client_config(roots));
    r.unwrap();
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        assert_eq!(names(&index, root), ["docs", "empty", "movie one.mkv"]);
        let docs = index.child_named(root, "docs").unwrap();
        assert_eq!(names(&index, docs), ["a.txt", "ünï.md"]);
    }
    let log = server.state.log.lock().unwrap().clone();
    let at = |needle: &str| {
        log.iter()
            .position(|l| l == needle)
            .unwrap_or_else(|| panic!("{needle} not sent: {log:?}"))
    };
    assert!(
        at("AUTH TLS") < at("USER me") && at("PROT P") < at("USER me"),
        "secured before the login"
    );
    drop(log);

    // A server that offers no FTPS is refused unless plain FTP was allowed on purpose.
    let plain_only = start(Style::Mlsd);
    let (index, id, remote) = add(&plain_only, FtpTls::Explicit);
    let err = sync(&index, id, &remote, &Progress::default()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("Allow plain FTP"), "{err}");
    assert!(plain_only.state.passwords.lock().unwrap().is_empty());
    assert_eq!(index.read().live_count(), 1);
}

#[test]
fn an_ftp_remote_survives_a_save_and_the_settings_shape_does_not_hold_the_password() {
    let remote = Remote::Ftp(FtpRemote::new("ftp.example.com", 2121, "me", "/pub", FtpTls::Plain).unwrap());
    let index = SharedIndex::new(Index::new());
    assert_eq!(ve_scan::reconcile_remotes(&index, std::slice::from_ref(&remote)).len(), 1);
    assert!(ve_scan::reconcile_remotes(&index, std::slice::from_ref(&remote)).is_empty());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.read().save(&path).unwrap();
    let loaded = Index::load(&path).unwrap();
    let (_, volume) = loaded.volumes().next().unwrap();
    assert_eq!(volume.remote(), Some(remote));
    assert_eq!(volume.identity, "ftp://me@ftp.example.com:2121/pub");
    assert!(volume.is_remote());
}

/// The password end to end through the real OS keyring. Needs a Secret Service / Keychain /
/// Credential Manager, so not run by default: `cargo test -p ve-scan --test ftp -- --ignored`.
#[test]
#[ignore]
fn the_password_comes_from_the_keyring_and_is_forgotten_with_the_remote() {
    let server = start(Style::Mlsd);
    let (index, id, remote) = add(&server, FtpTls::Plain);
    let identity = remote.identity();
    ve_scan::s3::store_secret(&identity, "hunter2").unwrap();
    let result = ve_scan::ftp::sync(&index, id, &Progress::default());
    ve_scan::ftp::forget_password(&remote);
    result.unwrap();
    assert_eq!(index.read().live_count(), 6);
    // With the password gone the sync says where it should have been.
    let err = ve_scan::ftp::sync(&index, id, &Progress::default()).unwrap_err();
    assert!(err.to_string().contains("keyring"), "{err}");
}
