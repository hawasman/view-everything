//! S3 Remotes against an in-process stand-in for an S3-compatible server (no minio binary is
//! needed): it answers path-style ListObjectsV2 with a small page size, checks that requests are
//! signed with the expected access key, and can be told to fail.

use rusty_s3::Credentials;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use ve_core::{EntryId, Filter, Index, Remote, S3Remote};
use ve_scan::{Progress, SharedIndex, s3};

/// Objects per page the stand-in returns, whatever `max-keys` asks.
const SERVER_PAGE: usize = 2;

#[derive(Default)]
struct State {
    /// key, size, LastModified
    objects: Mutex<Vec<(String, u64, String)>>,
    /// The query of every request.
    requests: Mutex<Vec<Vec<(String, String)>>>,
    /// Access key a signed request must carry; anonymous requests are refused when set.
    require_key: Mutex<Option<String>>,
    /// Fail every request after this many have been answered.
    fail_after: AtomicUsize,
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

fn encode(key: &str) -> String {
    key.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

fn serve(state: &State, mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let url = url::Url::parse(&format!("http://stand-in{target}")).unwrap();
    let query: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let get = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let answered = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(query.clone());
        requests.len() - 1
    };
    let failing = state.fail_after.load(Ordering::Relaxed);
    if failing > 0 && answered >= failing {
        return respond(&mut stream, "500 Internal Server Error", "<Error><Code>InternalError</Code><Message>boom</Message></Error>");
    }
    if url.path() != "/bkt" && url.path() != "/bkt/" {
        return respond(&mut stream, "404 Not Found", "<Error><Code>NoSuchBucket</Code><Message>no such bucket</Message></Error>");
    }
    if let Some(key) = state.require_key.lock().unwrap().as_ref() {
        let credential = get("X-Amz-Credential").unwrap_or_default();
        if !credential.starts_with(&format!("{key}/")) || get("X-Amz-Signature").is_none() {
            return respond(&mut stream, "403 Forbidden", "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>");
        }
    }
    assert_eq!(get("list-type").as_deref(), Some("2"));
    let prefix = get("prefix").unwrap_or_default();
    let after = get("continuation-token");
    let objects = state.objects.lock().unwrap();
    let mut matching: Vec<_> = objects.iter().filter(|(k, ..)| k.starts_with(&prefix)).collect();
    matching.sort();
    // The continuation token is the last key of the previous page.
    let rest: Vec<_> = matching.into_iter().filter(|(k, ..)| after.as_ref().is_none_or(|a| k > a)).collect();
    let page = &rest[..rest.len().min(SERVER_PAGE)];
    let truncated = rest.len() > SERVER_PAGE;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bkt</Name>");
    xml += &format!("<Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{SERVER_PAGE}</MaxKeys><EncodingType>url</EncodingType>", encode(&prefix), page.len());
    xml += &format!("<IsTruncated>{truncated}</IsTruncated>");
    if truncated {
        xml += &format!("<NextContinuationToken>{}</NextContinuationToken>", page.last().unwrap().0);
    }
    for (key, size, modified) in page {
        xml += &format!("<Contents><Key>{}</Key><LastModified>{modified}</LastModified><ETag>&quot;x&quot;</ETag><Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>", encode(key));
    }
    xml += "</ListBucketResult>";
    respond(&mut stream, "200 OK", &xml);
}

fn start(objects: &[(&str, u64, &str)]) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(State::default());
    *state.objects.lock().unwrap() = objects.iter().map(|&(k, s, m)| (k.to_owned(), s, m.to_owned())).collect();
    let shared = state.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(stream) = stream {
                serve(&shared, stream);
            }
        }
    });
    Server { port, state }
}

fn remote(server: &Server, prefix: &str) -> S3Remote {
    S3Remote::new("bkt", prefix, &format!("http://127.0.0.1:{}", server.port), "us-east-1", true, "", "AKIATEST").unwrap()
}

fn keys(_: &S3Remote) -> Result<Option<Credentials>, String> {
    Ok(Some(Credentials::new("AKIATEST", "secret")))
}

fn names(index: &Index, dir: EntryId) -> Vec<String> {
    let mut v: Vec<_> = index.children(dir).iter().map(|&c| index.name(c).to_owned()).collect();
    v.sort();
    v
}

