//! S3 Remotes (ADR-0006): list a bucket (or a prefix of it) with ListObjectsV2, 1000 keys a page,
//! and apply the current objects to the Index as folders and files. Read-only: nothing but GET
//! list requests is ever made. Requests are presigned by `rusty-s3` and sent by blocking `ureq`.

use crate::remote::Applier;
use crate::{Progress, SharedIndex};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;
use ve_core::{EntryId, EntryMeta, S3Remote, SyncState, VolumeId};

/// Keys S3 returns per page, the most it allows.
const PAGE: usize = 1000;

/// Keyring service the secrets of typed keys are filed under; the account is the Remote's identity.
const SERVICE: &str = "view-everything";

fn invalid(text: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, text.into())
}

// ---- keyring ---------------------------------------------------------------------------------

fn entry(identity: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, identity).map_err(|e| format!("the OS keyring is not available: {e}"))
}

/// Keeps the secret of typed keys in the OS keyring (never in settings.toml).
pub fn store_secret(identity: &str, secret: &str) -> Result<(), String> {
    entry(identity)?
        .set_password(secret)
        .map_err(|e| format!("the OS keyring refused the secret: {e}"))
}

/// The secret kept for `identity`.
pub fn read_secret(identity: &str) -> Result<String, String> {
    entry(identity)?.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => "the secret key is not in the OS keyring: remove the Remote and add it again".into(),
        e => format!("the OS keyring could not be read: {e}"),
    })
}

/// Forgets the secret kept for `identity`; nothing to forget is fine.
pub fn forget_secret(identity: &str) {
    if let Ok(entry) = entry(identity) {
        let _ = entry.delete_credential();
    }
}

// ---- the standard credential chain -----------------------------------------------------------

/// `[section]` -> `key = value`, keys lowercased; `#`/`;` comments, no continuation lines.
type Ini = HashMap<String, HashMap<String, String>>;

fn parse_ini(text: &str) -> Ini {
    let mut ini = Ini::new();
    let mut section = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(name.trim().to_owned());
            ini.entry(name.trim().to_owned()).or_default();
        } else if let (Some(section), Some((k, v))) = (&section, line.split_once('=')) {
            ini.entry(section.clone())
                .or_default()
                .insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    ini
}

/// Where the standard chain reads from: environment variables and the two `~/.aws` files.
pub struct Chain<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub credentials_file: Option<String>,
    pub config_file: Option<String>,
}

impl Chain<'_> {
    fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|v| !v.trim().is_empty())
    }

    /// The settings of a profile: `~/.aws/config` (`[profile name]`, or `[default]`), overridden
    /// by `~/.aws/credentials` (`[name]`).
    fn profile(&self, name: &str) -> HashMap<String, String> {
        let config = parse_ini(self.config_file.as_deref().unwrap_or(""));
        let credentials = parse_ini(self.credentials_file.as_deref().unwrap_or(""));
        let config_section = if name == "default" {
            config.get("default")
        } else {
            config.get(&format!("profile {name}"))
        };
        let mut merged = config_section.cloned().unwrap_or_default();
        merged.extend(credentials.get(name).cloned().unwrap_or_default());
        merged
    }

    fn profile_name(&self, explicit: Option<&str>) -> String {
        explicit
            .map(str::to_owned)
            .or_else(|| self.var("AWS_PROFILE"))
            .unwrap_or_else(|| "default".into())
    }

    /// The region and endpoint a profile (or, failing that, the environment) names, to fill what
    /// the Add Remote dialog left empty.
    pub fn defaults(&self, profile: Option<&str>) -> Defaults {
        let p = self.profile(&self.profile_name(profile));
        Defaults {
            region: p
                .get("region")
                .cloned()
                .or_else(|| self.var("AWS_REGION"))
                .or_else(|| self.var("AWS_DEFAULT_REGION")),
            endpoint: p
                .get("endpoint_url")
                .cloned()
                .or_else(|| self.var("AWS_ENDPOINT_URL_S3"))
                .or_else(|| self.var("AWS_ENDPOINT_URL")),
        }
    }

    /// The keys of `remote`, or `None` for anonymous access. Order: keys typed into the dialog
    /// (secret from the keyring, through `secret`), the profile chosen in the dialog, the
    /// environment, then `AWS_PROFILE` or `default`.
    pub fn credentials(&self, remote: &S3Remote, secret: &dyn Fn() -> Result<String, String>) -> Result<Option<Credentials>, String> {
        if let Some(key) = &remote.access_key {
            return Ok(Some(Credentials::new(key.clone(), secret()?)));
        }
        if remote.profile.is_none()
            && let (Some(key), Some(secret)) = (self.var("AWS_ACCESS_KEY_ID"), self.var("AWS_SECRET_ACCESS_KEY"))
        {
            return Ok(Some(match self.var("AWS_SESSION_TOKEN") {
                Some(token) => Credentials::new_with_token(key, secret, token),
                None => Credentials::new(key, secret),
            }));
        }
        let name = self.profile_name(remote.profile.as_deref());
        let p = self.profile(&name);
        match (p.get("aws_access_key_id"), p.get("aws_secret_access_key")) {
            (Some(key), Some(secret)) => Ok(Some(match p.get("aws_session_token") {
                Some(token) => Credentials::new_with_token(key.clone(), secret.clone(), token.clone()),
                None => Credentials::new(key.clone(), secret.clone()),
            })),
            // A profile named in the dialog must exist; the implicit chain just ends in anonymous.
            _ if remote.profile.is_some() => Err(format!(
                "profile {name} has no aws_access_key_id and aws_secret_access_key in ~/.aws (role and SSO profiles are not supported)"
            )),
            _ => Ok(None),
        }
    }
}

