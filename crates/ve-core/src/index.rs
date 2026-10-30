use crate::column::{Rank, Seq, SparseColumn, U64Column};
use crate::owner::{Owner, OwnerId};
use hashbrown::HashTable;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Source of Index generations, shared by every Index in the process.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Identifies one Entry within an [`Index`]. Stable for the lifetime of the in-memory Index;
/// ids are reassigned when an Index is saved and loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId(pub u32);

impl EntryId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VolumeId(pub u32);

/// Everything the Index records about an Entry apart from its name and place in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EntryMeta {
    pub is_dir: bool,
    /// Logical Size in bytes.
    pub logical: u64,
    /// On-disk Size in bytes.
    pub on_disk: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: i64,
    /// Creation time in nanoseconds since the Unix epoch; 0 when the platform or Remote does not report it.
    pub created: i64,
    /// Filesystem identity of the underlying file: the inode on Unix, the MFT record number on
    /// NTFS. Entries sharing a key on one Volume are hard links to one file.
    pub key: u64,
    /// More than one name refers to the underlying file.
    pub hardlinked: bool,
}

/// An SSH Remote: a folder on another machine, indexed over `ssh` without mounting it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SshRemote {
    /// `user@host` or an `~/.ssh/config` alias, as given to `ssh`.
    pub host: String,
    /// Absolute folder on the server.
    pub path: String,
}

/// An S3 Remote: a bucket, or a prefix in one, listed over the S3 API. It holds no secret: typed
/// keys keep their secret in the OS keyring, under the Remote's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
// No `skip_serializing_if` on the Options: bincode (the saved Index) cannot read back a skipped field.
pub struct S3Remote {
    pub bucket: String,
    /// Folder-like prefix without leading or trailing `/`; empty for the whole bucket.
    #[serde(default)]
    pub prefix: String,
    /// `https://host[:port]` of a custom endpoint (R2, MinIO, B2, Wasabi); `None` is AWS.
    #[serde(default)]
    pub endpoint: Option<String>,
    pub region: String,
    /// `endpoint/bucket/key` rather than `bucket.endpoint/key`.
    #[serde(default)]
    pub path_style: bool,
    /// A named profile of `~/.aws`; `None` is the standard chain (environment, then `default`).
    #[serde(default)]
    pub profile: Option<String>,
    /// Access key ID of keys typed into the dialog; its secret is in the keyring.
    #[serde(default)]
    pub access_key: Option<String>,
}

impl S3Remote {
    /// A Remote from what the user typed. `endpoint` may omit the scheme (https is assumed).
    pub fn new(
        bucket: &str,
        prefix: &str,
        endpoint: &str,
        region: &str,
        path_style: bool,
        profile: &str,
        access_key: &str,
    ) -> Result<S3Remote, String> {
        let bucket = bucket.trim();
        if bucket.is_empty() || bucket.contains(|c: char| c == '/' || c.is_whitespace()) {
            return Err("Bucket must be a bucket name, without slashes".into());
        }
        let region = region.trim();
        if region.is_empty() || region.contains(|c: char| c == '/' || c.is_whitespace()) {
            return Err("Region is required, such as us-east-1 (auto for Cloudflare R2)".into());
        }
        let endpoint = endpoint.trim().trim_end_matches('/');
        let endpoint = match endpoint {
            "" => None,
            e if e.contains(char::is_whitespace) => return Err("Endpoint must be a URL or host name".into()),
            e if e.starts_with("http://") || e.starts_with("https://") => Some(e.to_owned()),
            e if e.contains("://") => return Err("Endpoint must be http:// or https://".into()),
            e => Some(format!("https://{e}")),
        };
        let some = |text: &str| Some(text.trim().to_owned()).filter(|t| !t.is_empty());
        Ok(S3Remote {
            bucket: bucket.into(),
            prefix: prefix.trim().trim_matches('/').into(),
            endpoint,
            region: region.into(),
            path_style,
            profile: some(profile),
            access_key: some(access_key),
        })
    }

    /// `host[:port]` of the endpoint, or the AWS host of the region.
    pub fn endpoint_host(&self) -> String {
        match &self.endpoint {
            Some(e) => e
                .split_once("://")
                .map_or(e.as_str(), |(_, rest)| rest)
                .trim_end_matches('/')
                .to_owned(),
            None => format!("s3.{}.amazonaws.com", self.region),
        }
    }

    /// Whether requests go to `endpoint/bucket/key`: when chosen, and always for an endpoint
    /// named by an IP address or `localhost`, which cannot carry the bucket as a subdomain.
    pub fn uses_path_style(&self) -> bool {
        let host = self.endpoint_host();
        let name = match host.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(v6),
            None => host
                .rsplit_once(':')
                .filter(|(_, port)| port.parse::<u16>().is_ok())
                .map_or(host.as_str(), |(name, _)| name),
        };
        self.path_style || name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok()
    }

    /// `s3:<endpoint-host>/<bucket>/<prefix>`: the same Location wherever it is listed.
    pub fn identity(&self) -> String {
        format!("s3:{}/{}/{}", self.endpoint_host(), self.bucket, self.prefix)
    }

    /// `s3://bucket/prefix`, with the host of a custom endpoint, for display.
    pub fn label(&self) -> String {
        let name = format!("s3://{}/{}", self.bucket, self.prefix);
        match &self.endpoint {
            Some(_) => format!("{name} ({})", self.endpoint_host()),
            None => name,
        }
    }
}

/// How an FTP Remote secures the connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FtpTls {
    /// Explicit FTPS (`AUTH TLS`): a server that will not secure the connection is refused.
    #[default]
    Explicit,
    /// Plain FTP, chosen on purpose: the password crosses the network in clear text.
    Plain,
}

/// An FTP Remote: a folder on an FTP server, listed in passive mode. It holds no password: that
/// is in the OS keyring, under the Remote's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FtpRemote {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Absolute folder on the server.
    pub path: String,
    #[serde(default)]
    pub tls: FtpTls,
}

impl FtpRemote {
    /// A Remote from what the user typed. An empty path is the server's root.
    pub fn new(host: &str, port: u16, user: &str, path: &str, tls: FtpTls) -> Result<FtpRemote, String> {
        let (host, user, path) = (host.trim(), user.trim(), path.trim());
        if host.is_empty() || host.contains(|c: char| c.is_whitespace() || "/@".contains(c)) {
            return Err("Host must be a host name or address, such as ftp.example.com".into());
        }
        if port == 0 {
            return Err("Port must be 1 to 65535 (FTP is usually 21)".into());
        }
        if user.is_empty() || user.contains(|c: char| c.is_whitespace() || "/@:".contains(c)) {
            return Err("User is required (anonymous for a public server)".into());
        }
        if !path.is_empty() && !path.starts_with('/') {
            return Err("Path must be an absolute folder, such as /pub".into());
        }
        let path = path.trim_end_matches('/');
        Ok(FtpRemote {
            host: host.into(),
            port,
            user: user.into(),
            path: if path.is_empty() { "/".into() } else { path.into() },
            tls,
        })
    }

    /// A public server: it needs no password in the keyring.
    pub fn is_anonymous(&self) -> bool {
        matches!(self.user.as_str(), "anonymous" | "ftp")
    }

    /// `ftp://user@host:port/path`: the same Location wherever it is listed.
    pub fn identity(&self) -> String {
        format!("ftp://{}@{}:{}{}", self.user, self.host, self.port, self.path)
    }
}

/// A WebDAV Remote: a folder on a WebDAV server, walked one folder at a time. It holds no
/// password: that is in the OS keyring, under the Remote's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebdavRemote {
    /// `https://host[:port]/path` of the folder, without a trailing `/`.
    pub url: String,
    /// Basic-auth user; empty for a server that needs none.
    #[serde(default)]
    pub user: String,
}

impl WebdavRemote {
    /// A Remote from what the user typed. The user is typed apart from the URL, never inside it.
    pub fn new(url: &str, user: &str) -> Result<WebdavRemote, String> {
        let (url, user) = (url.trim().trim_end_matches('/'), user.trim());
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
        let host = rest.map_or("", |r| r.split('/').next().unwrap_or(""));
        if host.is_empty() || url.contains(|c: char| c.is_whitespace() || "?#".contains(c)) {
            return Err("URL must be a folder such as https://host/dav (no query)".into());
        }
        if host.contains('@') {
            return Err("Type the user in User, not in the URL".into());
        }
        if user.contains(|c: char| c.is_whitespace() || c == ':') {
            return Err("User must not contain spaces or a colon".into());
        }
        Ok(WebdavRemote {
            url: url.into(),
            user: user.into(),
        })
    }

    /// `webdav:user@https://host/path`: the same Location wherever it is listed.
    pub fn identity(&self) -> String {
        match self.user.is_empty() {
            true => format!("webdav:{}", self.url),
            false => format!("webdav:{}@{}", self.user, self.url),
        }
    }
}

/// An Azure Blob Remote: a container, or a prefix in one, listed flat like S3. It holds no
/// secret: the account key or SAS token is in the OS keyring, under the Remote's identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AzureRemote {
    pub account: String,
    pub container: String,
    /// Folder-like prefix without leading or trailing `/`; empty for the whole container.
    #[serde(default)]
    pub prefix: String,
    /// `https://host[:port][/account]` of a custom endpoint (Azurite, sovereign clouds); `None` is
    /// `https://<account>.blob.core.windows.net`.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The secret is a SAS token rather than an account key.
    #[serde(default)]
    pub sas: bool,
}

