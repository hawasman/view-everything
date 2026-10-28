//! A Volume's total and free capacity, measured when it is synced.

use std::path::Path;
use ve_core::Capacity;

/// Total bytes, and bytes free to an unprivileged user (what `df` calls Avail). `None` if the
/// filesystem cannot say.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // statvfs field widths differ between Unix platforms
pub fn capacity(mount: &Path) -> Option<Capacity> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(mount.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is NUL terminated and `stat` is a valid out pointer; it is only read after
    // statvfs reports success.
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        stat
    };
    let unit = stat.f_frsize as u64;
    Some(Capacity {
        total: stat.f_blocks as u64 * unit,
        free: stat.f_bavail as u64 * unit,
    })
}

#[cfg(windows)]
pub fn capacity(mount: &Path) -> Option<Capacity> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = mount.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL terminated and the out pointers are valid.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some(Capacity { total, free })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_mounted_folder_reports_a_plausible_capacity() {
        let c = super::capacity(&std::env::temp_dir()).expect("temp dir is on a Volume");
        assert!(c.total > 0 && c.free <= c.total);
    }
}
