use std::path::PathBuf;

/// A mounted Volume that could be indexed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub mount: PathBuf,
    /// The filesystem's own identity (UUID or serial), so a Volume is the same at any mount
    /// path; device and mount path where the platform has none.
    pub identity: String,
    /// Filesystem name as the OS reports it, such as `ext4`, `btrfs` or `NTFS`.
    pub fs_type: String,
    /// Human-readable label or device, for display.
    pub label: String,
    /// Removable media: indexed only when the user asks.
    pub removable: bool,
    /// A mounted network share (NFS, SMB, sshfs): indexed only when the user asks.
    pub network: bool,
    /// Mounted read-only: Trash is unavailable.
    pub read_only: bool,
}

impl VolumeInfo {
    /// Indexed only when the user asks: removable media and network shares.
    pub fn opt_in(&self) -> bool {
        self.removable || self.network
    }
}

/// Volumes that can be indexed: local fixed and removable ones, and mounted network shares.
/// Pseudo filesystems and repeated mounts of the same filesystem are left out.
pub fn discover_volumes() -> Vec<VolumeInfo> {
    let mut volumes = crate::platform::discover_volumes();
    volumes.sort_by(|a, b| a.mount.cmp(&b.mount));
    volumes
}

/// The identity of a network share from its mount source (`nas.lan:/volume1/media`,
/// `//user@server/share`, `\\\\server\\share`), which the share has instead of a UUID. Credentials
/// are dropped and SMB names are case-folded, so the same share has one identity however it
/// was mounted.
pub fn share_identity(source: &str) -> String {
    let source = source.replace('\\', "/");
    let strip_user = |authority: &str| authority.rsplit('@').next().unwrap_or("").to_owned();
    let normal = if let Some(rest) = source.strip_prefix("//") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        format!("//{}/{path}", strip_user(host)).to_lowercase()
    } else if let Some((host, path)) = source.split_once(':') {
        format!("{}:{path}", strip_user(host))
    } else {
        source
    };
    format!("share:{normal}")
}

#[cfg(test)]
mod tests {
    use super::share_identity;

    #[test]
    fn a_share_has_one_identity_however_it_was_mounted() {
        assert_eq!(share_identity("nas.lan:/volume1/media"), "share:nas.lan:/volume1/media");
        assert_eq!(share_identity("me@host:/srv"), "share:host:/srv");
        let smb = "share://server/share";
        assert_eq!(share_identity("//server/share"), smb);
        assert_eq!(share_identity("//guest@SERVER/Share"), smb);
        assert_eq!(share_identity(r"\\Server\share"), smb);
    }
}