impl AzureRemote {
    /// A Remote from what the user typed. `endpoint` may omit the scheme (https is assumed).
    pub fn new(account: &str, container: &str, prefix: &str, endpoint: &str, sas: bool) -> Result<AzureRemote, String> {
        let (account, container) = (account.trim(), container.trim());
        if account.is_empty() || account.contains(|c: char| !c.is_ascii_alphanumeric()) {
            return Err("Account must be the storage account name, letters and digits only".into());
        }
        if container.is_empty() || container.contains(|c: char| c == '/' || c.is_whitespace()) {
            return Err("Container must be a container name, without slashes".into());
        }
        let endpoint = endpoint.trim().trim_end_matches('/');
        let endpoint = match endpoint {
            "" => None,
            e if e.contains(|c: char| c.is_whitespace() || "?#".contains(c)) => return Err("Endpoint must be a URL or host name".into()),
            e if e.starts_with("http://") || e.starts_with("https://") => Some(e.to_owned()),
            e if e.contains("://") => return Err("Endpoint must be http:// or https://".into()),
            e => Some(format!("https://{e}")),
        };
        Ok(AzureRemote {
            account: account.into(),
            container: container.into(),
            prefix: prefix.trim().trim_matches('/').into(),
            endpoint,
            sas,
        })
    }

    /// The service URL: the custom endpoint, or the account's Blob endpoint.
    pub fn base_url(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{}.blob.core.windows.net", self.account))
    }

    /// `azure:<service host and path>/<container>/<prefix>`: the same Location wherever it is listed.
    pub fn identity(&self) -> String {
        let base = self.base_url();
        let base = base.split_once("://").map_or(base.as_str(), |(_, rest)| rest);
        format!("azure:{base}/{}/{}", self.container, self.prefix)
    }

    /// `azure://account/container/prefix`, with the host of a custom endpoint, for display.
    pub fn label(&self) -> String {
        let name = format!("azure://{}/{}/{}", self.account, self.container, self.prefix);
        match &self.endpoint {
            Some(e) => format!("{name} ({})", e.split_once("://").map_or(e.as_str(), |(_, rest)| rest)),
            None => name,
        }
    }
}

/// A Remote Location: storage reached over a network protocol, read-only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Remote {
    // First: an SSH Remote's `host` and `path` alone would also match this one's fields.
    Ftp(FtpRemote),
    Ssh(SshRemote),
    S3(S3Remote),
    // Last: their fields (`url`, `account`) are their own, so no other kind matches them.
    Webdav(WebdavRemote),
    Azure(AzureRemote),
}

impl Remote {
    /// An SSH Remote from what the user typed. The host must be safe to hand to `ssh` as an
    /// argument and the path absolute (the server's shell would not expand `~` in quotes).
    pub fn new(host: &str, path: &str) -> Result<Remote, String> {
        let (host, path) = (host.trim(), path.trim());
        if host.is_empty() || host.starts_with('-') || host.contains(char::is_whitespace) {
            return Err("Host must be user@host or a name from ~/.ssh/config".into());
        }
        if !path.starts_with('/') {
            return Err("Path must be an absolute folder, such as /srv/backup".into());
        }
        let path = if path.len() > 1 { path.trim_end_matches('/') } else { path };
        Ok(Remote::Ssh(SshRemote {
            host: host.into(),
            path: if path.is_empty() { "/".into() } else { path.into() },
        }))
    }

    /// The kind's own behaviour: the one `match` over kinds in ve-core.
    fn kind_impl(&self) -> &dyn RemoteKind {
        match self {
            Remote::Ssh(r) => r,
            Remote::S3(r) => r,
            Remote::Ftp(r) => r,
            Remote::Webdav(r) => r,
            Remote::Azure(r) => r,
        }
    }

    /// What makes this Remote the same Location wherever it is listed: `ssh:user@host:/path`,
    /// `s3:host/bucket/prefix` or `ftp://user@host:port/path`.
    pub fn identity(&self) -> String {
        self.kind_impl().identity()
    }

    /// `user@host:/path` or `s3://bucket/prefix`, for display.
    pub fn label(&self) -> String {
        self.kind_impl().label()
    }

    /// What to show for this Remote: the user's `name` when it has one (trimmed, not empty), else
    /// its [`label`](Remote::label). The name is kept in the settings, not in the Remote, so it
    /// never changes the Remote's identity or the saved Index.
    pub fn display_name(&self, name: Option<&str>) -> String {
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => name.to_owned(),
            None => self.label(),
        }
    }

    /// The protocol, for the Remote · tag.
    pub fn kind(&self) -> &'static str {
        self.kind_impl().kind()
    }

    /// Whether it may keep a secret in the OS keyring, under its identity.
    pub fn has_secret(&self) -> bool {
        self.kind_impl().has_secret()
    }

    /// Whether it cannot connect without a secret.
    pub fn needs_secret(&self) -> bool {
        self.kind_impl().needs_secret()
    }

    /// The Volume's filesystem type and SyncState for this Remote. The SyncState's variant order
    /// is the saved Index's wire format: a new kind goes last.
    fn sync_state(&self) -> (&'static str, SyncState) {
        match self {
            Remote::Ssh(r) => ("ssh", SyncState::Ssh(r.clone())),
            Remote::S3(r) => ("s3", SyncState::S3(r.clone())),
            Remote::Ftp(r) => ("ftp", SyncState::Ftp(r.clone())),
            Remote::Webdav(r) => ("webdav", SyncState::Webdav(r.clone())),
            Remote::Azure(r) => ("azure", SyncState::Azure(r.clone())),
        }
    }
}

/// What a Remote kind defines in ve-core; sync and check live in ve-scan's module for the kind.
trait RemoteKind {
    fn identity(&self) -> String;
    fn label(&self) -> String;
    fn kind(&self) -> &'static str;
    fn has_secret(&self) -> bool {
        false
    }
    fn needs_secret(&self) -> bool {
        false
    }
}

impl RemoteKind for SshRemote {
    fn identity(&self) -> String {
        format!("ssh:{}:{}", self.host, self.path)
    }
    fn label(&self) -> String {
        format!("{}:{}", self.host, self.path)
    }
    fn kind(&self) -> &'static str {
        "SSH"
    }
}

impl RemoteKind for S3Remote {
    fn identity(&self) -> String {
        S3Remote::identity(self)
    }
    fn label(&self) -> String {
        S3Remote::label(self)
    }
    fn kind(&self) -> &'static str {
        "S3"
    }
    fn has_secret(&self) -> bool {
        self.access_key.is_some()
    }
    fn needs_secret(&self) -> bool {
        self.access_key.is_some()
    }
}

impl RemoteKind for FtpRemote {
    fn identity(&self) -> String {
        FtpRemote::identity(self)
    }
    fn label(&self) -> String {
        FtpRemote::identity(self)
    }
    fn kind(&self) -> &'static str {
        "FTP"
    }
    fn has_secret(&self) -> bool {
        true
    }
    fn needs_secret(&self) -> bool {
        !self.is_anonymous()
    }
}

impl RemoteKind for WebdavRemote {
    fn identity(&self) -> String {
        WebdavRemote::identity(self)
    }
    fn label(&self) -> String {
        self.url.clone()
    }
    fn kind(&self) -> &'static str {
        "WebDAV"
    }
    fn has_secret(&self) -> bool {
        !self.user.is_empty()
    }
    fn needs_secret(&self) -> bool {
        !self.user.is_empty()
    }
}

impl RemoteKind for AzureRemote {
    fn identity(&self) -> String {
        AzureRemote::identity(self)
    }
    fn label(&self) -> String {
        AzureRemote::label(self)
    }
    fn kind(&self) -> &'static str {
        "Azure Blob"
    }
    fn has_secret(&self) -> bool {
        true
    }
    fn needs_secret(&self) -> bool {
        true
    }
}

/// Refresh interval meaning Manual: too long to ever come due.
pub const MANUAL_REFRESH: Duration = Duration::from_secs(u32::MAX as u64);

/// How a Volume's Index is kept current.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncState {
    /// Re-walk the folder tree and apply differences.
    Walk,
    /// A Remote: re-run `find` over ssh and apply differences.
    Ssh(SshRemote),
    /// Read the NTFS change journal from `next_usn`.
    Ntfs { journal_id: u64, next_usn: i64 },
    /// A Remote: list the bucket again and apply differences. Last, to keep older Indexes readable.
    S3(S3Remote),
    /// A Remote: list the FTP server's folders again and apply differences. Last, as above.
    Ftp(FtpRemote),
    /// A Remote: walk the WebDAV folders again and apply differences. Last, as above.
    Webdav(WebdavRemote),
    /// A Remote: list the container again and apply differences. Last, as above.
    Azure(AzureRemote),
}

impl SyncState {
    /// The Remote this is the state of, if it is one.
    pub fn remote(&self) -> Option<Remote> {
        match self {
            SyncState::Ssh(r) => Some(Remote::Ssh(r.clone())),
            SyncState::S3(r) => Some(Remote::S3(r.clone())),
            SyncState::Ftp(r) => Some(Remote::Ftp(r.clone())),
            SyncState::Webdav(r) => Some(Remote::Webdav(r.clone())),
            SyncState::Azure(r) => Some(Remote::Azure(r.clone())),
            SyncState::Walk | SyncState::Ntfs { .. } => None,
        }
    }
}

/// A Volume's total and free bytes, as measured at a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    pub total: u64,
    pub free: u64,
}

/// An Indexed Volume.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Volume {
    /// Where the filesystem is mounted now, or was when last seen if it is offline.
    pub mount: PathBuf,
    /// What makes this Volume the same Volume at any mount path: the filesystem's UUID or
    /// serial, or device and mount path where the platform has none.
    pub identity: String,
    /// Mounted right now. Not saved: a loaded Volume is offline until the launch finds it.
    #[serde(skip)]
    pub online: bool,
    pub fs_type: String,
    pub root: EntryId,
    pub sync: SyncState,
    /// Last Synced: wall-clock seconds since the Unix epoch, `None` until the first sync completes.
    pub last_synced: Option<i64>,
    /// Capacity measured at that sync.
    pub capacity: Option<Capacity>,
    /// The name the user gave this Remote. Not saved (the settings keep it, the Engine sets it).
    #[serde(skip)]
    pub name: Option<String>,
}

