//! WebDAV Remotes against an in-process stand-in for a WebDAV server: it answers `PROPFIND` with
//! Depth 0 or 1 (and refuses `infinity`, as many servers do), checks Basic auth, and can fail a folder.

use base64::Engine as _;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use ve_core::{EntryId, Index, Remote, WebdavRemote};
use ve_scan::{Progress, SharedIndex, webdav};

#[derive(Default)]
struct State {
    /// Decoded path (folders end in `/`) -> size of a file, `None` for a folder.
    tree: Mutex<Vec<(String, Option<u64>)>>,
    /// (Depth header, had a body) of every request.
    requests: Mutex<Vec<(String, bool)>>,
    /// The `Authorization` a request must carry.
    require: Mutex<Option<String>>,
    /// Folders that answer 500.
    broken: Mutex<Vec<String>>,
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

fn encode(path: &str) -> String {
    path.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn respond(stream: &mut TcpStream, status: &str, extra: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\n{extra}Content-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn serve(state: &State, port: u16, mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').unwrap();
        headers.insert(k.to_lowercase(), v.trim().to_owned());
    }
    let length: usize = headers.get("content-length").map_or(0, |l| l.parse().unwrap());
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut parts = request_line.split_whitespace();
    let (method, target) = (parts.next().unwrap(), parts.next().unwrap());
    let depth = headers.get("depth").cloned().unwrap_or_default();
    state.requests.lock().unwrap().push((depth.clone(), length > 0));
    if method != "PROPFIND" {
        return respond(&mut stream, "405 Method Not Allowed", "", "");
    }
    if let Some(want) = state.require.lock().unwrap().as_ref()
        && headers.get("authorization") != Some(want)
    {
        return respond(&mut stream, "401 Unauthorized", "WWW-Authenticate: Basic realm=\"x\"\r\n", "");
    }
    let path = percent_encoding::percent_decode_str(target).decode_utf8_lossy().into_owned();
    let tree = state.tree.lock().unwrap();
    let Some((_, own)) = tree.iter().find(|(p, _)| *p == path) else {
        return respond(&mut stream, "404 Not Found", "", "");
    };
    if state.broken.lock().unwrap().contains(&path) {
        return respond(&mut stream, "500 Internal Server Error", "", "");
    }
    let mut shown = vec![(&path, own)];
    match depth.as_str() {
        "0" => {}
        "1" => shown.extend(tree.iter().filter_map(|(p, s)| {
            let rest = p.strip_prefix(&path)?;
            let name = rest.trim_end_matches('/');
            (!name.is_empty() && !name.contains('/')).then_some((p, s))
        })),
        _ => return respond(&mut stream, "403 Forbidden", "", "<error>propfind-finite-depth</error>"),
    }
    let mut xml = String::from(r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">"#);
    for (p, size) in shown {
        // Some servers answer with absolute URLs, some with paths.
        let href = if p.starts_with("/dav/sub/") {
            format!("http://127.0.0.1:{port}{}", encode(p))
        } else {
            encode(p)
        };
        let props = match size {
            None => "<D:resourcetype><D:collection/></D:resourcetype>".to_owned(),
            Some(n) => format!(
                "<D:resourcetype/><D:getcontentlength>{n}</D:getcontentlength><D:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</D:getlastmodified>"
            ),
        };
        xml += &format!(
            "<D:response><D:href>{}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
            href.replace('&', "&amp;")
        );
    }
    xml += "</D:multistatus>";
    respond(&mut stream, "207 Multi-Status", "", &xml);
}

fn start() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(State::default());
    *state.tree.lock().unwrap() = [
        ("/dav/", None),
        ("/dav/a.txt", Some(10)),
        ("/dav/ünï & co.txt", Some(3)),
        ("/dav/sub/", None),
        ("/dav/sub/b c.bin", Some(20)),
        ("/dav/sub/deep/", None),
        ("/dav/sub/deep/d", Some(5)),
        ("/other/x", Some(99)),
    ]
    .map(|(p, s)| (p.to_owned(), s))
    .into();
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(stream) = stream {
                serve(&shared, port, stream);
            }
        }
    });
    Server { port, state }
}

fn remote(server: &Server, user: &str) -> WebdavRemote {
    WebdavRemote::new(&format!("http://127.0.0.1:{}/dav/", server.port), user).unwrap()
}

