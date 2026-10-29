//! Update checks against GitHub Releases, and the minisign verification that self-updates build on
//! (ADR-0007).

use serde::Deserialize;
use std::time::Duration;

/// The release signing key (`packaging/minisign.pub`); every release asset carries a `.minisig` for it.
const PUBLIC_KEY: &str = include_str!("../../../packaging/minisign.pub");
const LATEST: &str = "https://api.github.com/repos/hawasman/view-everything/releases/latest";
const DAY: i64 = 24 * 60 * 60;

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// A published release newer than the running app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// The release notes page ("What's new").
    pub notes_url: String,
}

#[derive(Deserialize)]
struct Latest {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

/// Where to ask: GitHub, or (measuring builds) `VE_UPDATE_URL`, a mock server.
pub fn latest_url() -> String {
    if cfg!(feature = "bench")
        && let Ok(url) = std::env::var("VE_UPDATE_URL")
    {
        return url;
    }
    LATEST.to_owned()
}

/// Whether a check is due: none yet, or the last one is a day old (or in the future: the clock moved back).
pub fn due(last_check: Option<i64>, now: i64) -> bool {
    last_check.is_none_or(|t| t > now || now - t >= DAY)
}

fn parse(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.trim_start_matches('v').split('.').map(|p| p.parse().ok());
    let found = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(found)
}

/// Whether `latest` is a later version than `current` (plain `x.y.z`, optionally `v`-prefixed).
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// Asks `url` for the latest release: `Some` when it is a newer, non-pre-release version.
pub fn check(url: &str, current: &str) -> Result<Option<Release>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("User-Agent", "view-everything")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?;
    let body = response.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let latest: Latest = serde_json::from_str(&body).map_err(|e| format!("unexpected reply: {e}"))?;
    let version = latest.tag_name.trim_start_matches('v').to_owned();
    if parse(&version).is_none() {
        return Err(format!("unexpected version {}", latest.tag_name));
    }
    let newer = !latest.prerelease && !latest.draft && is_newer(&version, current);
    Ok(newer.then_some(Release {
        version,
        notes_url: latest.html_url,
    }))
}

/// Accepts `file` only when `signature` (the text of its `.minisig`) was made with the key `public_key`
/// (the text of a minisign `.pub` file).
pub fn verify(public_key: &str, file: &[u8], signature: &str) -> Result<(), String> {
    let key = minisign_verify::PublicKey::decode(public_key).map_err(|e| format!("bad public key: {e}"))?;
    let signature = minisign_verify::Signature::decode(signature).map_err(|e| format!("bad signature: {e}"))?;
    key.verify(file, &signature, false)
        .map_err(|e| format!("signature does not match: {e}"))
}

/// [`verify`] with the release key built into the app.
#[allow(dead_code)] // the per-platform self-update slices call it on a downloaded asset
pub fn verify_release(file: &[u8], signature: &str) -> Result<(), String> {
    verify(PUBLIC_KEY, file, signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};
    use std::net::TcpListener;

    /// A throwaway key pair (tests only): the public key text and the `.minisig` text for `file`.
    fn sign(file: &[u8]) -> (String, String) {
        let minisign::KeyPair { pk, sk } = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let signature = minisign::sign(Some(&pk), &sk, Cursor::new(file), None, None).unwrap();
        (pk.to_box().unwrap().to_string(), signature.to_string())
    }

    #[test]
    fn verify_accepts_signed_rejects_tampered_and_wrong_key() {
        let file = b"view-everything release";
        let (key, signature) = sign(file);
        assert!(verify(&key, file, &signature).is_ok());
        assert!(verify(&key, b"view-everything releasE", &signature).is_err());
        let (other_key, _) = sign(file);
        assert!(verify(&other_key, file, &signature).is_err());
        assert!(verify(&key, file, "not a signature").is_err());
        // The key built into the app parses.
        assert!(verify_release(file, &signature).is_err());
        assert!(minisign_verify::PublicKey::decode(PUBLIC_KEY).is_ok());
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("0.2.0-rc1", "0.1.0"));
    }

    #[test]
    fn checks_are_due_once_a_day() {
        assert!(due(None, 1_000_000));
        assert!(!due(Some(1_000_000), 1_000_000 + DAY - 1));
        assert!(due(Some(1_000_000), 1_000_000 + DAY));
        assert!(due(Some(2_000_000), 1_000_000));
    }

    /// Serves `body` once on 127.0.0.1 and returns the URL.
    fn serve(status: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/latest", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        url
    }

    const NEWER: &str = r#"{"tag_name":"v0.2.0","html_url":"https://example.test/v0.2.0","prerelease":false}"#;

    #[test]
    fn newer_release_is_reported_with_its_notes_link() {
        let found = check(&serve("200 OK", NEWER), "0.1.0").unwrap();
        assert_eq!(
            found,
            Some(Release {
                version: "0.2.0".into(),
                notes_url: "https://example.test/v0.2.0".into()
            })
        );
    }

    #[test]
    fn equal_older_and_pre_releases_show_nothing() {
        assert_eq!(check(&serve("200 OK", NEWER), "0.2.0").unwrap(), None);
        assert_eq!(check(&serve("200 OK", NEWER), "0.3.0").unwrap(), None);
        let pre = r#"{"tag_name":"v0.9.0","html_url":"https://example.test/x","prerelease":true}"#;
        assert_eq!(check(&serve("200 OK", pre), "0.1.0").unwrap(), None);
    }

    #[test]
    fn failures_are_errors() {
        assert!(check(&serve("500 Internal Server Error", "{}"), "0.1.0").is_err());
        assert!(check(&serve("200 OK", "not json"), "0.1.0").is_err());
        // Nothing listens: bind, learn the port, drop.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(check(&format!("http://127.0.0.1:{port}/"), "0.1.0").is_err());
    }
}
