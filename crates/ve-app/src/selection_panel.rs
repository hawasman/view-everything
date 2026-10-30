//! The count of folders under the inspected folder, which the Inspector shows: counted off the UI
//! thread, kept per (Entry, View).

use crate::engine::Shared;
use crate::format;
use crate::view::View;
use gpui_kit::*;
use std::sync::Arc;
use ve_core::EntryId;

/// How many folders lie under a folder the Query shows.
struct FolderCount {
    id: EntryId,
    view: u64,
    folders: u64,
}

pub struct SelectionPanel {
    shared: Arc<Shared>,
    folders: Option<FolderCount>,
    /// The (Entry, View id) a count is running for.
    pending: Option<(EntryId, u64)>,
}

/// Folders shown under `dir` by `view`, at any depth. The Index is locked per folder, not for
/// the whole walk, so a writer never waits behind a large count.
fn count_folders(index: &ve_scan::SharedIndex, view: &View, dir: EntryId) -> u64 {
    let mut stack = vec![dir];
    let mut folders = 0;
    while let Some(id) = stack.pop() {
        let index = index.read();
        for child in index.children(id) {
            if index.is_dir(child) && view.filter.is_shown(child) {
                folders += 1;
                stack.push(child);
            }
        }
    }
    folders
}

impl SelectionPanel {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            folders: None,
            pending: None,
        }
    }

    /// "12 folders" for the folder `id` under `view`, or "counting folders…" while the count
    /// runs. A new View (a zoom) changes the key, so the count is asked for again.
    pub fn folders_text(&mut self, id: EntryId, view: &Arc<View>, cx: &mut Context<Self>) -> String {
        let key = (id, view.id);
        if let Some(f) = self.folders.as_ref().filter(|f| (f.id, f.view) == key) {
            return format!("{} folders", format::count(f.folders));
        }
        if self.pending != Some(key) {
            self.pending = Some(key);
            let (shared, view) = (self.shared.clone(), view.clone());
            cx.spawn(async move |this, cx| {
                let folders = cx
                    .background_executor()
                    .spawn(async move { count_folders(&shared.index, &view, id) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.pending == Some(key) {
                        this.pending = None;
                    }
                    this.folders = Some(FolderCount {
                        id: key.0,
                        view: key.1,
                        folders,
                    });
                    cx.notify();
                });
            })
            .detach();
        }
        "counting folders…".to_owned()
    }
}
