//! Trash and Empty Trash. The app never deletes permanently except through Empty Trash.

use crate::engine::{Repaint, Shared};
use crate::format;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::*;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use ve_core::{EntryId, Index};
use ve_scan::VolumeInfo;

/// Moves `paths` to the operating system's trash.
pub fn move_to_trash(paths: &[PathBuf]) -> Result<(), String> {
    trash::delete_all(paths).map_err(|e| e.to_string())
}

/// Bytes and items the trash currently holds, or `None` where the platform can't tell.
pub fn trash_size() -> Option<(u64, u64)> {
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
fn purge_each(admin_paths: impl Fn(&trash::TrashItem, &trash::Error) -> Option<Vec<PathBuf>>) -> Result<EmptyOutcome, String> {
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

    pub fn trash_size() -> Option<(u64, u64)> {
        let folders = trash::os_limited::trash_folders().ok()?;
        // The `files` folders' own blocks don't count: an empty Trash holds 0 bytes.
        let items = folders
            .iter()
            .flat_map(|folder| std::fs::read_dir(folder.join("files")).into_iter().flatten().flatten());
        Some(items.fold((0, 0), |(bytes, count), entry| (bytes + on_disk_size(&entry.path()), count + 1)))
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
        let mut command = vec!["pkexec".to_owned(), RM.to_owned(), "-rf".to_owned(), "--".to_owned()];
        command.extend(paths.iter().map(|p| p.display().to_string()));
        command
    }

    pub fn delete_as_admin(paths: &[PathBuf]) -> Result<(), String> {
        let folders = trash::os_limited::trash_folders().map_err(|e| e.to_string())?;
        for path in paths {
            let inside_trash = path
                .parent()
                .is_some_and(|parent| folders.iter().any(|t| parent == t.join("files") || parent == t.join("info")));
            if !inside_trash || path.file_name().is_none() {
                return Err(format!("{} is not an item in the Trash", path.display()));
            }
        }
        let status = std::process::Command::new("pkexec")
            .arg(RM)
            .args(["-rf", "--"])
            .args(paths)
            .status()
            .map_err(|e| format!("could not start pkexec ({e}); run: sudo {RM} -rf -- {}", shell_list(paths)))?;
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
        paths.iter().map(|p| format!("'{}'", p.display())).collect::<Vec<_>>().join(" ")
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
        own + children.map(|entry| on_disk_size(&entry.path())).sum::<u64>()
    }
}

#[cfg(windows)]
mod platform {
    use super::EmptyOutcome;
    use std::path::PathBuf;
    use windows_sys::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};

    pub const CAN_EMPTY: bool = true;

    pub fn trash_size() -> Option<(u64, u64)> {
        let mut info = SHQUERYRBINFO {
            cbSize: size_of::<SHQUERYRBINFO>() as u32,
            i64Size: 0,
            i64NumItems: 0,
        };
        // SAFETY: a null root path queries every drive's recycle bin; `info` is a valid,
        // correctly sized out-parameter.
        let result = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
        (result == 0).then_some((info.i64Size as u64, info.i64NumItems as u64))
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

    pub fn trash_size() -> Option<(u64, u64)> {
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

/// The window's side of Trash: confirmation dialogs, the background work behind them, and the
/// Trash size the status bar shows. Cheap to clone into dialog callbacks.
#[derive(Clone)]
pub struct Trash {
    shared: Arc<Shared>,
    repaint: Repaint,
    size: Arc<Mutex<Option<(u64, u64)>>>,
    /// Bumped per measurement so a slow, older one can't overwrite a newer result.
    measures: Arc<AtomicU64>,
}

impl Trash {
    pub fn new(shared: Arc<Shared>, repaint: Repaint) -> Self {
        let trash = Self {
            shared,
            repaint,
            size: Arc::new(Mutex::new(None)),
            measures: Arc::new(AtomicU64::new(0)),
        };
        trash.measure();
        trash
    }

    /// Bytes the Trash holds, as of the last measurement.
    pub fn size(&self) -> Option<u64> {
        self.size.lock().map(|(bytes, _)| bytes)
    }

    /// Items the Trash holds, as of the last measurement.
    pub fn items(&self) -> Option<u64> {
        self.size.lock().map(|(_, items)| items)
    }

    fn measure(&self) {
        let this = self.clone();
        let ticket = this.measures.fetch_add(1, Ordering::SeqCst) + 1;
        std::thread::spawn(move || {
            let size = trash_size();
            if this.measures.load(Ordering::SeqCst) == ticket {
                *this.size.lock() = size;
            }
            (this.repaint)();
        });
    }

    pub fn open(&self, window: &mut Window, cx: &mut App) {
        if let Err(e) = open_trash() {
            window.push_notification(Notification::error(format!("Could not open the Trash: {e}")), cx);
        }
    }

    /// Why none of `ids` can go to the Trash right now, if so.
    pub fn blocker_of(&self, ids: &[EntryId], volumes: &[VolumeInfo]) -> Option<String> {
        let index = self.shared.index.read();
        ids.iter().find_map(|&id| blocker(&index, volumes, id))
    }

    /// Asks before moving `ids` to the Trash. Says why instead when they can't go.
    pub fn ask_trash(&self, ids: &[EntryId], volumes: &[VolumeInfo], window: &mut Window, cx: &mut App) {
        let (items, total) = {
            let index = self.shared.index.read();
            let ids = top_level(&index, ids);
            if let Some(reason) = ids.iter().find_map(|&id| blocker(&index, volumes, id)) {
                window.push_notification(Notification::warning(reason), cx);
                return;
            }
            let total: u64 = ids.iter().map(|&id| subtree_on_disk(&index, id)).sum();
            let items: Vec<PathBuf> = ids.iter().map(|&id| index.path(id)).collect();
            (items, total)
        };
        if items.is_empty() {
            return;
        }
        let this = self.clone();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
            let paths = items.clone();
            let mut list = v_flex().gap_1().font_family(cx.theme().mono_font_family.clone());
            for path in items.iter().take(8) {
                list = list.child(path.display().to_string());
            }
            if items.len() > 8 {
                list = list.child(format!("…and {} more", items.len() - 8));
            }
            dialog
                .confirm()
                .title(if items.len() == 1 {
                    "Move to Trash?"
                } else {
                    "Move these items to Trash?"
                })
                .description(format!(
                    "{} item(s), {} on disk. They can be restored from the Trash, and keep using space until the Trash is emptied.",
                    items.len(),
                    format::size(total)
                ))
                .child(list)
                .ok_text("Move to Trash")
                .on_ok(move |_, window, cx| {
                    this.start_trash(paths.clone(), window, cx);
                    true
                })
        });
    }

    /// Asks before permanently deleting everything in the Trash.
    pub fn ask_empty(&self, window: &mut Window, cx: &mut App) {
        let this = self.clone();
        let amount = self.size().map_or_else(|| "everything".to_owned(), format::size);
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            dialog
                .confirm()
                .title("Empty the Trash?")
                .description(format!("This permanently deletes {amount} in the Trash. It cannot be undone."))
                .ok_text("Empty Trash")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    this.start_empty(window, cx);
                    true
                })
        });
    }

    /// Offers to delete, as administrator, Trash items the user may not delete.
    fn ask_admin(&self, failures: Vec<PurgeFailure>, window: &mut Window, cx: &mut App) {
        let paths: Vec<PathBuf> = failures.iter().flat_map(|f| f.admin_paths.iter().flatten().cloned()).collect();
        let command = admin_command(&paths).join(" ");
        let this = self.clone();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
            let paths = paths.clone();
            let mono = cx.theme().mono_font_family.clone();
            let mut list = v_flex().gap_1();
            for failure in &failures {
                list = list.child(failure.original.display().to_string());
            }
            dialog
                .confirm()
                .title("Administrator rights needed")
                .description(
                    "These Trash items contain files your user may not delete, usually because \
                     something created them as root. Delete them as administrator?",
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(list)
                        .child("You will be asked for your password once. Only this command runs as root:")
                        .child(
                            div()
                                .p_2()
                                .rounded_md()
                                .bg(cx.theme().muted)
                                .font_family(mono)
                                .child(command.clone()),
                        ),
                )
                .ok_text("Delete as administrator")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Leave in Trash")
                .on_ok(move |_, window, cx| {
                    this.start_admin_delete(paths.clone(), window, cx);
                    true
                })
        });
    }

    fn start_trash(&self, items: Vec<PathBuf>, window: &mut Window, cx: &mut App) {
        let this = self.clone();
        window
            .spawn(cx, async move |cx| {
                let outcome = cx
                    .background_executor()
                    .spawn(async move {
                        // Check the disk first: anything already gone just leaves the Index.
                        let (present, missing): (Vec<_>, Vec<_>) = items.into_iter().partition(|p| std::fs::symlink_metadata(p).is_ok());
                        let result = if present.is_empty() { Ok(()) } else { move_to_trash(&present) };
                        (present, missing, result)
                    })
                    .await;
                let (present, missing, result) = outcome;
                // A failed batch may still have moved some items: whatever left the disk is gone.
                let moved: Vec<PathBuf> = present.into_iter().filter(|p| std::fs::symlink_metadata(p).is_err()).collect();
                {
                    let mut index = this.shared.index.write();
                    for path in moved.iter().chain(&missing) {
                        if let Some(id) = index.find_path(path) {
                            index.remove(id);
                        }
                    }
                }
                this.measure();
                let text = trashed_message(&moved, missing.len(), result.err());
                cx.update(|window, cx| {
                    window.push_notification(Notification::info(text), cx);
                })
                .ok();
            })
            .detach();
    }

    fn start_empty(&self, window: &mut Window, cx: &mut App) {
        let this = self.clone();
        window
            .spawn(cx, async move |cx| {
                let result = cx.background_executor().spawn(async { empty_trash() }).await;
                this.measure();
                cx.update(|window, cx| match result {
                    Err(e) => window.push_notification(Notification::error(format!("Could not empty the Trash: {e}")), cx),
                    Ok(outcome) => {
                        let (needs_admin, other): (Vec<_>, Vec<_>) = outcome.failed.into_iter().partition(|f| f.admin_paths.is_some());
                        let mut text = match outcome.purged {
                            0 => String::new(),
                            n => format!("Permanently deleted {n} item(s) from the Trash."),
                        };
                        if let Some(first) = other.first() {
                            text.push_str(&format!(
                                " {} item(s) could not be deleted: {} ({})",
                                other.len(),
                                first.original.display(),
                                first.error
                            ));
                        }
                        if !text.is_empty() {
                            window.push_notification(Notification::info(text.trim_start().to_owned()), cx);
                        }
                        if !needs_admin.is_empty() {
                            this.ask_admin(needs_admin, window, cx);
                        }
                    }
                })
                .ok();
            })
            .detach();
    }

    fn start_admin_delete(&self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut App) {
        let this = self.clone();
        window
            .spawn(cx, async move |cx| {
                let result = cx.background_executor().spawn(async move { delete_as_admin(&paths) }).await;
                this.measure();
                cx.update(|window, cx| {
                    let note = match result {
                        Ok(()) => Notification::info("Deleted the remaining Trash items as administrator."),
                        Err(e) => Notification::warning(format!("Left in the Trash: {e}")),
                    };
                    window.push_notification(note, cx);
                })
                .ok();
            })
            .detach();
    }
}