impl Volume {
    /// The Remote this Location is, if it is one.
    pub fn remote(&self) -> Option<Remote> {
        self.sync.remote()
    }

    /// What to show for this Location when it is a Remote: its name, else its address.
    pub fn remote_display_name(&self) -> Option<String> {
        self.remote().map(|r| r.display_name(self.name.as_deref()))
    }

    /// A read-only Remote rather than a mounted Volume.
    pub fn is_remote(&self) -> bool {
        !matches!(self.sync, SyncState::Walk | SyncState::Ntfs { .. })
    }

    /// Whether Catch-up should run at `now` (seconds since the Unix epoch): the Last Synced is
    /// older than `interval`, missing, or in the future (the clock moved back). NTFS Volumes
    /// always catch up because reading the change journal is cheap (ADR-0003). A Remote with a
    /// Manual interval never does.
    pub fn needs_catch_up(&self, now: i64, interval: Duration) -> bool {
        if matches!(self.sync, SyncState::Ntfs { .. }) {
            return true;
        }
        if self.is_remote() && interval >= MANUAL_REFRESH {
            return false;
        }
        match self.last_synced {
            Some(at) => at > now || now - at >= i64::try_from(interval.as_secs()).unwrap_or(i64::MAX),
            None => true,
        }
    }
}

const DIR: u8 = 1;
const HARDLINK: u8 = 2;
const REMOVED: u8 = 4;
const NO_PARENT: u32 = u32::MAX;
const NO_SLOT: u32 = u32::MAX;
/// Allocation unit assumed for On-disk Size: a file's Logical Size rounded up to this is the
/// predicted value, and only files that differ are stored.
const BLOCK_SIZE: u64 = 4096;

const MAGIC: &[u8; 8] = b"VEINDEX\0";
/// v2 adds Last Synced and capacity per Volume; v3 adds the Volume's identity; v4 adds Remotes; v5 adds creation time;
/// v6 stores sizes and creation time (in seconds) in 4 bytes per Entry; v7 interns names, keeps
/// modification time in seconds, stores children as id ranges and only the unpredictable
/// On-disk Sizes and creation times; v8 adds Owners (ADR-0014).
const FORMAT_VERSION: u32 = 8;

/// A time as stored: whole seconds since the Unix epoch, as the bits of an `i64`.
fn secs(nanos: i64) -> u64 {
    nanos.div_euclid(1_000_000_000) as u64
}

/// A stored time back in nanoseconds.
fn nanos(secs: u64) -> i64 {
    (secs as i64).saturating_mul(1_000_000_000)
}

/// What On-disk Size is expected to be, so only the exceptions are stored.
fn predicted_on_disk(is_dir: bool, logical: u64) -> u64 {
    if is_dir {
        0
    } else {
        logical.div_ceil(BLOCK_SIZE).saturating_mul(BLOCK_SIZE)
    }
}

/// Every distinct Entry name once; an Entry refers to its name by number.
#[derive(Serialize, Deserialize)]
struct Names {
    /// Where each name starts in `bytes`; one more entry than names.
    starts: Vec<u32>,
    bytes: String,
    /// Name number by name. Only inserting needs it: built by the first [`Names::intern`]
    /// after a load, dropped by [`Index::idle`].
    #[serde(skip)]
    lookup: Option<(RandomState, HashTable<u32>)>,
}

impl Default for Names {
    fn default() -> Self {
        Names {
            starts: vec![0],
            bytes: String::new(),
            lookup: None,
        }
    }
}

impl Names {
    #[inline]
    fn get(&self, n: u32) -> &str {
        &self.bytes[self.starts[n as usize] as usize..self.starts[n as usize + 1] as usize]
    }

    fn len(&self) -> usize {
        self.starts.len() - 1
    }

    /// The number of `name`, adding it if new.
    fn intern(&mut self, name: &str) -> u32 {
        let Names { starts, bytes, lookup } = self;
        let get = |n: u32| &bytes[starts[n as usize] as usize..starts[n as usize + 1] as usize];
        let (state, table) = lookup.get_or_insert_with(|| {
            let state = RandomState::new();
            let mut table = HashTable::with_capacity(starts.len() - 1);
            for n in 0..starts.len() as u32 - 1 {
                table.insert_unique(state.hash_one(get(n)), n, |&m| state.hash_one(get(m)));
            }
            (state, table)
        });
        let hash = state.hash_one(name);
        match table.entry(hash, |&n| get(n) == name, |&m| state.hash_one(get(m))) {
            hashbrown::hash_table::Entry::Occupied(e) => *e.get(),
            hashbrown::hash_table::Entry::Vacant(e) => {
                let n = u32::try_from(starts.len() - 1).expect("more than 4 billion distinct names");
                bytes.push_str(name);
                starts.push(u32::try_from(bytes.len()).expect("Index name storage exceeds 4 GiB"));
                e.insert(n);
                n
            }
        }
    }
}

/// The persistent record of every Entry on every Indexed Volume.
///
/// Stored column-wise to keep per-Entry overhead small. Removed Entries leave tombstones that
/// are dropped when the Index is saved. Entries are numbered depth-first when saved, so after a
/// load a folder's children are one run of consecutive ids; children added since then are
/// kept aside until the next save.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    parent: Vec<u32>,
    /// Name number per Entry, into `names`.
    name: Vec<u32>,
    flags: Vec<u8>,
    logical: U64Column,
    /// On-disk Size where it is not [`predicted_on_disk`].
    on_disk: SparseColumn,
    /// See [`secs`].
    modified: U64Column,
    /// Creation time (see [`secs`]) where it differs from `modified`.
    created: SparseColumn,
    key: U64Column,
    /// Which Entries are folders; a folder's slot is its rank here.
    dirs: Rank,
    /// First id and length of each folder's run of children, by folder slot.
    child_first: Vec<u32>,
    child_count: Vec<u32>,
    /// Children outside their folder's run, by folder slot; only live ones.
    #[serde(skip)]
    extra_children: HashMap<u32, Vec<EntryId>>,
    names: Names,
    volumes: Vec<Option<Volume>>,
    live: usize,
    /// Owner number on Owner roots only, into `owners`.
    owner: SparseColumn,
    /// Every Owner referred to by `owner`, plus ones no longer referred to until the next save.
    owners: Vec<Owner>,
    #[serde(skip)]
    generation: u64,
    /// Loaded from a format without Owners: every Location needs detection.
    #[serde(skip)]
    owners_stale: bool,
}

/// The format 7 layout: format 8 without Owners. Its columns move over as they are.
#[derive(Serialize, Deserialize)]
struct IndexV7 {
    parent: Vec<u32>,
    name: Vec<u32>,
    flags: Vec<u8>,
    logical: U64Column,
    on_disk: SparseColumn,
    modified: U64Column,
    created: SparseColumn,
    key: U64Column,
    dirs: Rank,
    child_first: Vec<u32>,
    child_count: Vec<u32>,
    names: Names,
    volumes: Vec<Option<Volume>>,
    live: usize,
}

impl From<IndexV7> for Index {
    fn from(old: IndexV7) -> Index {
        let mut owner = SparseColumn::default();
        (0..old.parent.len()).for_each(|_| owner.push(None));
        Index {
            parent: old.parent,
            name: old.name,
            flags: old.flags,
            logical: old.logical,
            on_disk: old.on_disk,
            modified: old.modified,
            created: old.created,
            key: old.key,
            dirs: old.dirs,
            child_first: old.child_first,
            child_count: old.child_count,
            extra_children: HashMap::new(),
            names: old.names,
            volumes: old.volumes,
            live: old.live,
            owner,
            owners: Vec::new(),
            generation: 0,
            owners_stale: true,
        }
    }
}

/// The format 5 layout, read once and converted.
#[derive(Deserialize)]
struct IndexV5 {
    parent: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u8>,
    logical: Vec<u64>,
    on_disk: Vec<u64>,
    modified: Vec<i64>,
    created: Vec<i64>,
    key: Vec<u64>,
    slot: Vec<u32>,
    children: Vec<Vec<EntryId>>,
    names: String,
    volumes: Vec<Option<Volume>>,
    live: usize,
}

/// The format 6 layout, read once and converted.
#[derive(Deserialize)]
struct IndexV6 {
    parent: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u8>,
    logical: U64Column,
    on_disk: U64Column,
    modified: Vec<i64>,
    created: U64Column,
    key: Vec<u64>,
    slot: Vec<u32>,
    children: Vec<Vec<EntryId>>,
    names: String,
    volumes: Vec<Option<Volume>>,
    live: usize,
}

impl From<IndexV5> for IndexV6 {
    fn from(old: IndexV5) -> IndexV6 {
        let column = |values: Vec<u64>| {
            let mut c = U64Column::default();
            values.into_iter().for_each(|v| c.push(v));
            c
        };
        IndexV6 {
            logical: column(old.logical),
            on_disk: column(old.on_disk),
            created: column(old.created.into_iter().map(secs).collect()),
            parent: old.parent,
            name_start: old.name_start,
            name_len: old.name_len,
            flags: old.flags,
            modified: old.modified,
            key: old.key,
            slot: old.slot,
            children: old.children,
            names: old.names,
            volumes: old.volumes,
            live: old.live,
        }
    }
}

