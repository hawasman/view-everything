//! WebDAV Remotes: Basic auth with the password in the OS keyring, walking the server folder by
//! folder with `PROPFIND Depth: 1`, since many servers refuse `Depth: infinity`.

use crate::remote::Applier;
use crate::s3::{agent_config, forget_secret, read_secret};
use crate::xml::{self, Ev};
use crate::{Progress, SharedIndex};
use base64::Engine as _;
use percent_encoding::percent_decode_str;
use std::collections::HashSet;
use std::io;
use std::sync::atomic::Ordering;
use ve_core::{EntryId, EntryMeta, SyncState, VolumeId, WebdavRemote};

/// Entries applied to the Index per write lock.
const BATCH: usize = 4096;

const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?><propfind xmlns="DAV:"><prop><resourcetype/><getcontentlength/><getlastmodified/></prop></propfind>"#;

/// One `response` of a multistatus: a resource the server lists.
#[derive(Debug, Default, PartialEq, Eq)]
struct Item {
    href: String,
    is_dir: bool,
    size: u64,
    /// Nanoseconds since the Unix epoch.
    modified: i64,
}

fn parse_multistatus(body: &str) -> Result<Vec<Item>, String> {
    let (mut items, mut current) = (Vec::new(), None::<Item>);
    xml::walk(body, |event| match (event, current.as_mut()) {
        (Ev::Start("response"), _) => current = Some(Item::default()),
        (Ev::Start("collection"), Some(item)) => item.is_dir = true,
        (Ev::End("href", t), Some(item)) => item.href = t.into(),
        (Ev::End("getcontentlength", t), Some(item)) => item.size = t.parse().unwrap_or(0),
        (Ev::End("getlastmodified", t), Some(item)) => {
            item.modified = chrono::DateTime::parse_from_rfc2822(t)
                .ok()
                .and_then(|t| t.timestamp_nanos_opt())
                .unwrap_or(0)
        }
        (Ev::End("response", _), _) => items.extend(current.take()),
        _ => {}
    })?;
    Ok(items)
}

/// PROPFIND is not a standard method to ureq; redirects are refused so credentials stay on one host.
fn agent() -> ureq::Agent {
    agent_config().allow_non_standard_methods(true).build().into()
}

/// The password kept in the keyring; a Remote without a user has none to keep.
pub(crate) fn password(remote: &WebdavRemote) -> Result<String, String> {
    match remote.user.is_empty() {
        true => Ok(String::new()),
        false => read_secret(&remote.identity()).map_err(|e| e.replace("the secret key is", "the password is")),
    }
}

/// The `scheme://host[:port]` and the (encoded) path of the Remote's folder.
fn split(remote: &WebdavRemote) -> io::Result<(String, String)> {
    let url = url::Url::parse(&remote.url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}: {e}", remote.url)))?;
    Ok((url.origin().ascii_serialization(), url.path().to_owned()))
}

fn host(remote: &WebdavRemote) -> String {
    let rest = remote.url.split_once("://").map_or(remote.url.as_str(), |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_owned()
}

/// `PROPFIND` of `url` at `depth`; the multistatus body, or why the server would not give one.
fn propfind(agent: &ureq::Agent, remote: &WebdavRemote, password: &str, url: &str, depth: &str) -> io::Result<Vec<Item>> {
    let mut request = ureq::http::Request::builder()
        .method("PROPFIND")
        .uri(url)
        .header("Depth", depth)
        .header("Content-Type", "application/xml");
    if !remote.user.is_empty() {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{password}", remote.user));
        request = request.header("Authorization", format!("Basic {basic}"));
    }
    let request = request
        .body(PROPFIND)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{url}: {e}")))?;
    let host = host(remote);
    let mut response = agent
        .run(request)
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, format!("{host}: {e}")))?;
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|l| l.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = response
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| io::Error::other(format!("{host}: {e}")))?;
    let (kind, why) = match status {
        207 => {
            return parse_multistatus(&body)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("unreadable listing from {host}: {e}")));
        }
        401 | 403 => (io::ErrorKind::PermissionDenied, " (check the user and password)".to_owned()),
        404 => (io::ErrorKind::NotFound, " (is the URL a folder on the server?)".to_owned()),
        301 | 302 | 307 | 308 => (io::ErrorKind::InvalidInput, format!(", redirecting to {location}: use that URL")),
        _ => (io::ErrorKind::Other, String::new()),
    };
    Err(io::Error::new(kind, format!("{host} answered {status}{why}")))
}

/// A server path as the server named it: `href` may be a full URL or a path, relative to the
/// folder `from` if it has no leading `/`. Returns the encoded path and the decoded one.
fn href_path(href: &str, from: &str) -> (String, String) {
    let path = match url::Url::parse(href) {
        Ok(url) => url.path().to_owned(),
        Err(_) => href.split(['?', '#']).next().unwrap_or("").to_owned(),
    };
    let path = if path.starts_with('/') { path } else { format!("{from}{path}") };
    let decoded = percent_decode_str(&path).decode_utf8_lossy().into_owned();
    (path, decoded)
}

