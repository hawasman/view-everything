//! Azure Blob Remotes: a container listed flat with List Blobs (like S3), authorised by the
//! account key (SharedKey, signed here) or a SAS token; the secret is in the OS keyring.

use crate::remote::Applier;
use crate::s3::{Object, agent, entries, forget_secret, read_secret, xml_text};
use crate::xml::{self, Ev};
use crate::{Progress, SharedIndex};
use base64::Engine as _;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::Ordering;
use ve_core::{AzureRemote, EntryId, SyncState, VolumeId};

/// Blobs per page.
const PAGE: usize = 1000;

/// The service version the requests (and so their signature) are made for.
const VERSION: &str = "2021-08-06";

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The password kept in the keyring: the account key or the SAS token.
pub(crate) fn secret(remote: &AzureRemote) -> Result<String, String> {
    let what = if remote.sas { "the SAS token" } else { "the account key" };
    read_secret(&remote.identity()).map_err(|e| e.replace("the secret key is", &format!("{what} is")))
}

/// One page of List Blobs.
#[derive(Debug, Default, PartialEq, Eq)]
struct Page {
    objects: Vec<Object>,
    next: Option<String>,
}

fn parse_page(body: &str, prefix: &str) -> Result<Page, String> {
    let (mut page, mut key, mut size, mut modified, mut in_blob) = (Page::default(), String::new(), 0, 0, false);
    xml::walk(body, |event| match event {
        Ev::Start("Blob") => (in_blob, key, size, modified) = (true, String::new(), 0, 0),
        Ev::End("Name", t) if in_blob => key = t.into(),
        Ev::End("Content-Length", t) if in_blob => size = t.parse().unwrap_or(0),
        Ev::End("Last-Modified", t) if in_blob => {
            modified = chrono::DateTime::parse_from_rfc2822(t)
                .ok()
                .and_then(|t| t.timestamp_nanos_opt())
                .unwrap_or(0)
        }
        Ev::End("Blob", _) => {
            in_blob = false;
            if let Some(key) = key.strip_prefix(prefix) {
                page.objects.push(Object {
                    key: key.into(),
                    size,
                    modified,
                });
            }
        }
        Ev::End("NextMarker", t) if !t.is_empty() => page.next = Some(t.into()),
        _ => {}
    })?;
    Ok(page)
}

/// `prefix/` of the Remote, as blob names under it start; empty for the whole container.
fn name_prefix(remote: &AzureRemote) -> String {
    match remote.prefix.is_empty() {
        true => String::new(),
        false => format!("{}/", remote.prefix),
    }
}

/// The `Authorization` of a `GET` of `path` with `params` (SharedKey): the account key signs the
/// canonical form of the verb, the `x-` headers, the account and path, and the sorted query.
fn shared_key(account: &str, key: &[u8], date: &str, path: &str, params: &[(&str, &str)]) -> String {
    let mut params = params.to_vec();
    params.sort();
    // The verb, then the eleven standard headers (none set) one to a line.
    let mut text = format!("GET{}x-ms-date:{date}\nx-ms-version:{VERSION}\n/{account}{path}", "\n".repeat(12));
    for (name, value) in params {
        text.push_str(&format!("\n{name}:{value}"));
    }
    let signature = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), text.as_bytes());
    format!("SharedKey {account}:{}", B64.encode(signature.as_ref()))
}

/// Why a request failed, in words for the user.
fn http_failure(host: &str, status: u16, body: &str, sas: bool) -> io::Error {
    let code = xml_text(body, "Code").unwrap_or("");
    let message = xml_text(body, "Message").unwrap_or("").lines().next().unwrap_or("");
    let (kind, hint) = match status {
        401 | 403 if sas => (
            io::ErrorKind::PermissionDenied,
            " (check the SAS token: it needs list permission and must not be expired)",
        ),
        401 | 403 => (io::ErrorKind::PermissionDenied, " (check the account name and key)"),
        404 => (io::ErrorKind::NotFound, " (check the container name, and the Endpoint)"),
        _ => (io::ErrorKind::Other, ""),
    };
    io::Error::new(kind, format!("{host} answered {status} {code} {message}{hint}").replace("  ", " "))
}

/// One List Blobs page of at most `max` blobs under `prefix`, after `marker`.
fn fetch_page(agent: &ureq::Agent, remote: &AzureRemote, secret: &str, prefix: &str, marker: Option<&str>, max: usize) -> io::Result<Page> {
    let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
    let url = format!("{}/{}", remote.base_url().trim_end_matches('/'), remote.container);
    let parsed = url::Url::parse(&url).map_err(|e| invalid(format!("endpoint {url}: {e}")))?;
    let host = parsed.host_str().unwrap_or_default().to_owned();
    let max = max.to_string();
    let mut params = vec![("restype", "container"), ("comp", "list"), ("maxresults", max.as_str())];
    if !prefix.is_empty() {
        params.push(("prefix", prefix));
    }
    if let Some(marker) = marker {
        params.push(("marker", marker));
    }
    let mut query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, NON_ALPHANUMERIC)))
        .collect();
    let mut request_headers = vec![("x-ms-version", VERSION.to_owned())];
    if remote.sas {
        query.push(secret.trim().trim_start_matches('?').to_owned());
    } else {
        let key = B64.decode(secret.trim()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the account key is not valid base64: copy it again from the portal",
            )
        })?;
        let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        request_headers.push(("Authorization", shared_key(&remote.account, &key, &date, parsed.path(), &params)));
        request_headers.push(("x-ms-date", date));
    }
    let mut request = agent.get(format!("{url}?{}", query.join("&")));
    for (name, value) in request_headers {
        request = request.header(name, value);
    }
    let mut response = request
        .call()
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, format!("{host}: {e}")))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| io::Error::other(format!("{host}: {e}")))?;
    if status != 200 {
        return Err(http_failure(&host, status, &body, remote.sas));
    }
    parse_page(&body, prefix).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("unreadable listing from {host}: {e}")))
}

