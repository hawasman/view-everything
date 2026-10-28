use crate::volumes::VolumeInfo;
use std::ffi::CStr;
use std::path::PathBuf;

const LOCAL_FILESYSTEMS: &[&str] = &["apfs", "hfs", "msdos", "exfat", "ntfs"];

/// System-managed APFS volumes that hold no user files worth indexing.
const SYSTEM_VOLUMES: &[&str] = &[
    "/System/Volumes/VM",
    "/System/Volumes/Preboot",
    "/System/Volumes/Update",
    "/System/Volumes/xarts",
    "/System/Volumes/iSCPreboot",
    "/System/Volumes/Hardware",
    "/Volumes/Recovery",
];

pub fn discover_volumes() -> Vec<VolumeInfo> {
    let mut mounts: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `mounts` at a buffer it owns, valid until the next call on this
    // thread; it is only read here.
    let count = unsafe { libc::getmntinfo(&mut mounts, libc::MNT_NOWAIT) };
    if count <= 0 || mounts.is_null() {
        return Vec::new();
    }
    // SAFETY: getmntinfo returned `count` initialised entries at `mounts`.
    let mounts = unsafe { std::slice::from_raw_parts(mounts, count as usize) };
    mounts
        .iter()
        .filter(|m| m.f_flags & libc::MNT_LOCAL as u32 != 0)
        .filter_map(|m| {
            let fs_type = c_text(&m.f_fstypename);
            let mount = c_text(&m.f_mntonname);
            if !LOCAL_FILESYSTEMS.contains(&fs_type.as_str())
                || SYSTEM_VOLUMES.contains(&mount.as_str())
            {
                return None;
            }
            Some(VolumeInfo {
                removable: mount.starts_with("/Volumes/"),
                read_only: m.f_flags & libc::MNT_RDONLY as u32 != 0,
                label: c_text(&m.f_mntfromname),
                mount: PathBuf::from(mount),
                fs_type,
            })
        })
        .collect()
}

fn c_text(field: &[libc::c_char]) -> String {
    // SAFETY: statfs name fields are NUL-terminated within their fixed size.
    unsafe { CStr::from_ptr(field.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