/// A folder's URL path ends in `/`: without it many servers redirect.
fn slash(path: &str) -> String {
    if path.ends_with('/') { path.to_owned() } else { format!("{path}/") }
}

/// Walks the Remote's folders, calling `emit` with each Entry's path under the root. Returns how
/// many folders or names could not be read. A failure of the root itself is an error.
fn walk(agent: &ureq::Agent, remote: &WebdavRemote, password: &str, mut emit: impl FnMut(String, EntryMeta)) -> io::Result<u64> {
    let (origin, root) = split(remote)?;
    let mut stack = vec![(slash(&root), String::new())];
    let (mut seen, mut unreadable) = (HashSet::new(), 0);
    while let Some((request, rel)) = stack.pop() {
        let (_, folder) = href_path(&request, "/");
        if !seen.insert(folder.clone()) {
            continue;
        }
        let items = match propfind(agent, remote, password, &format!("{origin}{request}"), "1") {
            Ok(items) => items,
            Err(e) if rel.is_empty() => return Err(e),
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        let folder = folder.trim_end_matches('/');
        for item in items {
            let (encoded, decoded) = href_path(&item.href, &request);
            let decoded = decoded.trim_end_matches('/');
            if decoded == folder {
                continue;
            }
            let name = decoded.strip_prefix(folder).and_then(|r| r.strip_prefix('/'));
            let Some(name) = name.filter(|n| !n.is_empty() && !n.contains('/') && !matches!(*n, "." | "..")) else {
                unreadable += 1;
                continue;
            };
            let path = if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
            let is_dir = item.is_dir || item.href.ends_with('/');
            let meta = EntryMeta {
                is_dir,
                logical: if is_dir { 0 } else { item.size },
                on_disk: if is_dir { 0 } else { item.size },
                modified: item.modified,
                ..EntryMeta::default()
            };
            if is_dir {
                stack.push((slash(&encoded), path.clone()));
            }
            emit(path, meta);
        }
    }
    Ok(unreadable)
}

/// Brings a WebDAV Remote's Entries up to date with what its server lists now.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    sync_with(index, volume, progress, &password)
}

/// [`sync`] with `auth` giving the password (a test double, or the keyring).
pub fn sync_with(
    index: &SharedIndex,
    volume: VolumeId,
    progress: &Progress,
    auth: &dyn Fn(&WebdavRemote) -> Result<String, String>,
) -> io::Result<()> {
    let (root, remote): (EntryId, WebdavRemote) = {
        let index = index.read();
        let v = index
            .volume(volume)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Remote"))?;
        match &v.sync {
            SyncState::Webdav(remote) => (v.root, remote.clone()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a WebDAV Remote")),
        }
    };
    let password = auth(&remote).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    let mut applier = Applier::new(index, root, progress);
    let mut batch = Vec::new();
    let walked = walk(&agent(), &remote, &password, |path, meta| {
        batch.push((path, meta));
        if batch.len() >= BATCH {
            applier.apply(&mut batch);
        }
    });
    applier.apply(&mut batch);
    let unreadable = walked?;
    progress.unreadable.fetch_add(unreadable, Ordering::Relaxed);
    // Only a walk that reached every folder says what is gone.
    if unreadable == 0 {
        applier.prune();
    }
    Ok(())
}

/// Opens the Remote's folder with `secret` (a password typed in but not stored yet; `None` uses
/// the keyring) without reading below it: the Test of the Remote dialog.
pub fn check_remote(r: &WebdavRemote, secret: Option<&str>) -> io::Result<()> {
    let password = match secret {
        Some(p) => p.to_owned(),
        None => password(r).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?,
    };
    let (origin, root) = split(r)?;
    propfind(&agent(), r, &password, &format!("{origin}{}", slash(&root)), "0").map(|_| ())
}

/// Forgets the password kept for `remote`.
pub fn forget_password(remote: &WebdavRemote) {
    forget_secret(&remote.identity());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multistatus_is_read_whatever_its_prefixes_and_entities() {
        let items = parse_multistatus(
            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
 <d:response><d:href>/dav/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/dav/a%20&amp;b.txt</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>12</d:getcontentlength><d:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</d:getlastmodified></d:prop></d:propstat></d:response>
</d:multistatus>"#,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_dir && items[0].size == 0);
        assert_eq!(
            (items[1].href.as_str(), items[1].is_dir, items[1].size, items[1].modified),
            ("/dav/a%20&b.txt", false, 12, 784_111_777_000_000_000)
        );
    }

    #[test]
    fn hrefs_are_paths_whether_absolute_or_relative() {
        assert_eq!(href_path("https://h/dav/a%20b/", "/x/"), ("/dav/a%20b/".into(), "/dav/a b/".into()));
        assert_eq!(href_path("/dav/c?x=1", "/x/"), ("/dav/c".into(), "/dav/c".into()));
        assert_eq!(href_path("d", "/dav/"), ("/dav/d".into(), "/dav/d".into()));
    }
}
