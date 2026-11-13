//! Trash (CONTEXT.md): moving Entries to the operating system's trash, as a Task the user
//! started. Whatever leaves the disk leaves the Index too. Every client trashes through here:
//! the window's Move to Trash, and an applied Proposal (`ve proposals --apply`).

use crate::tasks::{Outcome, TaskKind};
use crate::{Engine, Shared};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Moves one path to the trash, or says why it could not.
pub type Mover = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// What a Trash did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trashed {
    /// Moved to the trash.
    pub moved: Vec<PathBuf>,
    /// Already gone from the disk: they only left the Index.
    pub missing: Vec<PathBuf>,
    /// Still on the disk, and why.
    pub failed: Vec<(PathBuf, String)>,
}

impl Trashed {
    /// Why some items could not be moved, in one line, if so.
    pub fn error(&self) -> Option<String> {
        match self.failed.as_slice() {
            [] => None,
            [(_, only)] => Some(only.clone()),
            [(_, first), ..] => Some(format!("{} items could not be moved; the first: {first}", self.failed.len())),
        }
    }
}

/// The operating system's trash.
fn system_trash(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

impl Shared {
    /// Moves `items` to the trash one by one, as a Task the user started ("Move N item(s) to
    /// the Trash"), so progress is real and one failure leaves the rest moved; then drops what
    /// left the disk from the Index. Blocks until done: call it off the UI thread.
    pub fn move_to_trash(&self, items: Vec<PathBuf>) -> Trashed {
        let total = items.len() as u64;
        let task = self
            .tasks
            .begin(TaskKind::Trash, format!("Move {total} item(s) to the Trash"), false);
        task.set_progress(0, Some(total));
        let mover = self.trash_mover.lock().clone();
        // Check the disk first: anything already gone just leaves the Index.
        let (present, missing): (Vec<_>, Vec<_>) = items.into_iter().partition(|p| std::fs::symlink_metadata(p).is_ok());
        let mut trashed = Trashed {
            missing,
            ..Trashed::default()
        };
        task.set_progress(trashed.missing.len() as u64, Some(total));
        for path in present {
            let result = match &mover {
                Some(mover) => mover(&path),
                None => system_trash(&path),
            };
            match result {
                Ok(()) => trashed.moved.push(path),
                // A failure may still have moved it: whatever left the disk is gone.
                Err(_) if std::fs::symlink_metadata(&path).is_err() => trashed.moved.push(path),
                Err(e) => trashed.failed.push((path, e)),
            }
            task.set_progress((trashed.missing.len() + trashed.moved.len() + trashed.failed.len()) as u64, Some(total));
        }
        {
            let mut index = self.index.write();
            for path in trashed.moved.iter().chain(&trashed.missing) {
                if let Some(id) = index.find_path(path) {
                    index.remove(id);
                }
            }
        }
        task.finish(match trashed.error() {
            None => Outcome::Done(format!("Moved {} item(s)", crate::format::count(trashed.moved.len() as u64))),
            Some(e) => Outcome::Failed(e),
        });
        (self.repaint)();
        trashed
    }
}

impl Engine {
    /// Has the Trash move items with `mover` instead of the operating system's trash: for a
    /// host whose trash is its own (and tests, which keep the user's trash out of it).
    pub fn set_trash_mover(&self, mover: Mover) {
        *self.shared.trash_mover.lock() = Some(mover);
    }

    /// Moves `items` to the trash as a Task the user started; see [`Shared::move_to_trash`].
    pub fn move_to_trash(&self, items: Vec<PathBuf>) -> Trashed {
        self.shared.move_to_trash(items)
    }
}