impl From<IndexV6> for Index {
    /// Rebuilds row by row. A saved Index has no tombstones and numbers Entries depth-first,
    /// so every folder's children form one run; a run that is not consecutive goes aside.
    fn from(old: IndexV6) -> Index {
        let n = old.parent.len();
        let mut index = Index::new();
        for i in 0..n {
            let start = old.name_start[i] as usize;
            let name = &old.names[start..start + old.name_len[i] as usize];
            // Pushed, not inserted: the run of children is set from the saved lists below.
            index.push_raw(
                old.parent[i],
                name,
                old.flags[i] & (DIR | HARDLINK),
                old.logical.get(i),
                old.on_disk.get(i),
                secs(old.modified[i]),
                old.created.get(i),
                old.key[i],
            );
        }
        for (i, &slot) in old.slot.iter().enumerate() {
            if slot == NO_SLOT {
                continue;
            }
            debug_assert_eq!(index.dirs.rank(i), slot as usize);
            let kids = &old.children[slot as usize];
            let consecutive = kids.windows(2).all(|w| w[1].0 == w[0].0 + 1);
            if let (true, Some(first)) = (consecutive, kids.first()) {
                index.child_first[slot as usize] = first.0;
                index.child_count[slot as usize] = kids.len() as u32;
            } else if !kids.is_empty() {
                index.extra_children.insert(slot, kids.clone());
            }
        }
        index.volumes = old.volumes;
        debug_assert_eq!(index.live, old.live, "a saved Index has no tombstones");
        index
    }
}

impl Index {
    pub fn new() -> Self {
        let mut index = Self::default();
        index.touch();
        index
    }

    /// Changes on every change, and differs between Index instances (including one loaded from
    /// disk), so observers can tell that derived data is stale.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn touch(&mut self) {
        self.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of Entry ids ever allocated, including removed ones. Valid ids are below this.
    pub fn capacity(&self) -> usize {
        self.parent.len()
    }

    /// Number of Entries currently in the Index.
    pub fn live_count(&self) -> usize {
        self.live
    }

    pub fn add_volume(&mut self, mount: &Path, fs_type: &str, identity: &str, meta: EntryMeta, sync: SyncState) -> VolumeId {
        debug_assert!(meta.is_dir, "a Volume root is a folder");
        let root = self.push(NO_PARENT, &mount.to_string_lossy(), meta);
        let volume = Volume {
            mount: mount.to_path_buf(),
            identity: identity.to_owned(),
            online: true,
            fs_type: fs_type.to_owned(),
            root,
            sync,
            last_synced: None,
            capacity: None,
            name: None,
        };
        let id = VolumeId(self.volumes.len() as u32);
        self.volumes.push(Some(volume));
        id
    }