/// Region and endpoint taken from a profile or the environment.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    pub region: Option<String>,
    pub endpoint: Option<String>,
}

/// Runs `f` with the chain read from the real environment and `~/.aws`.
pub fn with_system_chain<T>(f: impl FnOnce(&Chain) -> T) -> T {
    let env = |name: &str| std::env::var(name).ok();
    let home = std::env::home_dir().map(|h| h.join(".aws"));
    let read = |var: &str, file: &str| {
        let path = env(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(file)))?;
        std::fs::read_to_string(path).ok()
    };
    let chain = Chain {
        env: &env,
        credentials_file: read("AWS_SHARED_CREDENTIALS_FILE", "credentials"),
        config_file: read("AWS_CONFIG_FILE", "config"),
    };
    f(&chain)
}

// ---- listing ---------------------------------------------------------------------------------

/// A current object: its key relative to the Remote's prefix, size and modification time.
#[derive(Debug, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub modified: i64,
}

fn bucket(remote: &S3Remote) -> Result<Bucket, io::Error> {
    let endpoint = remote
        .endpoint
        .clone()
        .unwrap_or_else(|| format!("https://{}", remote.endpoint_host()));
    let url = url::Url::parse(&endpoint).map_err(|e| invalid(format!("endpoint {endpoint}: {e}")))?;
    let style = if remote.uses_path_style() {
        UrlStyle::Path
    } else {
        UrlStyle::VirtualHost
    };
    Bucket::new(url, style, remote.bucket.clone(), remote.region.clone()).map_err(|e| invalid(format!("S3 bucket: {e}")))
}

/// The text between `<tag>` and `</tag>` in an S3 error body.
pub(crate) fn xml_text<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let start = body.find(&open)? + open.len();
    Some(body[start..start + body[start..].find(&close)?].trim())
}

/// Why a request failed, in words for the user.
fn http_failure(remote: &S3Remote, status: u16, body: &str, anonymous: bool) -> io::Error {
    let host = remote.endpoint_host();
    let code = xml_text(body, "Code").unwrap_or("");
    let message = xml_text(body, "Message").unwrap_or("");
    let (kind, hint) = match status {
        301 | 400 if code.contains("Redirect") || code == "AuthorizationHeaderMalformed" => {
            (io::ErrorKind::InvalidInput, " (is the Region right for this bucket?)")
        }
        401 | 403 if anonymous => (
            io::ErrorKind::PermissionDenied,
            " (no keys were found: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, choose a profile, or enter keys)",
        ),
        401 | 403 => (
            io::ErrorKind::PermissionDenied,
            " (check the keys and that they may list this bucket)",
        ),
        404 => (
            io::ErrorKind::NotFound,
            " (check the bucket name, and the Endpoint and path-style option)",
        ),
        _ => (io::ErrorKind::Other, ""),
    };
    io::Error::new(kind, format!("{host} answered {status} {code} {message}{hint}").replace("  ", " "))
}

pub(crate) fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        // The signature is in the URL: never forward it to another host.
        .max_redirects(0)
}

pub(crate) fn agent() -> ureq::Agent {
    agent_config().build().into()
}

/// `prefix/` of the Remote, as keys under it start; empty for the whole bucket.
fn key_prefix(remote: &S3Remote) -> String {
    if remote.prefix.is_empty() {
        String::new()
    } else {
        format!("{}/", remote.prefix)
    }
}

