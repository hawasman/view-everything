//! Azure Blob Remotes against an in-process stand-in for the service (Azurite-style endpoint, with
//! the account in the path): it pages List Blobs, and checks the SharedKey signature or SAS token.

use base64::Engine as _;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use ve_core::{AzureRemote, EntryId, Index, Remote};
use ve_scan::{Progress, SharedIndex, azure};

const ACCOUNT: &str = "devstoreaccount1";
const KEY: &[u8] = b"not-a-real-account-key";
/// Blobs per page the stand-in returns, whatever `maxresults` asks.
const SERVER_PAGE: usize = 2;

#[derive(Default)]
struct State {
    /// name, size, Last-Modified
    blobs: Mutex<Vec<(String, u64, String)>>,
    /// The query pairs of every request.
    requests: Mutex<Vec<Vec<(String, String)>>>,
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

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn error(code: &str) -> String {
    format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>nope\nRequestId:1</Message></Error>")
}

fn serve(state: &State, mut stream: TcpStream) {
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
    let target = request_line.split_whitespace().nth(1).unwrap().to_owned();
    let url = url::Url::parse(&format!("http://stand-in{target}")).unwrap();
    let query: Vec<(String, String)> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let get = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    state.requests.lock().unwrap().push(query.clone());
    if url.path() != format!("/{ACCOUNT}/c") {
        return respond(&mut stream, "404 Not Found", &error("ContainerNotFound"));
    }
    // Authorised by a SAS token, or by a SharedKey signature recomputed here from the request.
    let allowed = if let Some(sig) = get("sig") {
        sig == "sastoken" && get("sp").as_deref() == Some("rl")
    } else {
        let mut canonical: Vec<String> = query.iter().map(|(k, v)| format!("{}:{v}", k.to_lowercase())).collect();
        canonical.sort();
        let text = format!(
            "GET{}x-ms-date:{}\nx-ms-version:{}\n/{ACCOUNT}{}\n{}",
            "\n".repeat(12),
            headers.get("x-ms-date").cloned().unwrap_or_default(),
            headers.get("x-ms-version").cloned().unwrap_or_default(),
            url.path(),
            canonical.join("\n")
        );
        let signature = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, KEY), text.as_bytes());
        let want = format!(
            "SharedKey {ACCOUNT}:{}",
            base64::engine::general_purpose::STANDARD.encode(signature.as_ref())
        );
        headers.get("authorization") == Some(&want)
    };
    if !allowed {
        return respond(&mut stream, "403 Forbidden", &error("AuthenticationFailed"));
    }
    assert_eq!(
        (get("restype").as_deref(), get("comp").as_deref()),
        (Some("container"), Some("list"))
    );
    let prefix = get("prefix").unwrap_or_default();
    let after = get("marker");
    let blobs = state.blobs.lock().unwrap();
    let mut matching: Vec<_> = blobs.iter().filter(|(k, ..)| k.starts_with(&prefix)).collect();
    matching.sort();
    // The marker is the last name of the previous page.
    let rest: Vec<_> = matching
        .into_iter()
        .filter(|(k, ..)| after.as_ref().is_none_or(|a| k > a))
        .collect();
    let page = &rest[..rest.len().min(SERVER_PAGE)];
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ServiceEndpoint=\"x\" ContainerName=\"c\"><Prefix>{}</Prefix><Blobs>",
        prefix.replace('&', "&amp;")
    );
    for (name, size, modified) in page {
        xml += &format!(
            "<Blob><Name>{}</Name><Properties><Last-Modified>{modified}</Last-Modified><Content-Length>{size}</Content-Length><BlobType>BlockBlob</BlobType></Properties></Blob>",
            name.replace('&', "&amp;")
        );
    }
    xml += "</Blobs>";
    match (rest.len() > SERVER_PAGE, page.last()) {
        (true, Some((last, ..))) => xml += &format!("<NextMarker>{}</NextMarker>", last.replace('&', "&amp;")),
        _ => xml += "<NextMarker />",
    }
    xml += "</EnumerationResults>";
    respond(&mut stream, "200 OK", &xml);
}

fn start() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(State::default());
    let modified = "Mon, 01 Jan 2024 00:00:00 GMT";
    *state.blobs.lock().unwrap() = [
        ("photos/2024/a.jpg", 1000),
        ("photos/2024/b & b.jpg", 2000),
        ("photos/readme.txt", 5),
        ("photos/empty/", 0),
        ("photos/a//bad", 1),
        ("other/outside.bin", 99),
    ]
    .map(|(k, s)| (k.to_owned(), s, modified.to_owned()))
    .into();
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

fn remote(server: &Server, prefix: &str, sas: bool) -> AzureRemote {
    AzureRemote::new(ACCOUNT, "c", prefix, &format!("http://127.0.0.1:{}/{ACCOUNT}", server.port), sas).unwrap()
}

