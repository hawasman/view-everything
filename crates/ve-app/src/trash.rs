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

/// What Empty Trash managed to do.
#[derive(Debug, Default)]
pub struct EmptyOutcome {
    /// Items permanently deleted.
    pub purged: usize,
    /// Items the current user could not delete.
    pub failed: Vec<PurgeFailure>,
}

/// One Trash item Empty Trash could not delete.
#[derive(Debug, Clone)]
pub struct PurgeFailure {
    /// Where the item lived before it was trashed, for display.
    pub original: PathBuf,
    pub error: String,
    /// The item's paths inside the trash folder, when deleting them with administrator rights
    /// would succeed: the failure was a permission error, typically root-owned contents.
    pub admin_paths: Option<Vec<PathBuf>>,
}

/// Permanently removes everything in the trash, item by item, so one item that can't be
/// deleted doesn't stop the others.
pub fn empty_trash() -> Result<EmptyOutcome, String> {
    platform::empty_trash()
}

/// Opens the system trash in the file manager.
pub fn open_trash() -> Result<(), String> {
    platform::open_trash()
}

/// The command [`delete_as_admin`] runs for `paths`, for showing the user before they agree.
pub fn admin_command(paths: &[PathBuf]) -> Vec<String> {
    platform::admin_command(paths)
}

/// Deletes trash items with administrator rights, asking for them for this one command only.
/// Every path must be an item directly inside a trash folder's `files` or `info` folder.
pub fn delete_as_admin(paths: &[PathBuf]) -> Result<(), String> {
    platform::delete_as_admin(paths)
}

/// Empties the trash one item at a time where the `trash` crate can list it (Windows and
/// freedesktop systems). `admin_paths` says how to delete an item the user may not.
#[cfg(not(target_os = "macos"))]
fn purge_each(
    admin_paths: impl Fn(&trash::TrashItem, &trash::Error) -> Option<Vec<PathBuf>>,
) -> Result<EmptyOutcome, String> {
    let items = trash::os_limited::list().map_err(|e| e.to_string())?;
    let mut outcome = EmptyOutcome::default();
    for item in items {
        let original = item.original_path();
        match trash::os_limited::purge_all([&item]) {
            Ok(()) => outcome.purged += 1,
            Err(error) => {
                let admin_paths = admin_paths(&item, &error);
                outcome.failed.push(PurgeFailure {
                    original,
                    error: error.to_string(),
                    admin_paths,
                });
            }
        }
    }
    Ok(outcome)
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::EmptyOutcome;
    use std::path::{Path, PathBuf};

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

    pub fn empty_trash() -> Result<EmptyOutcome, String> {
        super::purge_each(|item, error| {
            let trash::Error::FileSystem { source, .. } = error else {
                return None;
            };
            if source.kind() != std::io::ErrorKind::PermissionDenied {
                return None;
            }
            // On freedesktop systems an item's id is its `.trashinfo` file; the item itself
            // sits in the sibling `files` folder under the same name.
            let info = PathBuf::from(&item.id);
            let trash = info.parent()?.parent()?;
            let file = trash.join("files").join(info.file_stem()?);
            Some(vec![file, info])
        })
    }

    pub fn admin_command(paths: &[PathBuf]) -> Vec<String> {
        let mut command = vec![
            "pkexec".to_owned(),
            RM.to_owned(),
            "-rf".to_owned(),
            "--".to_owned(),
        ];
        command.extend(paths.iter().map(|p| p.display().to_string()));
        command
    }

    pub fn delete_as_admin(paths: &[PathBuf]) -> Result<(), String> {
        let folders = trash::os_limited::trash_folders().map_err(|e| e.to_string())?;
        for path in paths {
            let inside_trash = path.parent().is_some_and(|parent| {
                folders
                    .iter()
                    .any(|t| parent == t.join("files") || parent == t.join("info"))
            });
            if !inside_trash || path.file_name().is_none() {
                return Err(format!("{} is not an item in the Trash", path.display()));
            }
        }
        let status = std::process::Command::new("pkexec")
            .arg(RM)
            .args(["-rf", "--"])
            .args(paths)
            .status()
            .map_err(|e| {
                format!(
                    "could not start pkexec ({e}); run: sudo {RM} -rf -- {}",
                    shell_list(paths)
                )
            })?;
        match status.code() {
            Some(0) => Ok(()),
            // pkexec: 126 = the password prompt was dismissed, 127 = not authorized.
            Some(126) => Err("cancelled".into()),
            Some(127) => Err("not authorized".into()),
            _ => Err(format!("rm failed ({status})")),
        }
    }

    /// `rm` by absolute path: pkexec runs it as root, so it must not be looked up via `PATH`.
    const RM: &str = "/usr/bin/rm";

    fn shell_list(paths: &[PathBuf]) -> String {
        paths
            .iter()
            .map(|p| format!("'{}'", p.display()))
            .collect::<Vec<_>>()
            .join(" ")
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
    use super::EmptyOutcome;
    use std::path::PathBuf;
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

    /// Windows runs the app elevated when it can, so items it can't delete are reported, not
    /// retried with more rights.
    pub fn empty_trash() -> Result<EmptyOutcome, String> {
        super::purge_each(|_, _| None)
    }

    pub fn admin_command(_paths: &[PathBuf]) -> Vec<String> {
        Vec::new()
    }

    pub fn delete_as_admin(_paths: &[PathBuf]) -> Result<(), String> {
        Err("deleting as administrator is only offered on Linux".into())
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
    use super::EmptyOutcome;
    use std::path::PathBuf;

    /// macOS offers no supported way to list or empty the Trash from another app.
    pub const CAN_EMPTY: bool = false;

    pub fn trash_size() -> Option<u64> {
        None
    }

    pub fn empty_trash() -> Result<EmptyOutcome, String> {
        Err("emptying the Trash is not supported on macOS; use Finder".into())
    }

    pub fn admin_command(_paths: &[PathBuf]) -> Vec<String> {
        Vec::new()
    }

    pub fn delete_as_admin(_paths: &[PathBuf]) -> Result<(), String> {
        Err("deleting as administrator is only offered on Linux".into())
    }

    pub fn open_trash() -> Result<(), String> {
        let trash = dirs::home_dir().ok_or("no home folder")?.join(".Trash");
        opener::open(&trash).map_err(|e| e.to_string())
    }
}
