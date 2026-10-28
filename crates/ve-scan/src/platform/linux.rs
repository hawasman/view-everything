use crate::volumes::{VolumeInfo, share_identity};
use std::fs;
use std::path::{Path, PathBuf};

/// Filesystems backed by local storage. Everything else (pseudo, overlay and image filesystems
/// such as squashfs) is never indexed, except the network ones below.
const LOCAL_FILESYSTEMS: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "zfs", "bcachefs", "jfs", "reiserfs", "nilfs2",
    "ntfs", "ntfs3", "fuseblk", "vfat", "exfat", "hfsplus",
];

/// Network filesystems, offered as Volumes the user can opt into.
const NETWORK_FILESYSTEMS: &[&str] = &["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs"];

struct Mount {
    device: String,
    /// Path inside the filesystem that is mounted; not `/` for bind mounts and btrfs subvolumes.
    root: String,
    mount: PathBuf,
    fs_type: String,
    source: String,
    read_only: bool,
}

impl Mount {
    fn is_network(&self) -> bool {
        NETWORK_FILESYSTEMS.contains(&self.fs_type.as_str())
    }
}

pub fn discover_volumes() -> Vec<VolumeInfo> {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    volumes_in(&text)
}

fn volumes_in(text: &str) -> Vec<VolumeInfo> {
    let mut mounts: Vec<Mount> = text
        .lines()
        .filter_map(parse_line)
        .filter(|m| LOCAL_FILESYSTEMS.contains(&m.fs_type.as_str()) || m.is_network())
        .collect();
    // Shortest mount paths first, so the first mount of a filesystem tree wins.
    mounts.sort_by_key(|m| m.mount.as_os_str().len());

    let mut kept: Vec<Mount> = Vec::new();
    for mount in mounts {
        // Network shares are told apart by identity later; they have no device to compare.
        let repeats_kept = !mount.is_network()
            && kept
                .iter()
                .any(|k| k.device == mount.device && is_within(&mount.root, &k.root));
        if !repeats_kept {
            kept.push(mount);
        }
    }
    let uuids = Path::new("/dev/disk/by-uuid");
    kept.into_iter()
        .map(|m| VolumeInfo {
            removable: !m.is_network() && is_removable(&m.device),
            network: m.is_network(),
            identity: if m.is_network() { share_identity(&m.source) } else { identity(&m, uuids) },
            label: m.source,
            mount: m.mount,
            fs_type: m.fs_type,
            read_only: m.read_only,
        })
        .collect()
}

/// The filesystem UUID, plus the path within it for a btrfs subvolume or bind mount (one UUID
/// then covers several Volumes); device and mount path when udev lists no UUID for the device.
fn identity(m: &Mount, by_uuid: &Path) -> String {
    let name = fs::canonicalize(Path::new("/sys/dev/block").join(&m.device)).ok();
    match name.as_ref().and_then(|n| uuid_of(n.file_name()?, by_uuid)) {
        Some(uuid) if m.root == "/" => format!("uuid:{uuid}"),
        Some(uuid) => format!("uuid:{uuid}:{}", m.root),
        None => format!("dev:{}@{}", m.device, m.mount.display()),
    }
}