/// One ListObjectsV2 page of at most `max_keys` keys under `prefix`, after `token`.
fn fetch_page(
    agent: &ureq::Agent,
    bucket: &Bucket,
    remote: &S3Remote,
    credentials: Option<&Credentials>,
    prefix: &str,
    token: Option<&str>,
    max_keys: usize,
) -> io::Result<rusty_s3::actions::list_objects_v2::ListObjectsV2Response> {
    let mut action = bucket.list_objects_v2(credentials);
    action.with_max_keys(max_keys);
    if !prefix.is_empty() {
        action.with_prefix(prefix);
    }
    if let Some(token) = token {
        action.with_continuation_token(token);
    }
    let url = action.sign(Duration::from_secs(300));
    let mut response = agent.get(url.as_str()).call().map_err(|e| connection_failure(remote, &e))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| io::Error::other(format!("{}: {e}", remote.endpoint_host())))?;
    if status != 200 {
        return Err(http_failure(remote, status, &body, credentials.is_none()));
    }
    rusty_s3::actions::ListObjectsV2::parse_response(&body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unreadable listing from {}: {e}", remote.endpoint_host()),
        )
    })
}

/// Why the endpoint could not be reached, in words for the user.
fn connection_failure(remote: &S3Remote, error: &ureq::Error) -> io::Error {
    let host = remote.endpoint_host();
    // TLS spoken to a server without it fails in the handshake, in words about records.
    let text = error.to_string();
    let tls = matches!(error, ureq::Error::Tls(_) | ureq::Error::Rustls(_)) || text.contains("corrupt message");
    let message = match &remote.endpoint {
        Some(e) if tls && e.starts_with("https://") => {
            format!("{host} did not answer over TLS; if it has none, start the Endpoint with http:// ({text})")
        }
        _ => format!("{host}: {text}"),
    };
    io::Error::new(io::ErrorKind::ConnectionRefused, message)
}

/// Lists every current object of `remote`, a page at a time, giving each page to `page`.
pub fn list(remote: &S3Remote, credentials: Option<&Credentials>, mut page: impl FnMut(Vec<Object>) -> io::Result<()>) -> io::Result<()> {
    let bucket = bucket(remote)?;
    let prefix = key_prefix(remote);
    let agent = agent();
    let mut token: Option<String> = None;
    let mut tokens = HashSet::new();
    loop {
        let parsed = fetch_page(&agent, &bucket, remote, credentials, &prefix, token.as_deref(), PAGE)?;
        let objects = parsed
            .contents
            .into_iter()
            .filter_map(|c| {
                let key = c.key.strip_prefix(prefix.as_str())?.to_owned();
                let modified = c
                    .last_modified
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
                Some(Object {
                    key,
                    size: c.size.unwrap_or(0),
                    modified: modified.and_then(|t| t.timestamp_nanos_opt()).unwrap_or(0),
                })
            })
            .collect();
        page(objects)?;
        match parsed.next_continuation_token {
            Some(next) if tokens.insert(next.clone()) => token = Some(next),
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} repeats a continuation token", remote.endpoint_host()),
                ));
            }
            None => return Ok(()),
        }
    }
}

/// The Test of the Remote dialog: `check` with the typed secret, else the credential chain.
pub fn check_remote(r: &S3Remote, secret: Option<&str>) -> io::Result<()> {
    let credentials = match (secret, &r.access_key) {
        (Some(secret), Some(key)) => Some(Credentials::new(key.clone(), secret.to_owned())),
        _ => with_system_chain(|chain| chain.credentials(r, &|| read_secret(&r.identity())))
            .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?,
    };
    check(r, credentials.as_ref())
}

/// Lists a single key of `remote`: proof that the endpoint answers, the bucket is there and the
/// keys may list it, without reading the whole bucket.
pub fn check(remote: &S3Remote, credentials: Option<&Credentials>) -> io::Result<()> {
    let bucket = bucket(remote)?;
    fetch_page(&agent(), &bucket, remote, credentials, &key_prefix(remote), None, 1).map(|_| ())
}

