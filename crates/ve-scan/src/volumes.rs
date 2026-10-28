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
    /// Mounted read-only: Trash is unavailable.
    pub read_only: bool,
}

/// Local Volumes that can be indexed, fixed and removable. Network filesystems, pseudo
/// filesystems and repeated mounts of the same filesystem are left out.
pub fn discover_volumes() -> Vec<VolumeInfo> {
    let mut volumes = crate::platform::discover_volumes();
    volumes.sort_by(|a, b| a.mount.cmp(&b.mount));
    volumes
}