const OBJECTS: [(&str, u64, &str); 7] = [
    ("photos/2024/a.jpg", 1000, "2024-01-02T03:04:05.000Z"),
    ("photos/2024/b b.jpg", 2000, "2024-01-03T00:00:00.000Z"),
    ("photos/2025/ünï.png", 300, "2025-06-01T12:00:00.500Z"),
    ("photos/readme.txt", 5, "2023-12-31T23:59:59.000Z"),
    ("photos/empty/", 0, "2024-01-01T00:00:00.000Z"),
    ("photos/a//bad", 1, "2024-01-01T00:00:00.000Z"),
    ("other/outside.bin", 99, "2024-01-01T00:00:00.000Z"),
];

#[test]
fn a_bucket_prefix_is_indexed_page_by_page_and_a_refresh_applies_differences() {
    let server = start(&OBJECTS);
    let remote = Remote::S3(remote(&server, "photos"));
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&remote);
    let progress = Progress::default();
    s3::sync_with(&index, id, &progress, &keys).unwrap();

    // 6 objects under photos/ at 2 a page is 3 pages, each signed and asking for 1000 keys and the prefix.
    {
        let requests = server.state.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for q in requests.iter() {
            let get = |n: &str| q.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
            assert_eq!((get("max-keys"), get("prefix")), (Some("1000"), Some("photos/")));
        }
        assert!(requests[0].iter().all(|(k, _)| k != "continuation-token"));
        assert!(requests[1].iter().any(|(k, _)| k == "continuation-token"));
    }
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        assert_eq!(names(&index, root), ["2024", "2025", "empty", "readme.txt"]);
        let a = index.child_named(index.child_named(root, "2024").unwrap(), "a.jpg").unwrap();
        // Logical Size is the object size, On-disk Size equals it, the time is LastModified.
        assert_eq!((index.logical(a), index.on_disk(a)), (1000, 1000));
        assert_eq!(index.modified(a), 1_704_164_645_000_000_000);
        let uni = index.child_named(index.child_named(root, "2025").unwrap(), "ünï.png").unwrap();
        assert_eq!(index.modified(uni), 1_748_779_200_500_000_000);
        assert_eq!(names(&index, index.child_named(root, "2024").unwrap()), ["a.jpg", "b b.jpg"]);
        assert_eq!(names(&index, index.child_named(root, "empty").unwrap()), Vec::<String>::new());
        let totals = Filter::compute(&index, None).totals(&index, root);
        assert_eq!((totals.logical, totals.on_disk), (3305, 3305));
        assert_eq!(index.path(a).to_string_lossy(), format!("s3:127.0.0.1:{}/bkt/photos/2024/a.jpg", server.port));
        // The key with an empty name is counted as unreadable, not indexed.
        assert_eq!(progress.unreadable(), 1);
    }

    // A failure part-way leaves the Entries as they were.
    server.state.fail_after.store(server.state.requests.lock().unwrap().len() + 1, Ordering::Relaxed);
    let err = s3::sync_with(&index, id, &Progress::default(), &keys).unwrap_err();
    assert!(err.to_string().contains("500") && err.to_string().contains("InternalError"), "{err}");
    assert_eq!(index.read().live_count(), 8, "root, 3 folders, 4 files");
    server.state.fail_after.store(0, Ordering::Relaxed);

    // A later listing drops what is gone and picks up what changed.
    {
        let mut objects = server.state.objects.lock().unwrap();
        objects.retain(|(k, ..)| k != "photos/2024/a.jpg" && k != "photos/empty/");
        objects.iter_mut().find(|(k, ..)| k == "photos/readme.txt").unwrap().1 = 50;
        objects.push(("photos/new/c.bin".into(), 7, "2026-01-01T00:00:00.000Z".into()));
    }
    s3::sync_with(&index, id, &Progress::default(), &keys).unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, root), ["2024", "2025", "new", "readme.txt"]);
    assert_eq!(index.logical(index.child_named(root, "readme.txt").unwrap()), 50);
    assert_eq!(names(&index, index.child_named(root, "2024").unwrap()), ["b b.jpg"]);
}

#[test]
fn the_whole_bucket_is_indexed_when_the_prefix_is_empty() {
    let server = start(&OBJECTS);
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::S3(remote(&server, "")));
    s3::sync_with(&index, id, &Progress::default(), &keys).unwrap();
    let index = index.read();
    assert_eq!(names(&index, index.volume(id).unwrap().root), ["other", "photos"]);
}