    /// Sets (or with `None` clears) the name shown for the Remote Location `id`.
    pub fn set_remote_name(&mut self, id: VolumeId, name: Option<&str>) {
        let name = name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned);
        let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) else {
            return;
        };
        if volume.name != name {
            volume.name = name;
            self.touch();
        }
    }

    /// Adds a Remote as a Location with only its root, ready for a sync. It is always online;
    /// its root Entry is named by its identity, so paths read `ssh:user@host:/path/...`,
    /// `s3:host/bucket/prefix/...` or `ftp://user@host:port/path/...`.
    pub fn add_remote(&mut self, remote: &Remote) -> VolumeId {
        let identity = remote.identity();
        let meta = EntryMeta {
            is_dir: true,
            ..EntryMeta::default()
        };
        let (fs_type, sync) = remote.sync_state();
        self.add_volume(Path::new(&identity), fs_type, &identity, meta, sync)
    }

    pub fn remove_volume(&mut self, id: VolumeId) {
        if let Some(volume) = self.volumes.get_mut(id.0 as usize).and_then(Option::take) {
            self.remove(volume.root);
        }
    }

    pub fn volumes(&self) -> impl Iterator<Item = (VolumeId, &Volume)> {
        self.volumes
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.as_ref().map(|v| (VolumeId(i as u32), v)))
    }

    pub fn volume(&self, id: VolumeId) -> Option<&Volume> {
        self.volumes.get(id.0 as usize)?.as_ref()
    }

    /// The Volume with filesystem identity `identity`.
    pub fn volume_with_identity(&self, identity: &str) -> Option<VolumeId> {
        self.volumes().find(|(_, v)| v.identity == identity).map(|(id, _)| id)
    }

    /// Records that `id` is mounted at `mount` (a remount keeps every Entry), or not mounted.
    pub fn set_mount(&mut self, id: VolumeId, mount: Option<&Path>) {
        let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) else {
            return;
        };
        volume.online = mount.is_some();
        let Some(mount) = mount.filter(|m| *m != volume.mount) else {
            return;
        };
        volume.mount = mount.to_path_buf();
        let root = volume.root;
        self.name[root.index()] = self.names.intern(&mount.to_string_lossy());
        self.touch();
    }

    pub fn set_sync_state(&mut self, id: VolumeId, sync: SyncState) {
        if let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) {
            volume.sync = sync;
            self.touch();
        }
    }

    /// Records that `id` was brought fully up to date at `at` (seconds since the Unix epoch).
    /// Does not change [`Index::generation`]: no Entry moved, so derived data stays valid.
    pub fn mark_synced(&mut self, id: VolumeId, at: i64, capacity: Option<Capacity>) {
        if let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) {
            volume.last_synced = Some(at);
            volume.capacity = capacity;
        }
    }

    /// Replaces the capacity recorded for `id`. Like [`Index::mark_synced`], not an Entry change.
    pub fn set_capacity(&mut self, id: VolumeId, capacity: Option<Capacity>) {
        if let Some(Some(volume)) = self.volumes.get_mut(id.0 as usize) {
            volume.capacity = capacity;
        }
    }

    /// The Last Synced of the least recently synced Volume. The outer `None` means there are no
    /// Volumes; the inner `None` means some Volume has never finished a sync.
    pub fn oldest_synced(&self) -> Option<Option<i64>> {
        self.volumes().map(|(_, v)| v.last_synced).min()
    }

    /// The Volume whose root is `root`.
    pub fn volume_with_root(&self, root: EntryId) -> Option<VolumeId> {
        self.volumes().find(|(_, v)| v.root == root).map(|(id, _)| id)
    }

    /// The Volume containing `id`.
    pub fn volume_of(&self, id: EntryId) -> Option<VolumeId> {
        self.volume_with_root(self.root_of(id))
    }

    pub fn root_of(&self, mut id: EntryId) -> EntryId {
        while let Some(parent) = self.parent(id) {
            id = parent;
        }
        id
    }

    /// Adds a new Entry under the folder `parent`.
    pub fn insert(&mut self, parent: EntryId, name: &str, meta: EntryMeta) -> EntryId {
        debug_assert!(self.is_dir(parent) && self.is_live(parent));
        let id = self.push(parent.0, name, meta);
        self.attach(parent, id);
        id
    }

    fn push(&mut self, parent: u32, name: &str, meta: EntryMeta) -> EntryId {
        self.push_raw(
            parent,
            name,
            flags_for(&meta),
            meta.logical,
            meta.on_disk,
            secs(meta.modified),
            secs(meta.created),
            meta.key,
        )
    }

    /// Appends a row with times already in stored form, attached to no folder's children.
    #[allow(clippy::too_many_arguments)]
    fn push_raw(
        &mut self,
        parent: u32,
        name: &str,
        flags: u8,
        logical: u64,
        on_disk: u64,
        modified: u64,
        created: u64,
        key: u64,
    ) -> EntryId {
        let id = EntryId(u32::try_from(self.parent.len()).expect("more than 4 billion Entries"));
        let is_dir = flags & DIR != 0;
        self.parent.push(parent);
        let name = self.names.intern(name);
        self.name.push(name);
        self.flags.push(flags);
        self.logical.push(logical);
        self.on_disk
            .push((on_disk != predicted_on_disk(is_dir, logical)).then_some(on_disk));
        self.modified.push(modified);
        self.created.push((created != modified).then_some(created));
        self.key.push(key);
        self.owner.push(None);
        self.dirs.push(is_dir);
        if is_dir {
            self.child_first.push(0);
            self.child_count.push(0);
        }
        self.live += 1;
        self.touch();
        id
    }

    /// Makes `id` (whose `parent` is already `folder`) one of `folder`'s children: the next id
    /// in the folder's run extends it, an id already inside the run is covered, any other id
    /// goes aside.
    fn attach(&mut self, folder: EntryId, id: EntryId) {
        debug_assert_eq!(self.parent[id.index()], folder.0);
        let slot = self.dirs.rank(folder.index());
        let (first, count) = (self.child_first[slot], self.child_count[slot]);
        if count == 0 {
            self.child_first[slot] = id.0;
            self.child_count[slot] = 1;
        } else if id.0 == first + count {
            self.child_count[slot] = count + 1;
        } else if !(first..first + count).contains(&id.0) {
            self.extra_children.entry(slot as u32).or_default().push(id);
        }
    }

    /// Replaces an Entry's sizes, time and identity. Whether it is a folder cannot change;
    /// remove and re-insert instead.
    pub fn update(&mut self, id: EntryId, meta: EntryMeta) {
        let i = id.index();
        debug_assert_eq!(self.flags[i] & DIR != 0, meta.is_dir);
        let flags = (self.flags[i] & !HARDLINK) | if meta.hardlinked { HARDLINK } else { 0 };
        let (modified, created) = (secs(meta.modified), secs(meta.created));
        if self.flags[i] == flags
            && self.logical.get(i) == meta.logical
            && self.on_disk(id) == meta.on_disk
            && self.modified.get(i) == modified
            && self.created.get(i).unwrap_or(modified) == created
            && self.key.get(i) == meta.key
        {
            return;
        }
        self.flags[i] = flags;
        self.logical.set(i, meta.logical);
        self.on_disk.set(
            i,
            (meta.on_disk != predicted_on_disk(meta.is_dir, meta.logical)).then_some(meta.on_disk),
        );
        self.modified.set(i, modified);
        self.created.set(i, (created != modified).then_some(created));
        self.key.set(i, meta.key);
        self.touch();
    }

    /// Moves an Entry (and everything beneath it) to a new parent folder and name.
    pub fn rename(&mut self, id: EntryId, new_parent: EntryId, new_name: &str) {
        debug_assert!(self.is_dir(new_parent) && self.is_live(new_parent));
        self.detach(id);
        self.parent[id.index()] = new_parent.0;
        self.attach(new_parent, id);
        if self.name(id) != new_name {
            self.name[id.index()] = self.names.intern(new_name);
        }
        self.touch();
    }

    /// Removes an Entry and everything beneath it.
    pub fn remove(&mut self, id: EntryId) {
        if !self.is_live(id) {
            return;
        }
        self.detach(id);
        self.tombstone(id);
        self.touch();
    }

    /// Removes several children of `dir` and everything beneath them, detaching them in one
    /// pass rather than one search per child.
    pub fn remove_children(&mut self, dir: EntryId, ids: &[EntryId]) {
        if ids.is_empty() {
            return;
        }
        let gone: std::collections::HashSet<EntryId> = ids.iter().copied().collect();
        let slot = self.dirs.rank(dir.index()) as u32;
        if let Some(extra) = self.extra_children.get_mut(&slot) {
            extra.retain(|c| !gone.contains(c));
            if extra.is_empty() {
                self.extra_children.remove(&slot);
            }
        }
        for &id in ids {
            if self.is_live(id) {
                self.tombstone(id);
            }
        }
        self.touch();
    }

    /// Marks `id` and everything beneath it removed. The removed Entries stay in their
    /// folders' runs (skipped as removed) and leave the side lists.
    fn tombstone(&mut self, id: EntryId) {
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            let i = current.index();
            self.flags[i] |= REMOVED;
            self.live -= 1;
            if self.dirs.contains(i) {
                stack.extend(self.children(current));
                self.extra_children.remove(&(self.dirs.rank(i) as u32));
            }
        }
    }

    /// Takes `id` out of its folder's side list; an id in the folder's run is skipped once its
    /// parent or removed flag changes.
    fn detach(&mut self, id: EntryId) {
        let parent = self.parent[id.index()];
        if parent == NO_PARENT {
            return;
        }
        let slot = self.dirs.rank(parent as usize);
        let (first, count) = (self.child_first[slot], self.child_count[slot]);
        if (first..first + count).contains(&id.0) {
            return;
        }
        if let Some(extra) = self.extra_children.get_mut(&(slot as u32)) {
            if let Some(pos) = extra.iter().position(|&c| c == id) {
                extra.swap_remove(pos);
            }
            if extra.is_empty() {
                self.extra_children.remove(&(slot as u32));
            }
        }
    }

    /// Frees what only inserting needs, for when no sync is running: the name lookup table
    /// and the spare capacity the columns grew by. Reallocating 200 MB of columns holds the
    /// write lock for a frame or two, so it is done only after real growth (a first scan),
    /// not after a Catch-up that added a few Entries to a loaded Index.
    pub fn idle(&mut self) {
        self.names.lookup = None;
        let grown = |len: usize, capacity: usize| capacity > len + len / 4;
        if grown(self.parent.len(), self.parent.capacity()) {
            self.parent.shrink_to_fit();
            self.name.shrink_to_fit();
            self.flags.shrink_to_fit();
            self.logical.shrink_to_fit();
            self.on_disk.shrink_to_fit();
            self.modified.shrink_to_fit();
            self.created.shrink_to_fit();
            self.key.shrink_to_fit();
            self.owner.shrink_to_fit();
            self.dirs.shrink_to_fit();
            self.child_first.shrink_to_fit();
            self.child_count.shrink_to_fit();
        }
        if grown(self.names.bytes.len(), self.names.bytes.capacity()) {
            self.names.starts.shrink_to_fit();
            self.names.bytes.shrink_to_fit();
        }
        self.extra_children.shrink_to_fit();
    }

    #[inline]
    pub fn is_live(&self, id: EntryId) -> bool {
        self.flags.get(id.index()).is_some_and(|f| f & REMOVED == 0)
    }

    #[inline]
    pub fn is_dir(&self, id: EntryId) -> bool {
        self.flags[id.index()] & DIR != 0
    }

    #[inline]
    pub fn is_hardlinked(&self, id: EntryId) -> bool {
        self.flags[id.index()] & HARDLINK != 0
    }

    #[inline]
    pub fn name(&self, id: EntryId) -> &str {
        self.names.get(self.name[id.index()])
    }

    /// What to show for `id`: its name, except that the root of a Remote Location the user named
    /// shows that name in place of its address. The path of the Entry is unchanged.
    pub fn display_name(&self, id: EntryId) -> &str {
        if self.parent(id).is_none()
            && let Some(name) = self
                .volume_with_root(id)
                .and_then(|v| self.volume(v))
                .and_then(|v| v.name.as_deref())
        {
            return name;
        }
        self.name(id)
    }

    #[inline]
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        let p = self.parent[id.index()];
        (p != NO_PARENT).then_some(EntryId(p))
    }

    /// Levels below the Location root; the root is 0.
    pub fn depth(&self, id: EntryId) -> u32 {
        std::iter::successors(self.parent(id), |&p| self.parent(p)).count() as u32
    }

    #[inline]
    pub fn logical(&self, id: EntryId) -> u64 {
        self.logical.get(id.index())
    }

    #[inline]
    pub fn on_disk(&self, id: EntryId) -> u64 {
        let i = id.index();
        self.on_disk
            .get(i)
            .unwrap_or_else(|| predicted_on_disk(self.is_dir(id), self.logical.get(i)))
    }

    /// Modification time in ns since the Unix epoch, to the whole second; 0 when unknown.
    #[inline]
    pub fn modified(&self, id: EntryId) -> i64 {
        nanos(self.modified.get(id.index()))
    }

    /// Creation time in ns since the Unix epoch, to the whole second; 0 when unknown.
    #[inline]
    pub fn created(&self, id: EntryId) -> i64 {
        let i = id.index();
        nanos(self.created.get(i).unwrap_or_else(|| self.modified.get(i)))
    }

    #[inline]
    pub fn key(&self, id: EntryId) -> u64 {
        self.key.get(id.index())
    }

    pub fn meta(&self, id: EntryId) -> EntryMeta {
        EntryMeta {
            is_dir: self.is_dir(id),
            logical: self.logical(id),
            on_disk: self.on_disk(id),
            modified: self.modified(id),
            created: self.created(id),
            key: self.key(id),
            hardlinked: self.is_hardlinked(id),
        }
    }

    /// The live children of a folder, in no particular order; none for a file.
    #[inline]
    pub fn children(&self, id: EntryId) -> Children<'_> {
        let i = id.index();
        let (run, extra) = if self.dirs.contains(i) {
            let slot = self.dirs.rank(i);
            let first = self.child_first[slot];
            let extra = self.extra_children.get(&(slot as u32)).map_or(&[][..], Vec::as_slice);
            (first..first + self.child_count[slot], extra)
        } else {
            (0..0, &[][..])
        };
        Children {
            index: self,
            parent: id.0,
            run,
            extra: extra.iter(),
        }
    }

    /// Whether a folder has any live child.
    pub fn has_children(&self, id: EntryId) -> bool {
        self.children(id).next().is_some()
    }

    /// Dense number of a folder, below [`Index::folder_slots`]; `None` for files.
    #[inline]
    pub fn folder_slot(&self, id: EntryId) -> Option<usize> {
        let i = id.index();
        self.dirs.contains(i).then(|| self.dirs.rank(i))
    }

    pub fn folder_slots(&self) -> usize {
        self.dirs.total()
    }

    pub fn child_named(&self, dir: EntryId, name: &str) -> Option<EntryId> {
        self.children(dir).find(|&c| self.name(c) == name)
    }

    /// Replaces the Owners of the Location (or folder) `root`: every Owner root beneath it is
    /// forgotten and `roots` become the Owner roots. Equal Owners share one [`OwnerId`].
    /// Owners nothing refers to any more stay in the table until the next save.
    pub fn set_owners(&mut self, root: EntryId, roots: Vec<(EntryId, Owner)>) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            self.owner.set(id.index(), None);
            if self.dirs.contains(id.index()) {
                stack.extend(self.children(id));
            }
        }
        let mut by_owner: HashMap<&Owner, u32> = self.owners.iter().enumerate().map(|(i, o)| (o, i as u32)).collect();
        let mut added = Vec::new();
        let mut marks = Vec::with_capacity(roots.len());
        for (id, owner) in &roots {
            let n = match by_owner.get(owner) {
                Some(&n) => n,
                None => {
                    let n = (self.owners.len() + added.len()) as u32;
                    added.push(owner.clone());
                    by_owner.insert(owner, n);
                    n
                }
            };
            marks.push((*id, n));
        }
        drop(by_owner);
        self.owners.extend(added);
        for (id, n) in marks {
            // A root found before a concurrent removal is simply not marked.
            if self.is_live(id) {
                self.owner.set(id.index(), Some(u64::from(n)));
            }
        }
        self.owners_stale = false;
        self.touch();
    }

    /// The Owner `id` is the Owner root of, if it is one.
    #[inline]
    pub fn owner_root_at(&self, id: EntryId) -> Option<OwnerId> {
        self.owner.get(id.index()).map(|n| OwnerId(n as u32))
    }

    /// The Owner of an Entry (CONTEXT.md): that of the nearest Owner root at or above it, with
    /// that root.
    pub fn owner_of(&self, id: EntryId) -> Option<(EntryId, OwnerId)> {
        std::iter::successors(Some(id), |&c| self.parent(c)).find_map(|c| self.owner_root_at(c).map(|o| (c, o)))
    }

    pub fn owner(&self, id: OwnerId) -> &Owner {
        &self.owners[id.0 as usize]
    }

    /// The Owner of an Entry, for showing it: [`Index::owner_of`] without the root.
    pub fn entry_owner(&self, id: EntryId) -> Option<&Owner> {
        self.owner_of(id).map(|(_, o)| self.owner(o))
    }

    /// Every live Owner root with its Owner.
    pub fn owner_roots(&self) -> impl Iterator<Item = (EntryId, OwnerId)> + '_ {
        (0..self.capacity())
            .map(|i| EntryId(i as u32))
            .filter(|&id| self.is_live(id))
            .filter_map(|id| self.owner_root_at(id).map(|o| (id, o)))
    }

    /// Whether the Index was loaded from a format without Owners, so every Location needs
    /// detection before Owners mean anything. Cleared by the first [`Index::set_owners`].
    pub fn owners_stale(&self) -> bool {
        self.owners_stale
    }

    /// Full path of an Entry. A Remote's paths are the server's, joined with `/` on every OS.
    pub fn path(&self, id: EntryId) -> PathBuf {
        let mut chain = vec![id];
        let mut current = id;
        while let Some(parent) = self.parent(current) {
            chain.push(parent);
            current = parent;
        }
        let remote = self
            .volume_with_root(current)
            .and_then(|v| self.volume(v))
            .is_some_and(Volume::is_remote);
        if remote {
            let mut path = String::new();
            self.write_match_path(id, &mut path, &mut chain);
            return PathBuf::from(path);
        }
        let mut path = PathBuf::from(self.name(current));
        for &component in chain.iter().rev().skip(1) {
            path.push(self.name(component));
        }
        path
    }

    /// Writes the full path of an Entry into `out` with `/` separators, for matching.
    /// `chain` is scratch space reused between calls.
    pub fn write_match_path(&self, id: EntryId, out: &mut String, chain: &mut Vec<EntryId>) {
        out.clear();
        chain.clear();
        let mut current = id;
        chain.push(current);
        while let Some(parent) = self.parent(current) {
            chain.push(parent);
            current = parent;
        }
        let (root, rest) = chain.split_last().expect("chain holds at least the Entry");
        out.extend(self.name(*root).chars().map(|c| if c == '\\' { '/' } else { c }));
        for &component in rest.iter().rev() {
            if !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(self.name(component));
        }
    }

    /// The Entry at `path`, if indexed.
    pub fn find_path(&self, path: &Path) -> Option<EntryId> {
        let (_, volume) = self
            .volumes()
            .filter(|(_, v)| path.starts_with(&v.mount))
            .max_by_key(|(_, v)| v.mount.as_os_str().len())?;
        let rest = path.strip_prefix(&volume.mount).ok()?;
        let mut current = volume.root;
        for component in rest.components() {
            current = self.child_named(current, component.as_os_str().to_str()?)?;
        }
        Some(current)
    }

    /// A copy containing only live Entries, with ids renumbered in depth-first order: what
    /// [`Index::save`] writes, built in full. Kept to check [`SaveView`] against.
    #[cfg(test)]
    fn compacted(&self) -> Index {
        let mut out = Index::new();
        let mut roots: Vec<(EntryId, Owner)> = Vec::new();
        let carry = |old: EntryId, new: EntryId, roots: &mut Vec<(EntryId, Owner)>| {
            if let Some(o) = self.owner_root_at(old) {
                roots.push((new, self.owner(o).clone()));
            }
        };
        for volume in self.volumes.iter() {
            let Some(volume) = volume else {
                out.volumes.push(None);
                continue;
            };
            let root = out.push(NO_PARENT, self.name(volume.root), self.meta(volume.root));
            carry(volume.root, root, &mut roots);
            out.volumes.push(Some(Volume { root, ..volume.clone() }));
            let mut stack = vec![(volume.root, root)];
            while let Some((old, new)) = stack.pop() {
                for child in self.children(old) {
                    let copy = out.insert(new, self.name(child), self.meta(child));
                    carry(child, copy, &mut roots);
                    if self.is_dir(child) {
                        stack.push((child, copy));
                    }
                }
            }
        }
        for (id, owner) in roots {
            let n = match out.owners.iter().position(|o| *o == owner) {
                Some(n) => n,
                None => {
                    out.owners.push(owner);
                    out.owners.len() - 1
                }
            };
            out.owner.set(id.index(), Some(n as u64));
        }
        // Marks went to the column's overlay; a saved column has them in place.
        let rows: Vec<u32> = (0..out.capacity() as u32).collect();
        out.owner = out.owner.reordered(&rows);
        out
    }

    /// The live Entries in the order a compacted copy would number them, and the volumes with
    /// their roots renumbered: everything [`SaveView`] needs, at 8 bytes per Entry rather than a
    /// second Index.
    fn save_view(&self) -> SaveView<'_> {
        let mut order: Vec<u32> = Vec::with_capacity(self.live);
        let mut new_id = vec![NO_PARENT; self.capacity()];
        let mut volumes = Vec::with_capacity(self.volumes.len());
        for volume in self.volumes.iter() {
            let Some(volume) = volume else {
                volumes.push(None);
                continue;
            };
            let root = EntryId(order.len() as u32);
            let mut number = |old: EntryId, order: &mut Vec<u32>| {
                new_id[old.index()] = order.len() as u32;
                order.push(old.0);
            };
            number(volume.root, &mut order);
            volumes.push(Some(Volume { root, ..volume.clone() }));
            let mut stack = vec![volume.root];
            while let Some(old) = stack.pop() {
                for child in self.children(old) {
                    number(child, &mut order);
                    if self.is_dir(child) {
                        stack.push(child);
                    }
                }
            }
        }
        SaveView {
            index: self,
            order,
            new_id,
            volumes,
        }
    }

    /// Writes the Index to `path` atomically, readable only by the current user.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let view = self.save_view();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut out = BufWriter::new(options.open(&tmp)?);
            out.write_all(MAGIC)?;
            out.write_all(&FORMAT_VERSION.to_le_bytes())?;
            bincode::serialize_into(&mut out, &view).map_err(io::Error::other)?;
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        }
        fs::rename(&tmp, path)
    }

    /// Reads an Index written by [`Index::save`].
    pub fn load(path: &Path) -> io::Result<Index> {
        let mut input = BufReader::new(fs::File::open(path)?);
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic)?;
        let mut version = [0u8; 4];
        input.read_exact(&mut version)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a saved Index"));
        }
        let version = u32::from_le_bytes(version);
        let mut index: Index = match version {
            FORMAT_VERSION => bincode::deserialize_from(input),
            7 => bincode::deserialize_from::<_, IndexV7>(input).map(Index::from),
            6 => bincode::deserialize_from::<_, IndexV6>(input).map(Index::from),
            5 => bincode::deserialize_from::<_, IndexV5>(input).map(|v5| Index::from(IndexV6::from(v5))),
            // Older layouts are not migrated: the Index is cheap to rebuild by a scan.
            v if v < FORMAT_VERSION => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("saved by an older version (format {v}); needs rebuild"),
                ));
            }
            v => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("saved by a newer version (format {v})"),
                ));
            }
        }
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        index.owners_stale = version < 8;
        index.touch();
        Ok(index)
    }
}