/// Finds the entry of `by_uuid` (`/dev/disk/by-uuid`) that points at the kernel device `name`.
fn uuid_of(name: &std::ffi::OsStr, by_uuid: &Path) -> Option<String> {
    fs::read_dir(by_uuid)
        .ok()?
        .flatten()
        .find(|e| fs::read_link(e.path()).is_ok_and(|t| t.file_name() == Some(name)))
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

/// Whether filesystem path `inner` is `outer` or lies beneath it.
fn is_within(inner: &str, outer: &str) -> bool {
    outer == "/"
        || inner == outer
        || inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Parses one line of `/proc/self/mountinfo` (see proc_pid_mountinfo(5)).
fn parse_line(line: &str) -> Option<Mount> {
    let (left, right) = line.split_once(" - ")?;
    let fields: Vec<&str> = left.split(' ').collect();
    let mut right = right.split(' ');
    let fs_type = right.next()?.to_owned();
    let source = unescape(right.next()?);
    let options = fields.get(5)?;
    Some(Mount {
        device: (*fields.get(2)?).to_owned(),
        root: unescape(fields.get(3)?),
        mount: PathBuf::from(unescape(fields.get(4)?)),
        fs_type,
        source,
        read_only: options.split(',').any(|o| o == "ro"),
    })
}

/// Decodes the octal escapes (`\040` for a space) mountinfo uses in paths.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether the block device `major:minor` is removable media or attached over USB.
fn is_removable(device: &str) -> bool {
    let Ok(sys) = fs::canonicalize(Path::new("/sys/dev/block").join(device)) else {
        return false;
    };
    if sys
        .components()
        .any(|c| c.as_os_str().to_str().is_some_and(|s| s.starts_with("usb")))
    {
        return true;
    }
    // A partition's `removable` flag lives on its parent disk.
    [sys.join("removable"), sys.join("../removable")]
        .iter()
        .any(|flag| fs::read_to_string(flag).is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uuid_is_the_by_uuid_entry_pointing_at_the_device() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("../../sdb1", dir.path().join("1234-ABCD")).unwrap();
        std::os::unix::fs::symlink("../../dm-0", dir.path().join("f00-ba4")).unwrap();
        assert_eq!(uuid_of("sdb1".as_ref(), dir.path()).as_deref(), Some("1234-ABCD"));
        assert_eq!(uuid_of("dm-0".as_ref(), dir.path()).as_deref(), Some("f00-ba4"));
        assert_eq!(uuid_of("sdc1".as_ref(), dir.path()), None);
    }

    #[test]
    fn network_mounts_are_found_and_pseudo_ones_are_not() {
        let mounts: Vec<Mount> = [
            "40 29 0:35 / /mnt/nas rw,relatime shared:1 - nfs4 nas.lan:/volume1/media rw,vers=4.2",
            "41 29 0:36 / /mnt/win\\040share rw - cifs //server/my\\040share rw",
            "42 29 0:37 / /mnt/ssh rw - fuse.sshfs me@host:/srv rw",
            "43 29 0:38 / /proc rw - proc proc rw",
            "44 29 0:39 / /mnt/fuse rw - fuse.portal portal rw",
        ]
        .iter()
        .filter_map(|l| parse_line(l))
        .filter(|m| m.is_network())
        .collect();
        let sources: Vec<_> = mounts.iter().map(|m| m.source.as_str()).collect();
        assert_eq!(sources, ["nas.lan:/volume1/media", "//server/my share", "me@host:/srv"]);
        assert_eq!(mounts[1].mount, PathBuf::from("/mnt/win share"));
    }

    #[test]
    fn a_share_mounted_twice_is_one_opt_in_volume_with_the_share_as_identity() {
        let text = "40 29 0:35 / /mnt/nas rw - nfs4 nas.lan:/volume1/media rw\n\
                    41 29 0:36 / /mnt/nas2 rw - nfs4 nas.lan:/volume1/media rw\n\
                    42 29 0:37 / /mnt/other rw - cifs //server/share rw\n\
                    43 29 0:38 / /proc rw - proc proc rw";
        let volumes = crate::volumes::dedupe_shares(volumes_in(text));
        assert_eq!(volumes.len(), 2);
        let nas = &volumes[0];
        assert_eq!(nas.mount, PathBuf::from("/mnt/nas"));
        assert_eq!(nas.identity, "share:nas.lan:/volume1/media");
        assert!(nas.network && !nas.removable && nas.opt_in());
        assert_eq!(volumes[1].identity, "share://server/share");
    }

    #[test]
    fn a_device_without_a_uuid_is_identified_by_device_and_mount() {
        let mount = Mount {
            device: "9999:1".into(), // no such block device
            root: "/".into(),
            mount: "/mnt/x".into(),
            fs_type: "ext4".into(),
            source: String::new(),
            read_only: false,
        };
        assert_eq!(identity(&mount, Path::new("/nonexistent")), "dev:9999:1@/mnt/x");
    }
}
