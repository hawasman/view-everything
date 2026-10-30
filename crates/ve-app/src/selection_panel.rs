//! The Selection panel under the Treemap: what is selected, its sizes, and what can be done to it.

use crate::engine::Shared;
use crate::format;
use crate::theme::{RADIUS_CARD, palette};
use crate::ui::{caption, pill};
use crate::view::{Focus, View};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use ve_core::EntryId;

/// What a button in the panel asks of the window.
pub enum SelectionEvent {
    /// Show in Explorer: zoom the Query into this folder (a file's parent folder).
    Scope(EntryId),
    /// Move to the Trash, after asking.
    Trash(EntryId),
    /// Sync this folder alone (a file's parent).
    Sync(EntryId),
}

impl EventEmitter<SelectionEvent> for SelectionPanel {}

/// How many folders lie under a folder the Query shows; counted off the UI thread.
struct FolderCount {
    id: EntryId,
    view: u64,
    folders: u64,
}

pub struct SelectionPanel {
    shared: Arc<Shared>,
    selected: Option<EntryId>,
    open: bool,
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
        for &child in index.children(id) {
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
            selected: None,
            open: true,
            folders: None,
            pending: None,
        }
    }

    /// Shows `id`, selected in the Treemap or a list; None shows the empty panel.
    pub fn set_selected(&mut self, id: Option<EntryId>, cx: &mut Context<Self>) {
        if self.selected == id {
            return;
        }
        self.selected = id;
        self.folders = None;
        cx.notify();
    }

    /// Starts counting the folders under `id` for `view` unless that count is done or running.
    /// A new View (a zoom) changes the key, so the count is asked for again.
    fn ensure_count(&mut self, id: EntryId, view: &Arc<View>, cx: &mut Context<Self>) {
        let key = (id, view.id);
        let done = self.folders.as_ref().is_some_and(|f| (f.id, f.view) == key);
        if done || self.pending == Some(key) {
            return;
        }
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

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        let path = {
            let index = self.shared.index.read();
            if !index.is_live(id) {
                return;
            }
            index.path(id)
        };
        if let Err(e) = opener::open(&path) {
            window.push_notification(Notification::error(format!("Could not open {}: {e}", path.display())), cx);
        }
    }

    fn scope_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else { return };
        let folder = {
            let index = self.shared.index.read();
            if !index.is_live(id) {
                return;
            }
            if index.is_dir(id) { Some(id) } else { index.parent(id) }
        };
        if let Some(folder) = folder {
            cx.emit(SelectionEvent::Scope(folder));
        }
    }

    /// One `label  value` pair of the details grid.
    fn field(label: &'static str, value: String, cx: &App) -> Div {
        let p = palette(cx);
        v_flex()
            .flex_1()
            .flex_basis(relative(0.))
            .min_w_0()
            .gap(px(1.))
            .child(div().text_size(px(11.)).text_color(p.muted).whitespace_nowrap().child(label))
            .child(div().text_xs().text_color(p.text).child(value))
    }
}

impl Render for SelectionPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let shared = self.shared.clone();
        let index = shared.index.read();
        let view = shared.view.lock().clone();
        let id = self.selected.filter(|&id| index.is_live(id));

        let title = |text: String| {
            h_flex()
                .gap(px(8.))
                .items_baseline()
                .min_w_0()
                .child(
                    div()
                        .flex_none()
                        .text_size(px(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .child("Selection"),
                )
                .child(div().min_w_0().truncate().text_xs().text_color(p.muted).child(text))
        };
        let toggle = |open: bool| {
            pill("selection-toggle", if open { "Collapse" } else { "Expand" }).small().build(
                cx,
                cx.listener(|this, _, _, cx| {
                    this.open = !this.open;
                    cx.notify();
                }),
            )
        };
        let card = div().flex_none().bg(p.surface).rounded(px(RADIUS_CARD)).px(px(20.)).py(px(14.));

        let (Some(id), Some(view)) = (id, view) else {
            return card.child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(title("Click a tile or a row to see its details here.".into()))
                    .child(toggle(self.open)),
            );
        };

        let is_dir = index.is_dir(id);
        let totals = view.filter.totals(&index, id);
        // Share of what the Treemap draws; a selection outside it is measured against its Location.
        let drawn = crate::treemap::zoom_root(&index, &view);
        let inside = drawn
            .folder()
            .is_none_or(|root| std::iter::successors(Some(id), |&e| index.parent(e)).any(|e| e == root));
        let base = if inside { drawn } else { Focus::Folder(index.root_of(id)) };
        let whole = base.totals(&index, &view.filter).on_disk.max(1);
        let name = index.display_name(id).to_owned();
        let head = h_flex()
            .justify_between()
            .items_center()
            .gap(px(12.))
            .child(title(name))
            .child(toggle(self.open));
        if !self.open {
            return card.child(head);
        }

        let share = format!("{:.1}% of {}", totals.on_disk as f64 * 100.0 / whole as f64, base.name(&index));
        let counts = if is_dir {
            self.ensure_count(id, &view, cx);
            let folders = self
                .folders
                .as_ref()
                .filter(|f| f.id == id && f.view == view.id)
                .map(|f| format!("{} folders", format::count(f.folders)))
                .unwrap_or_else(|| "counting folders…".to_owned());
            format!("{} files · {}", format::count(totals.files), folders)
        } else {
            "1 file".to_owned()
        };
        let when = format::time(index.modified(id));
        let path = index.path(id).display().to_string();
        let buttons = h_flex()
            .flex_wrap()
            .gap(px(8.))
            .child(
                pill("selection-open", "Open")
                    .small()
                    .build(cx, cx.listener(|this, _, window, cx| this.open_selected(window, cx))),
            )
            .child(
                pill("selection-scope", "Show in Explorer")
                    .small()
                    .build(cx, cx.listener(|this, _, _, cx| this.scope_selected(cx))),
            )
            .child(
                pill("selection-trash", "Trash this")
                    .small()
                    .build(cx, cx.listener(move |_, _, _, cx| cx.emit(SelectionEvent::Trash(id)))),
            )
            .child({
                let target = crate::engine::folder_of(&index, id);
                let why = (!crate::engine::can_sync_folder(&index, target)).then_some(crate::engine::WHOLE_ONLY);
                pill("selection-sync", "Sync this folder")
                    .small()
                    .busy(shared.folder_syncing(target))
                    .disabled(why)
                    .build(cx, cx.listener(move |_, _, _, cx| cx.emit(SelectionEvent::Sync(target))))
            });
        card.child(
            v_flex().gap(px(10.)).child(head).child(crate::motion::slide_in(
                v_flex()
                    .gap(px(10.))
                    .child(div().text_xs().text_color(p.muted).whitespace_nowrap().truncate().child(path))
                    .child(
                        v_flex()
                            .gap(px(8.))
                            .child(
                                h_flex()
                                    .gap(px(16.))
                                    .child(Self::field("On-disk Size", format::size(totals.on_disk), cx))
                                    .child(Self::field("Share", share, cx)),
                            )
                            .child(h_flex().gap(px(16.)).child(Self::field("Contains", counts, cx)).child(Self::field(
                                "Last write",
                                if when.is_empty() { "unknown".into() } else { when },
                                cx,
                            ))),
                    )
                    .child(buttons)
                    .when(!is_dir, |d| {
                        d.child(caption("Show in Explorer zooms the Query into the containing folder.", cx))
                    }),
                "selection-slide",
            )),
        )
    }
}