fn flags_for(meta: &EntryMeta) -> u8 {
    (if meta.is_dir { DIR } else { 0 }) | (if meta.hardlinked { HARDLINK } else { 0 })
}

/// The live children of a folder: see [`Index::children`].
#[derive(Clone)]
pub struct Children<'a> {
    index: &'a Index,
    parent: u32,
    /// The folder's run of ids; those still its live children are yielded.
    run: std::ops::Range<u32>,
    extra: std::slice::Iter<'a, EntryId>,
}

impl Iterator for Children<'_> {
    type Item = EntryId;

    #[inline]
    fn next(&mut self) -> Option<EntryId> {
        let ix = self.index;
        for c in self.run.by_ref() {
            let i = c as usize;
            if ix.parent[i] == self.parent && ix.flags[i] & REMOVED == 0 {
                return Some(EntryId(c));
            }
        }
        self.extra.next().copied()
    }
}

/// The Index as a compacted copy would hold it, serialised field by field from the live
/// Index without the copy (which doubled memory during every save).
struct SaveView<'a> {
    index: &'a Index,
    /// Old ids, in new id order.
    order: Vec<u32>,
    /// New id by old id; `NO_PARENT` for Entries not saved.
    new_id: Vec<u32>,
    volumes: Vec<Option<Volume>>,
}