/// The folders and file `objects` stand for, parents first. `seen` holds the folders already
/// produced (`true`) and the files (`false`); a key whose path is both a file and a folder (`a`
/// and `a/b`) cannot be Entries and is skipped. Keys with an empty name in their path (`a//b`) cannot be Entries and are counted in
/// the second result; a key ending in `/` is a folder marker and makes just its folders.
pub(crate) fn entries(objects: Vec<Object>, seen: &mut HashMap<String, bool>) -> (Vec<(String, EntryMeta)>, u64) {
    let (mut out, mut skipped) = (Vec::new(), 0);
    for object in objects {
        // The prefix's own marker object is empty once the prefix is stripped.
        if object.key.is_empty() {
            continue;
        }
        let folders = object.key.trim_end_matches('/');
        let marker = folders.len() != object.key.len();
        if folders.is_empty() || folders.split('/').any(str::is_empty) {
            skipped += 1;
            continue;
        }
        let parts: Vec<&str> = folders.split('/').collect();
        let last = if marker { parts.len() } else { parts.len() - 1 };
        let mut path = String::new();
        let mut clash = false;
        for part in &parts[..last] {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            match seen.get(&path) {
                Some(true) => {}
                Some(false) => clash = true,
                None => {
                    seen.insert(path.clone(), true);
                    out.push((
                        path.clone(),
                        EntryMeta {
                            is_dir: true,
                            ..EntryMeta::default()
                        },
                    ));
                }
            }
        }
        if !marker && seen.get(folders) == Some(&true) {
            clash = true;
        }
        if clash {
            skipped += 1;
            continue;
        }
        if !marker {
            seen.insert(folders.to_owned(), false);
            let meta = EntryMeta {
                logical: object.size,
                on_disk: object.size,
                modified: object.modified,
                ..EntryMeta::default()
            };
            out.push((folders.to_owned(), meta));
        }
    }
    (out, skipped)
}

/// Brings an S3 Remote's Entries up to date with what its bucket lists now.
pub fn sync(index: &SharedIndex, volume: VolumeId, progress: &Progress) -> io::Result<()> {
    sync_with(index, volume, progress, &|remote| {
        with_system_chain(|chain| chain.credentials(remote, &|| read_secret(&remote.identity())))
    })
}

