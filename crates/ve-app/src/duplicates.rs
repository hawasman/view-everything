//! The Duplicates list: Duplicates grouped by content, largest wasted space first, one row per
//! group. A click on a group opens its copies in the Inspector, where the copy to keep is chosen
//! and the rest go to the Trash through its confirmation.

use crate::engine::{Shared, refresh_duplicates};
use crate::format;
use crate::theme::{FS_SMALL, palette};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Sizable as _, Size};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use ve_core::EntryId;

/// Height of every group row: the list is virtualised, so only the rows in view are built
/// however many groups there are.
const ROW_HEIGHT: f32 = 52.;

/// A click on a group: inspect it, by its first copy.
pub struct InspectGroup(pub EntryId);

impl EventEmitter<InspectGroup> for DuplicatesPanel {}

/// One group as the Inspector draws it.
pub struct GroupView {
    /// Size of each copy.
    pub size: u64,
    pub copies: Vec<EntryId>,
    pub kept: EntryId,
}

/// Space taken by every copy but one.
fn wasted(size: u64, copies: usize) -> u64 {
    size * (copies as u64).saturating_sub(1)
}

impl GroupView {
    pub fn wasted(&self) -> u64 {
        wasted(self.size, self.copies.len())
    }

    /// Every copy but the one kept.
    pub fn others(&self) -> Vec<EntryId> {
        self.copies.iter().copied().filter(|&id| id != self.kept).collect()
    }
}

pub struct DuplicatesPanel {
    shared: Arc<Shared>,
    /// The copy to keep per group, by the group's first copy; unset means that first copy.
    keep: HashMap<EntryId, EntryId>,
    /// The groups drawn, rebuilt when a search finishes or the Index changes.
    shown: Vec<Shown>,
    /// (search version, Index generation, View) `shown` was built from.
    built_from: Option<(u64, u64, Option<u64>)>,
    /// The group the Inspector shows, by its first copy; its row is highlighted.
    pub selected: Option<EntryId>,
    scroll: UniformListScrollHandle,
}

impl DuplicatesPanel {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            keep: HashMap::new(),
            shown: Vec::new(),
            built_from: None,
            selected: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Copies trashed since the search are already gone from the Index: drop them. The groups
    /// are copied out only when the search, the Index or the View moved on.
    fn rebuild(&mut self) {
        let version = self.shared.duplicates.lock().version;
        let view = self.shared.view.lock().clone();
        let generation = self.shared.index.read().generation();
        let built = (version, generation, view.as_ref().map(|v| v.id));
        if self.built_from == Some(built) {
            return;
        }
        self.built_from = Some(built);
        let groups = self.shared.duplicates.lock().groups.clone();
        let index = self.shared.index.read();
        // The list follows the Query: a group is shown when any copy counts under it, and then
        // holds every copy.
        let follows = |ids: &[EntryId]| view.as_ref().is_none_or(|v| ids.iter().any(|&id| v.filter.is_counted(id)));
        self.shown = groups
            .iter()
            .map(|g| Shown {
                size: g.size,
                copies: g.files.iter().copied().filter(|&id| index.is_live(id)).collect(),
            })
            .filter(|g| g.copies.len() > 1 && follows(&g.copies))
            .collect();
        self.shown.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
    }

    /// The group whose first copy is `first`, if it is still shown.
    pub fn group(&mut self, first: EntryId) -> Option<GroupView> {
        self.rebuild();
        let group = self.shown.iter().find(|g| g.copies[0] == first)?;
        Some(GroupView {
            size: group.size,
            copies: group.copies.clone(),
            kept: self.kept(group),
        })
    }

    /// Keeps `id` of the group whose first copy is `first`.
    pub fn set_keep(&mut self, first: EntryId, id: EntryId) {
        self.keep.insert(first, id);
    }

