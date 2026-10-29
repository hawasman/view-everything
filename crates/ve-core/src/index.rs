use serde::{Deserialize, Serialize};
use std::fs;
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
}

impl Volume {
    /// The Remote this Location is, if it is one.
    pub fn remote(&self) -> Option<Remote> {
        self.sync.remote()
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

const MAGIC: &[u8; 8] = b"VEINDEX\0";
/// v2 adds Last Synced and capacity per Volume; v3 adds the Volume's identity; v4 adds Remotes.
const FORMAT_VERSION: u32 = 4;

/// The persistent record of every Entry on every Indexed Volume.
///
/// Stored column-wise to keep per-Entry overhead small. Removed Entries leave tombstones that
/// are dropped when the Index is saved.
#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    parent: Vec<u32>,
    name_start: Vec<u32>,
    name_len: Vec<u16>,
    flags: Vec<u8>,
    logical: Vec<u64>,
    on_disk: Vec<u64>,
    modified: Vec<i64>,
    key: Vec<u64>,
    /// Folder slot per Entry, or `NO_SLOT` for files.
    slot: Vec<u32>,
    /// Children of each folder, by folder slot.
    children: Vec<Vec<EntryId>>,
    names: String,
    volumes: Vec<Option<Volume>>,
    live: usize,
    #[serde(skip)]
    generation: u64,
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
        };
        let id = VolumeId(self.volumes.len() as u32);
        self.volumes.push(Some(volume));
        id
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
        let (start, len) = self.store_name(&mount.to_string_lossy());
        self.name_start[root.index()] = start;
        self.name_len[root.index()] = len;
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
        let slot = self.slot[parent.index()] as usize;
        self.children[slot].push(id);
        id
    }

    fn push(&mut self, parent: u32, name: &str, meta: EntryMeta) -> EntryId {
        let id = EntryId(u32::try_from(self.parent.len()).expect("more than 4 billion Entries"));
        self.parent.push(parent);
        let (start, len) = self.store_name(name);
        self.name_start.push(start);
        self.name_len.push(len);
        self.flags.push(flags_for(&meta));
        self.logical.push(meta.logical);
        self.on_disk.push(meta.on_disk);
        self.modified.push(meta.modified);
        self.key.push(meta.key);
        if meta.is_dir {
            self.slot.push(self.children.len() as u32);
            self.children.push(Vec::new());
        } else {
            self.slot.push(NO_SLOT);
        }
        self.live += 1;
        self.touch();
        id
    }

    fn store_name(&mut self, name: &str) -> (u32, u16) {
        let start = u32::try_from(self.names.len()).expect("Index name storage exceeds 4 GiB");
        let len = u16::try_from(name.len()).expect("file name longer than 65535 bytes");
        self.names.push_str(name);
        (start, len)
    }

    /// Replaces an Entry's sizes, time and identity. Whether it is a folder cannot change;
    /// remove and re-insert instead.
    pub fn update(&mut self, id: EntryId, meta: EntryMeta) {
        let i = id.index();
        debug_assert_eq!(self.flags[i] & DIR != 0, meta.is_dir);
        let flags = (self.flags[i] & !HARDLINK) | if meta.hardlinked { HARDLINK } else { 0 };
        if self.flags[i] == flags
            && self.logical[i] == meta.logical
            && self.on_disk[i] == meta.on_disk
            && self.modified[i] == meta.modified
            && self.key[i] == meta.key
        {
            return;
        }
        self.flags[i] = flags;
        self.logical[i] = meta.logical;
        self.on_disk[i] = meta.on_disk;
        self.modified[i] = meta.modified;
        self.key[i] = meta.key;
        self.touch();
    }

    /// Moves an Entry (and everything beneath it) to a new parent folder and name.
    pub fn rename(&mut self, id: EntryId, new_parent: EntryId, new_name: &str) {
        debug_assert!(self.is_dir(new_parent) && self.is_live(new_parent));
        self.detach(id);
        self.parent[id.index()] = new_parent.0;
        let slot = self.slot[new_parent.index()] as usize;
        self.children[slot].push(id);
        if self.name(id) != new_name {
            let (start, len) = self.store_name(new_name);
            self.name_start[id.index()] = start;
            self.name_len[id.index()] = len;
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
        let slot = self.slot[dir.index()] as usize;
        self.children[slot].retain(|c| !gone.contains(c));
        for &id in ids {
            if self.is_live(id) {
                self.tombstone(id);
            }
        }
        self.touch();
    }

    fn tombstone(&mut self, id: EntryId) {
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            let i = current.index();
            self.flags[i] |= REMOVED;
            self.live -= 1;
            if self.slot[i] != NO_SLOT {
                stack.append(&mut self.children[self.slot[i] as usize]);
            }
        }
    }

    fn detach(&mut self, id: EntryId) {
        let parent = self.parent[id.index()];
        if parent != NO_PARENT {
            let siblings = &mut self.children[self.slot[parent as usize] as usize];
            if let Some(pos) = siblings.iter().position(|&c| c == id) {
                siblings.swap_remove(pos);
            }
        }
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
        let start = self.name_start[id.index()] as usize;
        &self.names[start..start + self.name_len[id.index()] as usize]
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
        self.logical[id.index()]
    }

    #[inline]
    pub fn on_disk(&self, id: EntryId) -> u64 {
        self.on_disk[id.index()]
    }

    #[inline]
    pub fn modified(&self, id: EntryId) -> i64 {
        self.modified[id.index()]
    }

    #[inline]
    pub fn key(&self, id: EntryId) -> u64 {
        self.key[id.index()]
    }

    pub fn meta(&self, id: EntryId) -> EntryMeta {
        EntryMeta {
            is_dir: self.is_dir(id),
            logical: self.logical(id),
            on_disk: self.on_disk(id),
            modified: self.modified(id),
            key: self.key(id),
            hardlinked: self.is_hardlinked(id),
        }
    }

    /// Children of a folder; empty for files.
    #[inline]
    pub fn children(&self, id: EntryId) -> &[EntryId] {
        match self.slot[id.index()] {
            NO_SLOT => &[],
            slot => &self.children[slot as usize],
        }
    }

    /// Dense number of a folder, below [`Index::folder_slots`]; `None` for files.
    #[inline]
    pub fn folder_slot(&self, id: EntryId) -> Option<usize> {
        match self.slot[id.index()] {
            NO_SLOT => None,
            slot => Some(slot as usize),
        }
    }

    pub fn folder_slots(&self) -> usize {
        self.children.len()
    }

    pub fn child_named(&self, dir: EntryId, name: &str) -> Option<EntryId> {
        self.children(dir).iter().copied().find(|&c| self.name(c) == name)
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

    /// A copy containing only live Entries, with ids renumbered in depth-first order.
    pub fn compacted(&self) -> Index {
        let mut out = Index::new();
        for volume in self.volumes.iter() {
            let Some(volume) = volume else {
                out.volumes.push(None);
                continue;
            };
            let root = out.push(NO_PARENT, self.name(volume.root), self.meta(volume.root));
            out.volumes.push(Some(Volume { root, ..volume.clone() }));
            let mut stack = vec![(volume.root, root)];
            while let Some((old, new)) = stack.pop() {
                for &child in self.children(old) {
                    let copy = out.insert(new, self.name(child), self.meta(child));
                    if self.is_dir(child) {
                        stack.push((child, copy));
                    }
                }
            }
        }
        out
    }

    /// Writes the Index to `path` atomically, readable only by the current user.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let compact = self.compacted();
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
            bincode::serialize_into(&mut out, &compact).map_err(io::Error::other)?;
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
        match u32::from_le_bytes(version) {
            FORMAT_VERSION => {}
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
        let mut index: Index = bincode::deserialize_from(input).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        index.touch();
        Ok(index)
    }
}

fn flags_for(meta: &EntryMeta) -> u8 {
    (if meta.is_dir { DIR } else { 0 }) | (if meta.hardlinked { HARDLINK } else { 0 })
}