impl Serialize for SaveView<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let (ix, order, n) = (self.index, &self.order, self.order.len());
        let rows = order.iter().map(|&o| o as usize);
        let folders = rows.clone().filter(|&o| ix.dirs.contains(o));
        let parent = rows.clone().map(|o| match ix.parent[o] {
            NO_PARENT => NO_PARENT,
            p => self.new_id[p as usize],
        });

        // Names are numbered in order of first use, as inserting them one by one would.
        let mut new_name = vec![u32::MAX; ix.names.len()];
        let mut name_order: Vec<u32> = Vec::new();
        for o in rows.clone() {
            let old = ix.name[o];
            if new_name[old as usize] == u32::MAX {
                new_name[old as usize] = name_order.len() as u32;
                name_order.push(old);
            }
        }
        let name = rows.clone().map(|o| new_name[ix.name[o] as usize]);
        let name_starts = std::iter::once(0u32).chain(name_order.iter().scan(0u32, |end, &old| {
            *end += ix.names.get(old).len() as u32;
            Some(*end)
        }));
        let name_bytes: usize = name_order.iter().map(|&old| ix.names.get(old).len()).sum();
        let names = name_order.iter().flat_map(|&old| ix.names.get(old).bytes());

        let mut dirs = Rank::default();
        rows.clone().for_each(|o| dirs.push(ix.dirs.contains(o)));
        // Children were numbered consecutively, from the first one met.
        let child_first = folders
            .clone()
            .map(|o| ix.children(EntryId(o as u32)).next().map_or(0, |c| self.new_id[c.index()]));
        let child_count = folders.clone().map(|o| ix.children(EntryId(o as u32)).count() as u32);

        // Owners are numbered in order of first use too; unreferenced ones are dropped.
        let mut new_owner = vec![u32::MAX; ix.owners.len()];
        let mut owners: Vec<&Owner> = Vec::new();
        let mut owner = SparseColumn::default();
        for o in rows.clone() {
            owner.push(ix.owner.get(o).map(|old| {
                let old = old as usize;
                if new_owner[old] == u32::MAX {
                    new_owner[old] = owners.len() as u32;
                    owners.push(&ix.owners[old]);
                }
                u64::from(new_owner[old])
            }));
        }

        let mut s = serializer.serialize_struct("Index", 16)?;
        s.serialize_field("parent", &Seq::new(n, parent))?;
        s.serialize_field("name", &Seq::new(n, name))?;
        s.serialize_field("flags", &Seq::new(n, rows.clone().map(|o| ix.flags[o] & (DIR | HARDLINK))))?;
        s.serialize_field("logical", &ix.logical.reordered(order))?;
        s.serialize_field("on_disk", &ix.on_disk.reordered(order))?;
        s.serialize_field("modified", &ix.modified.reordered(order))?;
        s.serialize_field("created", &ix.created.reordered(order))?;
        s.serialize_field("key", &ix.key.reordered(order))?;
        s.serialize_field("dirs", &dirs)?;
        s.serialize_field("child_first", &Seq::new(dirs.total(), child_first))?;
        s.serialize_field("child_count", &Seq::new(dirs.total(), child_count))?;
        s.serialize_field(
            "names",
            &NamesView {
                starts: Seq::new(name_order.len() + 1, name_starts),
                // A String serialises as its length and bytes, as a sequence of bytes does.
                bytes: Seq::new(name_bytes, names),
            },
        )?;
        s.serialize_field("volumes", &self.volumes)?;
        s.serialize_field("live", &n)?;
        s.serialize_field("owner", &owner)?;
        s.serialize_field("owners", &owners)?;
        s.end()
    }
}

/// Serialises as a [`Names`] would.
#[derive(Serialize)]
struct NamesView<S, B> {
    starts: S,
    bytes: B,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owner::OwnerKind;

    fn meta(is_dir: bool, size: u64) -> EntryMeta {
        EntryMeta {
            is_dir,
            logical: size,
            on_disk: size,
            modified: 1_700_000_000_123_456_789,
            created: -5_500_000_000,
            key: size,
            hardlinked: false,
        }
    }

    /// An Index with removed Entries, a rename, a moved folder and wide values, so ids, slots
    /// and names all need renumbering when saved.
    fn churned() -> Index {
        let mut index = Index::new();
        let v = index.add_volume(Path::new("/v"), "ext4", "uuid", meta(true, 0), SyncState::Walk);
        let root = index.volume(v).unwrap().root;
        let a = index.insert(root, "a", meta(true, 1));
        let b = index.insert(root, "b", meta(true, 2));
        let gone = index.insert(a, "gone", meta(true, 3));
        index.insert(gone, "deep", meta(false, 4));
        index.insert(a, "big", meta(false, 5 << 32));
        let c = index.insert(b, "c", meta(true, 6));
        index.insert(c, "file", meta(false, u64::MAX));
        // An Owner root that is then removed: its Owner is kept in the table but not saved.
        index.set_owners(root, vec![(gone, owner(OwnerKind::Project, "gone"))]);
        index.remove(gone);
        index.rename(c, a, "renamed");
        index.add_volume(Path::new("/w"), "ext4", "uuid2", meta(true, 0), SyncState::Walk);
        // Two roots sharing one Owner; the detection replaces the Location's earlier roots.
        index.set_owners(
            root,
            vec![
                (a, owner(OwnerKind::Game, "Same")),
                (b, owner(OwnerKind::Game, "Same")),
                (c, owner(OwnerKind::Project, "c")),
            ],
        );
        index
    }

    #[test]
    fn the_streamed_save_is_byte_for_byte_the_compacted_copy() {
        let index = churned();
        assert_eq!(
            bincode::serialize(&index.save_view()).unwrap(),
            bincode::serialize(&index.compacted()).unwrap()
        );
    }

    /// The columns formats 5 and 6 share, taken from `index` through its accessors.
    struct OldColumns {
        parent: Vec<u32>,
        name_start: Vec<u32>,
        name_len: Vec<u16>,
        flags: Vec<u8>,
        slot: Vec<u32>,
        children: Vec<Vec<EntryId>>,
        names: String,
    }

    fn old_columns(index: &Index) -> OldColumns {
        let mut old = OldColumns {
            parent: Vec::new(),
            name_start: Vec::new(),
            name_len: Vec::new(),
            flags: Vec::new(),
            slot: Vec::new(),
            children: Vec::new(),
            names: String::new(),
        };
        for i in 0..index.capacity() {
            let id = EntryId(i as u32);
            old.parent.push(index.parent(id).map_or(NO_PARENT, |p| p.0));
            old.name_start.push(old.names.len() as u32);
            old.name_len.push(index.name(id).len() as u16);
            old.names.push_str(index.name(id));
            old.flags.push(index.flags[i]);
            match index.folder_slot(id) {
                Some(slot) => {
                    old.slot.push(slot as u32);
                    old.children.push(index.children(id).collect());
                }
                None => old.slot.push(NO_SLOT),
            }
        }
        old
    }

    fn load_bytes(version: u32, body: Vec<u8>) -> Index {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        let mut bytes = MAGIC.to_vec();
        bytes.extend(version.to_le_bytes());
        bytes.extend(body);
        fs::write(&path, bytes).unwrap();
        Index::load(&path).unwrap()
    }

    /// `index` with every Owner forgotten, as a format before 8 would hold it.
    fn unowned(mut index: Index) -> Index {
        let roots: Vec<EntryId> = index.volumes().map(|(_, v)| v.root).collect();
        for root in roots {
            index.set_owners(root, Vec::new());
        }
        index
    }