    fn row(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let group = &self.shown[row];
        let first = group.copies[0];
        let name = self.shared.index.read().display_name(first).to_owned();
        let selected = self.selected == Some(first);
        h_flex()
            .w_full()
            .id(("group", row))
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px(px(8.))
            .gap(px(12.))
            .items_center()
            .border_b_1()
            .border_color(p.rule)
            .cursor_pointer()
            .when(selected, |d| d.bg(p.active))
            .when(!selected, |d| d.hover(move |s| s.bg(p.raised)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = Some(first);
                cx.emit(InspectGroup(first));
                cx.notify();
            }))
            .child(v_flex().flex_1().min_w_0().child(div().truncate().child(name)).child(
                div().text_size(px(FS_SMALL)).text_color(p.muted).truncate().child(format!(
                    "{} copies × {}",
                    group.copies.len(),
                    format::size(group.size)
                )),
            ))
            .child(
                v_flex()
                    .flex_none()
                    .items_end()
                    .child(format::size(group.wasted()))
                    .child(div().text_size(px(FS_SMALL)).text_color(p.muted).child("wasted")),
            )
            .into_any_element()
    }

    /// The copy to keep in `group`.
    fn kept(&self, group: &Shown) -> EntryId {
        let first = group.copies[0];
        self.keep.get(&first).copied().filter(|k| group.copies.contains(k)).unwrap_or(first)
    }
}

/// A group as drawn: the copies still in the Index, if at least two.
struct Shown {
    size: u64,
    copies: Vec<EntryId>,
}

impl Shown {
    fn wasted(&self) -> u64 {
        wasted(self.size, self.copies.len())
    }
}

impl DuplicatesPanel {
    /// The page's facts line ("12 groups · 3 GB wasted"), and whether hashing runs. Brings the
    /// groups up to date first.
    pub fn facts(&mut self) -> (String, bool) {
        refresh_duplicates(&self.shared);
        self.rebuild();
        if self.shared.hashing_now() {
            return ("Looking for Duplicates…".to_owned(), true);
        }
        let skipped = self.shared.duplicates.lock().skipped;
        let wasted: u64 = self.shown.iter().map(Shown::wasted).sum();
        let copies: usize = self.shown.iter().map(|g| g.copies.len()).sum();
        let mut line = format!(
            "{} groups · {} copies · {} wasted",
            format::count(self.shown.len() as u64),
            format::count(copies as u64),
            format::size(wasted)
        );
        if skipped > 0 {
            line += &format!(
                " · {} file{} skipped by Duplicate Exclusions",
                format::count(skipped),
                if skipped == 1 { "" } else { "s" }
            );
        }
        (line, false)
    }

    /// Starts the search again.
    pub fn rescan(&self) {
        crate::engine::start_hashing(&self.shared);
    }
}

impl Render for DuplicatesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("DuplicatesPanel::render");
        refresh_duplicates(&self.shared);
        self.rebuild();
        let muted = palette(cx).muted;
        let hashing = self.shared.hashing_now();
        let progress = &self.shared.hash_progress;
        let status = hashing.then(|| {
            let (done, total) = (progress.done.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
            if total == 0 {
                h_flex()
                    .gap(px(10.))
                    .items_center()
                    .child(Spinner::new().with_size(Size::XSmall).color(muted))
                    .child("Grouping files by size…")
                    .into_any_element()
            } else {
                let what = if progress.stage.load(Ordering::Relaxed) == 2 {
                    "files hashed whole"
                } else {
                    "file starts and ends compared"
                };
                crate::ui::task_progress("dupes-progress", done, Some(total), what, cx).into_any_element()
            }
        });
        v_flex()
            .id("duplicates")
            .size_full()
            .children(status.map(|s| div().flex_none().pb(px(8.)).text_color(muted).child(s)))
            .child(if self.shown.is_empty() {
                div()
                    .text_color(muted)
                    .child(if hashing {
                        "No Duplicates found yet."
                    } else {
                        "No Duplicates among local files of 1 MB or more."
                    })
                    .into_any_element()
            } else {
                crate::ui::scrolling_list(
                    uniform_list(
                        "duplicates-list",
                        self.shown.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| range.map(|row| this.row(row, cx)).collect::<Vec<_>>()),
                    ),
                    &self.scroll,
                )
                .into_any_element()
            })
    }
}