#[test]
fn credentials_are_signed_in_and_a_wrong_key_or_none_explains_itself() {
    let server = start(&OBJECTS);
    *server.state.require_key.lock().unwrap() = Some("AKIATEST".into());
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::S3(remote(&server, "photos")));
    let wrong = |_: &S3Remote| Ok(Some(Credentials::new("AKIAWRONG", "secret")));
    let err = s3::sync_with(&index, id, &Progress::default(), &wrong).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("AccessDenied") && err.to_string().contains("check the keys"), "{err}");
    let err = s3::sync_with(&index, id, &Progress::default(), &|_| Ok(None)).unwrap_err();
    assert!(err.to_string().contains("no keys were found"), "{err}");
    // A missing secret in the keyring is reported, not guessed around.
    let err = s3::sync_with(&index, id, &Progress::default(), &|_| Err("the secret key is not in the OS keyring".into())).unwrap_err();
    assert!(err.to_string().contains("keyring"));
    assert_eq!(index.read().live_count(), 1);
    // Anonymous access to a server that allows it.
    *server.state.require_key.lock().unwrap() = None;
    s3::sync_with(&index, id, &Progress::default(), &|_| Ok(None)).unwrap();
    assert_eq!(index.read().live_count(), 8, "root, 3 folders, 4 files");
    let last = server.state.requests.lock().unwrap().last().unwrap().clone();
    assert!(last.iter().all(|(k, _)| k != "X-Amz-Signature"));
}

#[test]
fn a_missing_bucket_and_an_unreachable_endpoint_are_reported() {
    let server = start(&OBJECTS);
    let index = SharedIndex::new(Index::new());
    let mut missing = remote(&server, "");
    missing.bucket = "nope".into();
    let id = index.write().add_remote(&Remote::S3(missing));
    let err = s3::sync_with(&index, id, &Progress::default(), &keys).unwrap_err();
    assert!(err.to_string().contains("NoSuchBucket") && err.to_string().contains("bucket name"), "{err}");
    let mut gone = remote(&server, "");
    gone.endpoint = Some("http://127.0.0.1:1".into());
    let id = index.write().add_remote(&Remote::S3(gone));
    assert!(s3::sync_with(&index, id, &Progress::default(), &keys).is_err());
}

#[test]
fn an_s3_remote_survives_a_save_and_settings_style_round_trip() {
    let remote = Remote::S3(S3Remote::new("b", "p/q", "minio.lan:9000", "us-east-1", true, "work", "AKIA").unwrap());
    let index = SharedIndex::new(Index::new());
    assert_eq!(ve_scan::reconcile_remotes(&index, std::slice::from_ref(&remote)).len(), 1);
    assert!(ve_scan::reconcile_remotes(&index, std::slice::from_ref(&remote)).is_empty());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.bin");
    index.read().save(&path).unwrap();
    let loaded = Index::load(&path).unwrap();
    let (_, volume) = loaded.volumes().next().unwrap();
    assert_eq!(volume.remote(), Some(remote));
    assert_eq!(volume.identity, "s3:minio.lan:9000/b/p/q");
    // Dropped from the wanted list, the Remote goes.
    assert!(volume.is_remote());
    let loaded = SharedIndex::new(loaded);
    ve_scan::reconcile_remotes(&loaded, &[]);
    assert_eq!(loaded.read().volumes().count(), 0);
}

/// Typed keys end to end through the real OS keyring. Needs a Secret Service / Keychain /
/// Credential Manager, so not run by default: `cargo test -p ve-scan --test s3 -- --ignored`.
#[test]
#[ignore]
fn typed_keys_take_their_secret_from_the_keyring() {
    let server = start(&OBJECTS);
    *server.state.require_key.lock().unwrap() = Some("AKIATEST".into());
    let remote = remote(&server, "photos");
    let identity = remote.identity();
    s3::store_secret(&identity, "typed-secret").unwrap();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::S3(remote));
    let result = s3::sync(&index, id, &Progress::default());
    s3::forget_secret(&identity);
    result.unwrap();
    assert_eq!(index.read().live_count(), 8);
    // With the secret gone the sync says where it should have been.
    let err = s3::sync(&index, id, &Progress::default()).unwrap_err();
    assert!(err.to_string().contains("OS keyring"), "{err}");
}