    #[test]
    fn a_format_5_index_loads_with_every_value() {
        let index = unowned(churned()).compacted();
        let old = old_columns(&index);
        let ids = (0..index.capacity()).map(|i| EntryId(i as u32));
        // Format 5 kept full u64 sizes and i64 nanosecond times.
        #[derive(Serialize)]
        struct V5<'a> {
            parent: &'a [u32],
            name_start: &'a [u32],
            name_len: &'a [u16],
            flags: &'a [u8],
            logical: Vec<u64>,
            on_disk: Vec<u64>,
            modified: Vec<i64>,
            created: Vec<i64>,
            key: Vec<u64>,
            slot: &'a [u32],
            children: &'a [Vec<EntryId>],
            names: &'a str,
            volumes: &'a [Option<Volume>],
            live: usize,
        }
        let v5 = V5 {
            parent: &old.parent,
            name_start: &old.name_start,
            name_len: &old.name_len,
            flags: &old.flags,
            logical: ids.clone().map(|id| index.logical(id)).collect(),
            on_disk: ids.clone().map(|id| index.on_disk(id)).collect(),
            modified: ids.clone().map(|id| index.modified(id) + 123_456_789).collect(),
            created: ids.clone().map(|id| index.created(id) + 999_999_999).collect(),
            key: ids.clone().map(|id| index.key(id)).collect(),
            slot: &old.slot,
            children: &old.children,
            names: &old.names,
            volumes: &index.volumes,
            live: index.live,
        };
        let loaded = load_bytes(5, bincode::serialize(&v5).unwrap());
        assert_eq!(
            bincode::serialize(&loaded.save_view()).unwrap(),
            bincode::serialize(&index.save_view()).unwrap()
        );
        let file = loaded.find_path(Path::new("/v/a/renamed/file")).unwrap();
        assert_eq!((loaded.logical(file), loaded.created(file)), (u64::MAX, -6_000_000_000));
    }

    #[test]
    fn a_format_6_index_loads_with_every_value() {
        let index = unowned(churned()).compacted();
        let old = old_columns(&index);
        let ids = (0..index.capacity()).map(|i| EntryId(i as u32));
        let column = |values: Vec<u64>| {
            let mut c = U64Column::default();
            values.into_iter().for_each(|v| c.push(v));
            c
        };
        #[derive(Serialize)]
        struct V6<'a> {
            parent: &'a [u32],
            name_start: &'a [u32],
            name_len: &'a [u16],
            flags: &'a [u8],
            logical: U64Column,
            on_disk: U64Column,
            modified: Vec<i64>,
            created: U64Column,
            key: Vec<u64>,
            slot: &'a [u32],
            children: &'a [Vec<EntryId>],
            names: &'a str,
            volumes: &'a [Option<Volume>],
            live: usize,
        }
        let v6 = V6 {
            parent: &old.parent,
            name_start: &old.name_start,
            name_len: &old.name_len,
            flags: &old.flags,
            logical: column(ids.clone().map(|id| index.logical(id)).collect()),
            on_disk: column(ids.clone().map(|id| index.on_disk(id)).collect()),
            modified: ids.clone().map(|id| index.modified(id) + 123_456_789).collect(),
            created: column(ids.clone().map(|id| secs(index.created(id))).collect()),
            key: ids.clone().map(|id| index.key(id)).collect(),
            slot: &old.slot,
            children: &old.children,
            names: &old.names,
            volumes: &index.volumes,
            live: index.live,
        };
        let loaded = load_bytes(6, bincode::serialize(&v6).unwrap());
        assert_eq!(
            bincode::serialize(&loaded.save_view()).unwrap(),
            bincode::serialize(&index.save_view()).unwrap()
        );
        let big = loaded.find_path(Path::new("/v/a/big")).unwrap();
        assert_eq!((loaded.logical(big), loaded.on_disk(big)), (5 << 32, 5 << 32));
        assert_eq!(loaded.modified(big), 1_700_000_000_000_000_000);
    }

    fn sorted_names(index: &Index, dir: EntryId) -> Vec<String> {
        let mut names: Vec<String> = index.children(dir).map(|c| index.name(c).to_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn children_added_moved_and_removed_after_a_load_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        churned().save(&path).unwrap();
        let mut index = Index::load(&path).unwrap();
        let root = index.find_path(Path::new("/v")).unwrap();
        let a = index.find_path(Path::new("/v/a")).unwrap();
        let b = index.find_path(Path::new("/v/b")).unwrap();
        let renamed = index.find_path(Path::new("/v/a/renamed")).unwrap();
        assert_eq!(sorted_names(&index, a), ["big", "renamed"]);
        assert!(!index.has_children(b));

        // New ids are far from a's run: they go aside and are still its children.
        let x = index.insert(a, "x", meta(false, 7));
        let y = index.insert(a, "y", meta(false, 8));
        assert_eq!(sorted_names(&index, a), ["big", "renamed", "x", "y"]);
        // A move out of the run and back into another folder, then into its own run again.
        index.rename(renamed, b, "renamed");
        assert_eq!(sorted_names(&index, a), ["big", "x", "y"]);
        assert_eq!(sorted_names(&index, b), ["renamed"]);
        index.rename(renamed, a, "back");
        assert_eq!(sorted_names(&index, a), ["back", "big", "x", "y"]);
        assert_eq!(sorted_names(&index, b), Vec::<String>::new());
        // Removing one aside child and one run child.
        index.remove_children(a, &[x, index.child_named(a, "big").unwrap()]);
        assert_eq!(sorted_names(&index, a), ["back", "y"]);
        assert_eq!(index.live_count(), 7);
        // Removing a folder removes what is beneath it, run and aside alike.
        index.remove(a);
        assert_eq!(sorted_names(&index, root), ["b"]);
        assert!(!index.is_live(y) && !index.is_live(renamed));
        assert_eq!(index.live_count(), 3);
        assert!(index.extra_children.is_empty());
        assert_eq!(
            bincode::serialize(&index.save_view()).unwrap(),
            bincode::serialize(&index.compacted()).unwrap()
        );
    }

    #[test]
    fn sizes_and_times_survive_moving_between_predicted_and_stored() {
        let mut index = Index::new();
        let v = index.add_volume(Path::new("/v"), "ext4", "uuid", meta(true, 0), SyncState::Walk);
        let root = index.volume(v).unwrap().root;
        let usual = EntryMeta {
            is_dir: false,
            logical: 5000,
            on_disk: 8192,
            modified: 1_700_000_000_000_000_000,
            created: 1_700_000_000_000_000_000,
            key: 9,
            hardlinked: false,
        };
        let f = index.insert(root, "f", usual);
        assert_eq!((index.on_disk(f), index.created(f)), (8192, 1_700_000_000_000_000_000));
        assert!(index.on_disk.get(f.index()).is_none() && index.created.get(f.index()).is_none());

        // Written again: the time moves, the creation time stays and is now an exception.
        let touched = EntryMeta {
            modified: 1_700_000_100_000_000_000,
            ..usual
        };
        index.update(f, touched);
        assert_eq!(
            (index.modified(f), index.created(f)),
            (1_700_000_100_000_000_000, 1_700_000_000_000_000_000)
        );
        // Compressed on disk, then sparse, then back to the usual allocation.
        index.update(f, EntryMeta { on_disk: 4096, ..touched });
        assert_eq!(index.on_disk(f), 4096);
        index.update(f, EntryMeta { on_disk: 0, ..touched });
        assert_eq!(index.on_disk(f), 0);
        index.update(f, EntryMeta { on_disk: 8192, ..touched });
        assert_eq!(index.on_disk(f), 8192);
        // An unknown creation time is kept apart from a known modification time.
        index.update(f, EntryMeta { created: 0, ..touched });
        assert_eq!(index.created(f), 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        index.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        let f = back.find_path(Path::new("/v/f")).unwrap();
        assert_eq!(back.meta(f), EntryMeta { created: 0, ..touched });
        // A folder's sizes are its own (0): nothing stored for it.
        assert!(back.on_disk.get(root.index()).is_none());
    }

    #[test]
    fn names_are_stored_once_and_renumbered_in_first_use_order_when_saved() {
        let mut index = Index::new();
        let v = index.add_volume(Path::new("/v"), "ext4", "uuid", meta(true, 0), SyncState::Walk);
        let root = index.volume(v).unwrap().root;
        let a = index.insert(root, "a", meta(true, 1));
        let b = index.insert(root, "b", meta(true, 2));
        let gone = index.insert(root, "only-here", meta(false, 3));
        for dir in [a, b] {
            index.insert(dir, "same.txt", meta(false, 4));
        }
        index.remove(gone);
        assert_eq!(index.names.len(), 5, "/v, a, b, only-here, same.txt");
        index.idle();
        assert!(index.names.lookup.is_none());
        index.insert(a, "b", meta(false, 5));
        assert_eq!(index.names.len(), 5, "an interned name is found again after idle()");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        index.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        assert_eq!(back.names.len(), 4, "the removed Entry's name is not saved");
        assert_eq!(back.live_count(), 6);
        assert_eq!(sorted_names(&back, back.find_path(Path::new("/v/a")).unwrap()), ["b", "same.txt"]);
    }

    fn owner(kind: OwnerKind, name: &str) -> Owner {
        Owner::new(kind, name, "test")
    }

    #[test]
    fn owners_are_inherited_from_the_nearest_root_and_survive_a_save() {
        let mut index = churned();
        let a = index.find_path(Path::new("/v/a")).unwrap();
        let renamed = index.find_path(Path::new("/v/a/renamed")).unwrap();
        let file = index.find_path(Path::new("/v/a/renamed/file")).unwrap();
        let big = index.find_path(Path::new("/v/a/big")).unwrap();
        let b = index.find_path(Path::new("/v/b")).unwrap();
        let v = index.find_path(Path::new("/v")).unwrap();
        let w = index.find_path(Path::new("/w")).unwrap();
        index.set_owners(w, vec![(w, owner(OwnerKind::System, "w"))]);
        index.set_owners(
            v,
            vec![
                (a, owner(OwnerKind::Game, "Factorio")),
                (renamed, owner(OwnerKind::Project, "renamed")),
            ],
        );
        let name_of = |index: &Index, id| index.owner_of(id).map(|(root, o)| (root, index.owner(o).name.clone()));
        assert_eq!(name_of(&index, file), Some((renamed, "renamed".to_owned())));
        assert_eq!(name_of(&index, big), Some((a, "Factorio".to_owned())));
        assert_eq!(name_of(&index, b), None);
        assert_eq!(name_of(&index, w), Some((w, "w".to_owned())));
        assert!(!index.owners_stale());

        // Detecting the Location again replaces its Owners and leaves the other Location's.
        index.set_owners(v, vec![(b, owner(OwnerKind::Program, "B"))]);
        assert_eq!(name_of(&index, file), None);
        assert_eq!(name_of(&index, b), Some((b, "B".to_owned())));
        assert_eq!(name_of(&index, w), Some((w, "w".to_owned())));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.bin");
        index.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        assert!(!back.owners_stale());
        let b = back.find_path(Path::new("/v/b")).unwrap();
        let w = back.find_path(Path::new("/w")).unwrap();
        assert_eq!(name_of(&back, b), Some((b, "B".to_owned())));
        assert_eq!(name_of(&back, w), Some((w, "w".to_owned())));
        assert_eq!(back.owners.len(), 2, "unreferenced Owners are not saved");
        let mut roots: Vec<(EntryId, OwnerKind)> = back.owner_roots().map(|(id, o)| (id, back.owner(o).kind)).collect();
        roots.sort();
        assert_eq!(roots, [(b, OwnerKind::Program), (w, OwnerKind::System)]);
    }

    #[test]
    fn a_format_7_index_loads_without_owners_and_asks_for_detection() {
        let index = unowned(churned()).compacted();
        let v7 = IndexV7 {
            parent: index.parent.clone(),
            name: index.name.clone(),
            flags: index.flags.clone(),
            logical: index.logical.clone(),
            on_disk: index.on_disk.clone(),
            modified: index.modified.clone(),
            created: index.created.clone(),
            key: index.key.clone(),
            dirs: index.dirs.clone(),
            child_first: index.child_first.clone(),
            child_count: index.child_count.clone(),
            names: Names {
                starts: index.names.starts.clone(),
                bytes: index.names.bytes.clone(),
                lookup: None,
            },
            volumes: index.volumes.clone(),
            live: index.live,
        };
        let loaded = load_bytes(7, bincode::serialize(&v7).unwrap());
        assert!(loaded.owners_stale());
        assert_eq!(loaded.owner_roots().count(), 0);
        assert_eq!(
            bincode::serialize(&loaded.save_view()).unwrap(),
            bincode::serialize(&index.save_view()).unwrap()
        );
    }
}