fn trashed_message(moved: &[PathBuf], missing: usize, error: Option<String>) -> String {
    let mut text = match moved.len() {
        0 => String::new(),
        1 => format!("Moved {} to the Trash.", moved[0].display()),
        n => format!("Moved {n} items to the Trash."),
    };
    if missing > 0 {
        text.push_str(&format!(" {missing} item(s) were already gone and left the Index."));
    }
    if let Some(error) = error {
        text.push_str(&format!(" Nothing was moved: {error}"));
    }
    text.trim().to_owned()
}

/// `ids` without any Entry whose ancestor is also in `ids`: a folder takes its descendants
/// with it.
fn top_level(index: &Index, ids: &[EntryId]) -> Vec<EntryId> {
    ids.iter()
        .copied()
        .filter(|&id| {
            let mut ancestor = index.parent(id);
            while let Some(a) = ancestor {
                if ids.contains(&a) {
                    return false;
                }
                ancestor = index.parent(a);
            }
            true
        })
        .collect()
}

/// Why `id` can't go to the Trash, if it can't.
fn blocker(index: &Index, volumes: &[VolumeInfo], id: EntryId) -> Option<String> {
    let volume = index.volume(index.volume_of(id)?)?;
    if volume.is_remote() {
        return Some("Remote Locations are read-only".into());
    }
    if index.parent(id).is_none() {
        return Some("A whole Volume can't be moved to the Trash.".into());
    }
    if !volume.online {
        return Some("Offline Volume: it is not mounted, so nothing on it can go to the Trash.".into());
    }
    let info = volumes.iter().find(|v| v.mount == volume.mount)?;
    info.read_only.then(|| {
        format!(
            "{} is mounted read-only, so nothing on it can go to the Trash.",
            info.mount.display()
        )
    })
}