/// Lists every blob of `remote`, a page at a time, giving each page to `page`.
fn list(remote: &AzureRemote, secret: &str, mut page: impl FnMut(Vec<Object>) -> io::Result<()>) -> io::Result<()> {
    let (agent, prefix) = (agent(), name_prefix(remote));
    let (mut marker, mut markers) = (None::<String>, HashSet::new());
    loop {
        let parsed = fetch_page(&agent, remote, secret, &prefix, marker.as_deref(), PAGE)?;
        page(parsed.objects)?;
        match parsed.next {
            Some(next) if markers.insert(next.clone()) => marker = Some(next),
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the server repeats a continuation marker",
                ));
            }
            None => return Ok(()),
        }
    }
}

/// Lists a single blob of `remote`: proof that the endpoint answers, the container is there and
/// the secret may list it, without reading the whole container. `secret` is typed in but not
/// stored yet; `None` uses the keyring.
pub fn check_remote(r: &AzureRemote, secret: Option<&str>) -> io::Result<()> {
    let secret = match secret {
        Some(s) => s.to_owned(),
        None => secret_of(r)?,
    };
    fetch_page(&agent(), r, &secret, &name_prefix(r), None, 1).map(|_| ())
}

fn secret_of(r: &AzureRemote) -> io::Result<String> {
    secret(r).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
}

/// Brings an Azure Blob Remote's Entries up to date with what its container lists now.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    sync_with(index, volume, progress, &secret)
}

/// [`sync`] with `auth` giving the secret (a test double, or the keyring).
pub fn sync_with(
    index: &SharedIndex,
    volume: VolumeId,
    progress: &Progress,
    auth: &dyn Fn(&AzureRemote) -> Result<String, String>,
) -> io::Result<()> {
    let (root, remote): (EntryId, AzureRemote) = {
        let index = index.read();
        let v = index
            .volume(volume)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Remote"))?;
        match &v.sync {
            SyncState::Azure(remote) => (v.root, remote.clone()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "not an Azure Blob Remote")),
        }
    };
    let secret = auth(&remote).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    let mut applier = Applier::new(index, root, progress);
    let mut seen = HashMap::new();
    list(&remote, &secret, |objects| {
        let (mut batch, skipped) = entries(objects, &mut seen);
        progress.unreadable.fetch_add(skipped, Ordering::Relaxed);
        applier.apply(&mut batch);
        Ok(())
    })?;
    // Only a complete listing says what is gone; an error above leaves the old Entries alone.
    applier.prune();
    Ok(())
}

/// Forgets the secret kept for `remote`.
pub fn forget(remote: &AzureRemote) {
    forget_secret(&remote.identity());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_read_with_its_prefix_stripped_and_its_marker() {
        let page = parse_page(
            r#"<?xml version="1.0"?><EnumerationResults ServiceEndpoint="x" ContainerName="c"><Prefix>p/</Prefix><Blobs>
<Blob><Name>p/a &amp; b.txt</Name><Properties><Last-Modified>Sun, 06 Nov 1994 08:49:37 GMT</Last-Modified><Content-Length>12</Content-Length></Properties></Blob>
<Blob><Name>other</Name><Properties><Content-Length>1</Content-Length></Properties></Blob>
</Blobs><NextMarker>m1</NextMarker></EnumerationResults>"#,
            "p/",
        )
        .unwrap();
        assert_eq!(
            page,
            Page {
                objects: vec![Object {
                    key: "a & b.txt".into(),
                    size: 12,
                    modified: 784_111_777_000_000_000
                }],
                next: Some("m1".into()),
            }
        );
    }

    #[test]
    fn shared_key_signs_the_canonical_request() {
        // Computed independently: HMAC-SHA256 (key "key") over the documented string-to-sign.
        let text = format!(
            "GET{}x-ms-date:D\nx-ms-version:{VERSION}\n/acct/c\ncomp:list\nrestype:container",
            "\n".repeat(12)
        );
        let want = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"key"), text.as_bytes());
        assert_eq!(
            shared_key("acct", b"key", "D", "/c", &[("restype", "container"), ("comp", "list")]),
            format!("SharedKey acct:{}", B64.encode(want.as_ref()))
        );
    }
}