fn basic(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

fn names(index: &Index, dir: EntryId) -> Vec<String> {
    let mut v: Vec<_> = index.children(dir).iter().map(|&c| index.name(c).to_owned()).collect();
    v.sort();
    v
}

#[test]
fn a_cancelled_webdav_walk_is_interrupted_and_indexes_nothing() {
    let server = start();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Webdav(remote(&server, "")));
    let progress = Progress::default();
    progress.cancel.store(true, Ordering::Relaxed);
    let err = webdav::sync_with(&index, id, &progress, &|_| Ok(String::new())).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    let index = index.read();
    assert!(names(&index, index.volume(id).unwrap().root).is_empty());
}

#[test]
fn folders_are_walked_with_depth_one_and_a_refresh_applies_differences() {
    let server = start();
    *server.state.require.lock().unwrap() = Some(basic("me", "pw"));
    let remote = remote(&server, "me");
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Webdav(remote));
    let progress = Progress::default();
    webdav::sync_with(&index, id, &progress, &|_| Ok("pw".into())).unwrap();

    // One request per folder (/dav/, sub, deep), each Depth 1 with a body: never `infinity`.
    assert_eq!(*server.state.requests.lock().unwrap(), vec![("1".to_owned(), true); 3]);
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        assert_eq!(names(&index, root), ["a.txt", "sub", "ünï & co.txt"]);
        let sub = index.child_named(root, "sub").unwrap();
        assert_eq!(names(&index, sub), ["b c.bin", "deep"]);
        let d = index.child_named(index.child_named(sub, "deep").unwrap(), "d").unwrap();
        assert_eq!((index.logical(d), index.on_disk(d)), (5, 5));
        assert_eq!(index.modified(d), 784_111_777_000_000_000);
        assert_eq!(index.logical(index.child_named(root, "ünï & co.txt").unwrap()), 3);
        assert_eq!(progress.unreadable(), 0);
    }

    // A later walk drops what is gone and picks up what changed.
    {
        let mut tree = server.state.tree.lock().unwrap();
        tree.retain(|(p, _)| p != "/dav/a.txt" && !p.starts_with("/dav/sub/deep"));
        tree.iter_mut().find(|(p, _)| p == "/dav/sub/b c.bin").unwrap().1 = Some(21);
        tree.push(("/dav/new.bin".into(), Some(7)));
    }
    webdav::sync_with(&index, id, &Progress::default(), &|_| Ok("pw".into())).unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["new.bin", "sub", "ünï & co.txt"]);
    let sub = index.child_named(root, "sub").unwrap();
    assert_eq!(names(&index, sub), ["b c.bin"]);
    assert_eq!(index.logical(index.child_named(sub, "b c.bin").unwrap()), 21);
}

#[test]
fn a_folder_that_cannot_be_read_stops_the_prune() {
    let server = start();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Webdav(remote(&server, "")));
    webdav::sync_with(&index, id, &Progress::default(), &|_| Ok(String::new())).unwrap();
    assert_eq!(index.read().live_count(), 7, "root, sub, deep, 4 files");
    server.state.broken.lock().unwrap().push("/dav/sub/".into());
    server.state.tree.lock().unwrap().retain(|(p, _)| p != "/dav/a.txt");
    let progress = Progress::default();
    webdav::sync_with(&index, id, &progress, &|_| Ok(String::new())).unwrap();
    assert_eq!(progress.unreadable(), 1);
    assert_eq!(index.read().live_count(), 7, "nothing is dropped on a partial walk");
}

#[test]
fn a_wrong_password_a_missing_folder_and_a_dead_server_explain_themselves() {
    let server = start();
    *server.state.require.lock().unwrap() = Some(basic("me", "pw"));
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Webdav(remote(&server, "me")));
    let err = webdav::sync_with(&index, id, &Progress::default(), &|_| Ok("nope".into())).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("401") && err.to_string().contains("password"), "{err}");
    let err = webdav::sync_with(&index, id, &Progress::default(), &|_| {
        Err("the password is not in the OS keyring".into())
    })
    .unwrap_err();
    assert!(err.to_string().contains("keyring"));
    assert_eq!(index.read().live_count(), 1);

    // Test: Depth 0 on the folder, with the password typed in.
    webdav::check_remote(&remote(&server, "me"), Some("pw")).unwrap();
    assert_eq!(server.state.requests.lock().unwrap().last().unwrap().0, "0");
    let err = webdav::check_remote(&remote(&server, "me"), Some("nope")).unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    *server.state.require.lock().unwrap() = None;
    let missing = WebdavRemote::new(&format!("http://127.0.0.1:{}/nothing", server.port), "").unwrap();
    let err = webdav::check_remote(&missing, None).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let gone = WebdavRemote::new("http://127.0.0.1:1/dav", "").unwrap();
    assert!(webdav::check_remote(&gone, None).is_err());
}