fn key() -> String {
    base64::engine::general_purpose::STANDARD.encode(KEY)
}

fn names(index: &Index, dir: EntryId) -> Vec<String> {
    let mut v: Vec<_> = index.children(dir).map(|c| index.name(c).to_owned()).collect();
    v.sort();
    v
}

#[test]
fn a_cancelled_azure_listing_is_interrupted_and_indexes_nothing() {
    let server = start();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Azure(remote(&server, "photos", false)));
    let progress = Progress::default();
    progress.cancel.store(true, Ordering::Relaxed);
    let err = azure::sync_with(&index, id, &progress, &|_| Ok(key())).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    let index = index.read();
    assert!(names(&index, index.volume(id).unwrap().root).is_empty());
}

#[test]
fn a_container_prefix_is_indexed_page_by_page_with_a_signed_key_and_refreshed() {
    let server = start();
    let index = SharedIndex::new(Index::new());
    let id = index.write().add_remote(&Remote::Azure(remote(&server, "photos", false)));
    let progress = Progress::default();
    azure::sync_with(&index, id, &progress, &|_| Ok(key())).unwrap();

    // 4 blobs under photos/ (one with an empty name part) at 2 a page: 3 requests, the last empty-handed.
    assert_eq!(server.state.requests.lock().unwrap().len(), 3);
    {
        let index = index.read();
        let root = index.volume(id).unwrap().root;
        assert_eq!(names(&index, root), ["2024", "empty", "readme.txt"]);
        let sub = index.child_named(root, "2024").unwrap();
        assert_eq!(names(&index, sub), ["a.jpg", "b & b.jpg"]);
        let a = index.child_named(sub, "a.jpg").unwrap();
        assert_eq!((index.logical(a), index.on_disk(a)), (1000, 1000));
        assert_eq!(index.modified(a), 1_704_067_200_000_000_000);
        assert_eq!(progress.unreadable(), 1);
    }
    server
        .state
        .blobs
        .lock()
        .unwrap()
        .retain(|(k, ..)| k != "photos/2024/a.jpg" && k != "photos/readme.txt");
    azure::sync_with(&index, id, &Progress::default(), &|_| Ok(key())).unwrap();
    let index = index.read();
    let root = index.volume(id).unwrap().root;
    assert_eq!(names(&index, index.child_named(root, "2024").unwrap()), ["b & b.jpg"]);
    assert_eq!(names(&index, root), ["2024", "empty"]);
}

#[test]
fn a_sas_token_is_sent_as_is_and_bad_secrets_explain_themselves() {
    let server = start();
    let index = SharedIndex::new(Index::new());
    let sas = remote(&server, "", true);
    let id = index.write().add_remote(&Remote::Azure(sas.clone()));
    azure::sync_with(&index, id, &Progress::default(), &|_| Ok("?sv=2021&sp=rl&sig=sastoken".into())).unwrap();
    assert_eq!(names(&index.read(), index.read().volume(id).unwrap().root), ["other", "photos"]);

    let err = azure::sync_with(&index, id, &Progress::default(), &|_| Ok("sp=r&sig=wrong".into())).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        err.to_string().contains("AuthenticationFailed") && err.to_string().contains("SAS token"),
        "{err}"
    );
    assert!(!err.to_string().contains("RequestId"), "{err}");

    let account = remote(&server, "", false);
    let wrong = base64::engine::general_purpose::STANDARD.encode(b"another key");
    let err = azure::check_remote(&account, Some(&wrong)).unwrap_err();
    assert!(
        err.to_string().contains("403") && err.to_string().contains("account name and key"),
        "{err}"
    );
    let err = azure::check_remote(&account, Some("%%% not base64")).unwrap_err();
    assert!(err.to_string().contains("base64"), "{err}");
    let err = azure::sync_with(&index, id, &Progress::default(), &|_| {
        Err("the SAS token is not in the OS keyring".into())
    })
    .unwrap_err();
    assert!(err.to_string().contains("keyring"));
}

#[test]
fn test_lists_one_blob_and_reports_a_missing_container_or_dead_endpoint() {
    let server = start();
    azure::check_remote(&remote(&server, "photos", false), Some(&key())).unwrap();
    let last = server.state.requests.lock().unwrap().last().unwrap().clone();
    assert!(last.contains(&("maxresults".into(), "1".into())));
    let mut missing = remote(&server, "", false);
    missing.container = "nope".into();
    let err = azure::check_remote(&missing, Some(&key())).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("ContainerNotFound"), "{err}");
    let mut gone = remote(&server, "", false);
    gone.endpoint = Some("http://127.0.0.1:1".into());
    assert!(azure::check_remote(&gone, Some(&key())).is_err());
}
