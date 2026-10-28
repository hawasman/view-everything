//! Trash and Empty Trash. The app never deletes permanently except through Empty Trash.

use std::path::PathBuf;

/// Moves `paths` to the operating system's trash.
pub fn move_to_trash(paths: &[PathBuf]) -> Result<(), String> {
    trash::delete_all(paths).map_err(|e| e.to_string())
}

/// Bytes the trash currently holds, or `None` where the platform can't tell.
pub fn trash_size() -> Option<u64> {
    platform::trash_size()
}

/// Whether [`empty_trash`] is supported here.
pub const CAN_EMPTY: bool = platform::CAN_EMPTY;

/// Permanently removes everything in the trash.
pub fn empty_trash() -> Result<(), String> {
    platform::empty_trash()
}

/// Opens the system trash in the file manager.
pub fn open_trash() -> Result<(), String> {
    platform::open_trash()
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::path::Path;

    pub const CAN_EMPTY: bool = true;

    pub fn trash_size() -> Option<u64> {
        let folders = trash::os_limited::trash_folders().ok()?;
        Some(
            folders
                .iter()
                .map(|folder| on_disk_size(&folder.join("files")))
                .sum(),
        )
    }

    pub fn empty_trash() -> Result<(), String> {
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        trash::os_limited::purge_all(items).map_err(|e| e.to_string())
    }

    pub fn open_trash() -> Result<(), String> {
        opener::open("trash:///").map_err(|e| e.to_string())
    }

    fn on_disk_size(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        let own = metadata.blocks() * 512;
        if !metadata.is_dir() {
            return own;
        }
        let children = std::fs::read_dir(path).into_iter().flatten().flatten();
        own + children
            .map(|entry| on_disk_size(&entry.path()))
            .sum::<u64>()
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};

    pub const CAN_EMPTY: bool = true;

    pub fn trash_size() -> Option<u64> {
        let mut info = SHQUERYRBINFO {
            cbSize: size_of::<SHQUERYRBINFO>() as u32,
            i64Size: 0,
            i64NumItems: 0,
        };
        // SAFETY: a null root path queries every drive's recycle bin; `info` is a valid,
        // correctly sized out-parameter.
        let result = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
        (result == 0).then_some(info.i64Size as u64)
    }

    pub fn empty_trash() -> Result<(), String> {
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        trash::os_limited::purge_all(items).map_err(|e| e.to_string())
    }

    pub fn open_trash() -> Result<(), String> {
        std::process::Command::new("explorer.exe")
            .arg("shell:RecycleBinFolder")
            .spawn()
            .map(drop)
            .map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    /// macOS offers no supported way to list or empty the Trash from another app.
    pub const CAN_EMPTY: bool = false;

    pub fn trash_size() -> Option<u64> {
        None
    }

    pub fn empty_trash() -> Result<(), String> {
        Err("emptying the Trash is not supported on macOS; use Finder".into())
    }

    pub fn open_trash() -> Result<(), String> {
        let trash = dirs::home_dir().ok_or("no home folder")?.join(".Trash");
        opener::open(&trash).map_err(|e| e.to_string())
    }
}