/// [`sync`] with `auth` giving the keys (a test double, or the real chain).
pub fn sync_with(
    index: &SharedIndex,
    volume: VolumeId,
    progress: &Progress,
    auth: &dyn Fn(&S3Remote) -> Result<Option<Credentials>, String>,
) -> io::Result<()> {
    let (root, remote): (EntryId, S3Remote) = {
        let index = index.read();
        let v = index
            .volume(volume)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such Remote"))?;
        match &v.sync {
            SyncState::S3(remote) => (v.root, remote.clone()),
            _ => return Err(invalid("not an S3 Remote")),
        }
    };
    let credentials = auth(&remote).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    let mut applier = Applier::new(index, root, progress);
    let mut seen = HashMap::new();
    list(&remote, credentials.as_ref(), |objects| {
        let (mut batch, skipped) = entries(objects, &mut seen);
        progress.unreadable.fetch_add(skipped, Ordering::Relaxed);
        applier.apply(&mut batch);
        Ok(())
    })?;
    // Only a complete listing says what is gone; an error above leaves the old Entries alone.
    applier.prune();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(profile: &str, key: &str) -> S3Remote {
        S3Remote::new("bkt", "pre/fix/", "", "eu-west-1", false, profile, key).unwrap()
    }

    #[test]
    fn identity_is_endpoint_host_bucket_prefix() {
        assert_eq!(remote("", "").identity(), "s3:s3.eu-west-1.amazonaws.com/bkt/pre/fix");
        let r2 = S3Remote::new("b", "", "https://acct.r2.cloudflarestorage.com/", "auto", true, "", "").unwrap();
        assert_eq!(r2.identity(), "s3:acct.r2.cloudflarestorage.com/b/");
        let minio = S3Remote::new("b", "/x/", "localhost:9000", "us-east-1", true, "", "").unwrap();
        assert_eq!(
            (minio.endpoint.as_deref(), minio.identity().as_str()),
            (Some("https://localhost:9000"), "s3:localhost:9000/b/x")
        );
        assert!(S3Remote::new("a/b", "", "", "us-east-1", false, "", "").is_err());
        assert!(S3Remote::new("b", "", "ftp://x", "us-east-1", false, "", "").is_err());
    }

    #[test]
    fn keys_split_on_slash_into_folders_and_markers_make_folders() {
        let object = |key: &str, size| Object {
            key: key.into(),
            size,
            modified: 5,
        };
        let mut seen = HashMap::new();
        let (items, skipped) = entries(
            vec![
                object("a/b/c.txt", 10),
                object("a/d.txt", 20),
                object("empty/", 0),
                object("top.bin", 7),
                object("x//y", 1),
                object("", 0),
            ],
            &mut seen,
        );
        let paths: Vec<_> = items.iter().map(|(p, m)| (p.as_str(), m.is_dir, m.logical, m.on_disk)).collect();
        assert_eq!(
            paths,
            [
                ("a", true, 0, 0),
                ("a/b", true, 0, 0),
                ("a/b/c.txt", false, 10, 10),
                ("a/d.txt", false, 20, 20),
                ("empty", true, 0, 0),
                ("top.bin", false, 7, 7)
            ]
        );
        assert_eq!(skipped, 1);
        // A later page does not repeat folders.
        let (items, _) = entries(vec![object("a/e.txt", 1)], &mut seen);
        assert_eq!(items.len(), 1);
        // A file and a folder of one name: the first one wins, the other is skipped.
        let mut seen = HashMap::new();
        let (items, skipped) = entries(vec![object("f", 1), object("f/g", 2), object("d/h", 3), object("d", 4)], &mut seen);
        assert_eq!(
            (items.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), skipped),
            (["f", "d", "d/h"].to_vec(), 2)
        );
    }

    #[test]
    fn the_chain_prefers_typed_keys_then_the_profile_then_the_environment_then_default() {
        let env = |name: &str| match name {
            "AWS_ACCESS_KEY_ID" => Some("ENVKEY".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("envsecret".to_owned()),
            "AWS_SESSION_TOKEN" => Some("tok".to_owned()),
            _ => None,
        };
        let chain = Chain {
            env: &env,
            credentials_file: Some("[default]\naws_access_key_id = DEF\naws_secret_access_key = defsecret\n\n[work]\naws_access_key_id=WORK\naws_secret_access_key=worksecret\n".into()),
            config_file: Some("[profile work]\nregion = eu-north-1\nendpoint_url = https://minio.lan:9000\n".into()),
        };
        let none = || Err("no secret".to_owned());
        let key = |c: Option<Credentials>| c.map(|c| c.key().to_owned());
        // Typed keys read their secret through the keyring.
        let typed = remote("", "TYPED");
        assert_eq!(chain.credentials(&typed, &|| Ok("s".into())).map(key), Ok(Some("TYPED".into())));
        assert!(chain.credentials(&typed, &none).is_err());
        // A profile chosen in the dialog beats the environment.
        assert_eq!(chain.credentials(&remote("work", ""), &none).map(key), Ok(Some("WORK".into())));
        assert!(chain.credentials(&remote("nope", ""), &none).is_err());
        // Otherwise the environment (with its token), then [default].
        let c = chain.credentials(&remote("", ""), &none).unwrap().unwrap();
        assert_eq!((c.key(), c.token()), ("ENVKEY", Some("tok")));
        let no_env = |_: &str| None;
        let chain = Chain { env: &no_env, ..chain };
        assert_eq!(chain.credentials(&remote("", ""), &none).map(key), Ok(Some("DEF".into())));
        // Region and endpoint come from the profile.
        assert_eq!(
            chain.defaults(Some("work")),
            Defaults {
                region: Some("eu-north-1".into()),
                endpoint: Some("https://minio.lan:9000".into())
            }
        );
        assert_eq!(chain.defaults(None), Defaults::default());
        let empty = Chain {
            env: &no_env,
            credentials_file: None,
            config_file: None,
        };
        assert_eq!(empty.credentials(&remote("", ""), &none), Ok(None));
    }

    #[test]
    fn errors_read_as_what_to_do() {
        let r = remote("", "");
        let why = |status, body: &str, anon| http_failure(&r, status, body, anon).to_string();
        assert!(
            why(
                403,
                "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
                true
            )
            .contains("no keys were found")
        );
        assert!(why(403, "<Error><Code>SignatureDoesNotMatch</Code></Error>", false).contains("SignatureDoesNotMatch (check the keys"));
        assert!(why(301, "<Error><Code>PermanentRedirect</Code></Error>", false).contains("Region"));
        assert!(why(404, "<Error><Code>NoSuchBucket</Code></Error>", false).contains("NoSuchBucket (check the bucket"));
    }

    /// Needs a running Secret Service / Keychain / Credential Manager, so not run by default:
    /// `cargo test -p ve-scan -- --ignored keyring`.
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let id = "s3:test.invalid/keyring-round-trip/";
        store_secret(id, "s3cr3t/with+chars").unwrap();
        assert_eq!(read_secret(id).unwrap(), "s3cr3t/with+chars");
        forget_secret(id);
        assert!(read_secret(id).is_err());
    }
}