/// On-disk Size of an Entry and everything beneath it, regardless of the Query.
fn subtree_on_disk(index: &Index, id: EntryId) -> u64 {
    let mut total = 0;
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        total += index.on_disk(current);
        stack.extend_from_slice(index.children(current));
    }
    total
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob would bring gpui's own `test` attribute into scope.
    use super::{Index, VolumeInfo, blocker, subtree_on_disk, top_level};
    use std::path::Path;
    use ve_core::{EntryMeta, SyncState};

    #[test]
    fn nested_selection_collapses_and_read_only_volumes_block() {
        let dir = EntryMeta {
            is_dir: true,
            ..Default::default()
        };
        let file = EntryMeta {
            on_disk: 4096,
            ..Default::default()
        };
        let mut index = Index::new();
        let vol = index.add_volume(Path::new("/mnt/ro"), "ext4", "uuid:ro", dir, SyncState::Walk);
        let root = index.volume(vol).unwrap().root;
        let folder = index.insert(root, "a", dir);
        let inner = index.insert(folder, "f", file);

        assert_eq!(top_level(&index, &[folder, inner]), vec![folder]);
        assert_eq!(subtree_on_disk(&index, folder), 4096);

        let mut info = VolumeInfo {
            mount: "/mnt/ro".into(),
            identity: "uuid:ro".into(),
            fs_type: "ext4".into(),
            label: String::new(),
            removable: false,
            network: false,
            read_only: true,
        };
        assert!(blocker(&index, std::slice::from_ref(&info), inner).is_some());
        assert!(blocker(&index, std::slice::from_ref(&info), root).is_some());
        info.read_only = false;
        assert!(blocker(&index, std::slice::from_ref(&info), inner).is_none());
        // Not mounted: Trash is off whatever the mount flags were.
        index.set_mount(vol, None);
        let why = blocker(&index, &[info], inner).unwrap();
        assert!(why.starts_with("Offline Volume"), "{why}");

        // A Remote is read-only, whatever the Entry.
        let remote = index.add_remote(&ve_core::Remote::new("me@box", "/srv").unwrap());
        let rroot = index.volume(remote).unwrap().root;
        let rfile = index.insert(rroot, "f", file);
        for id in [rroot, rfile] {
            assert_eq!(blocker(&index, &[], id).as_deref(), Some("Remote Locations are read-only"));
        }
    }
}
