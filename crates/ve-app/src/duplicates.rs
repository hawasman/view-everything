//! The Duplicates panel: Duplicates grouped by content, largest wasted space first, with the
//! copy to keep chosen per group and the rest sent to the Trash through its confirmation.

use crate::engine::{Shared, refresh_duplicates};
use crate::format;
use crate::theme::palette;
use crate::trash::Trash;
use crate::ui::{caption, pill, task_progress};
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
use ve_scan::VolumeInfo;

/// Height of every row of the list, a group's heading or one copy: the list is virtualised, so
/// only the rows in view are built however many groups and copies there are.
const ROW_HEIGHT: f32 = 34.;

pub struct DuplicatesPanel {
    shared: Arc<Shared>,
    trash: Trash,
    volumes: Vec<VolumeInfo>,
    /// The copy to keep per group, by the group's first copy; unset means that first copy.
    keep: HashMap<EntryId, EntryId>,
    /// The groups and rows drawn, rebuilt when a search finishes or the Index changes.
    shown: Vec<Shown>,
    rows: Vec<Row>,
    /// (search version, Index generation) `shown` was built from.
    built_from: Option<(u64, u64, Option<u64>)>,
}

impl DuplicatesPanel {
    pub fn new(shared: Arc<Shared>, trash: Trash, volumes: Vec<VolumeInfo>) -> Self {
        Self {
            shared,
            trash,
            volumes,
            keep: HashMap::new(),
            shown: Vec::new(),
            rows: Vec::new(),
            built_from: None,
        }
    }

    pub fn set_volumes(&mut self, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.volumes = volumes;
        cx.notify();
    }

    /// Copies trashed since the search are already gone from the Index: drop them.
    fn rebuild(&mut self) {
        let (groups, version) = {
            let state = self.shared.duplicates.lock();
            (state.groups.clone(), state.version)
        };
        let view = self.shared.view.lock().clone();
        let index = self.shared.index.read();
        let built = (version, index.generation(), view.as_ref().map(|v| v.id));
        if self.built_from == Some(built) {
            return;
        }
        self.built_from = Some(built);
        // The panel follows the Query: a group is shown when any copy counts under it, and then
        // lists every copy.
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
        self.rows.clear();
        for (gi, group) in self.shown.iter().enumerate() {
            self.rows.push(Row::Heading(gi));
            self.rows.extend((0..group.copies.len()).map(|ri| Row::Copy(gi, ri)));
        }
    }

    fn row(&self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let (muted, rule, accent) = (p.muted, p.rule, p.accent);
        match self.rows[row] {
            Row::Heading(gi) => {
                let group = &self.shown[gi];
                let kept = self.kept(group);
                let others: Vec<EntryId> = group.copies.iter().copied().filter(|&id| id != kept).collect();
                h_flex()
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .gap(px(12.))
                    .items_center()
                    .border_t_1()
                    .border_color(rule)
                    .child(
                        div()
                            .flex_1()
                            .child(format!("{} × {}", group.copies.len(), format::size(group.size)))
                            .font_weight(FontWeight::MEDIUM),
                    )
                    .child(caption(format!("{} wasted", format::size(group.wasted())), cx))
                    .child(pill(("trash-others", gi), "Trash others").small().build(
                        cx,
                        cx.listener(move |this, _, window, cx| this.trash.ask_trash(&others, &this.volumes, window, cx)),
                    ))
                    .into_any_element()
            }
            Row::Copy(gi, ri) => {
                let group = &self.shown[gi];
                let first = group.copies[0];
                let kept = self.kept(group);
                let id = group.copies[ri];
                let path = self.shared.index.read().path(id).display().to_string();
                h_flex()
                    .id(("copy", gi << 32 | ri))
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .gap(px(8.))
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.keep.insert(first, id);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_color(if id == kept { accent } else { muted })
                            .child(if id == kept { "●" } else { "○" }),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(path))
                    .when(id == kept, |d| d.child(caption("keep", cx)))
                    .into_any_element()
            }
        }
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
        self.size * (self.copies.len() as u64 - 1)
    }
}

/// A row of the list: a group's heading, or one of its copies.
#[derive(Clone, Copy)]
enum Row {
    Heading(usize),
    Copy(usize, usize),
}

impl Render for DuplicatesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        refresh_duplicates(&self.shared);
        self.rebuild();
        let muted = palette(cx).muted;
        let hashing = self.shared.hashing_now();
        let skipped = self.shared.duplicates.lock().skipped;
        let wasted: u64 = self.shown.iter().map(Shown::wasted).sum();
        let progress = &self.shared.hash_progress;
        let status = if hashing {
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
                task_progress("dupes-progress", done, Some(total), what, cx).into_any_element()
            }
        } else {
            let mut line = format!(
                "{} groups · {} wasted",
                format::count(self.shown.len() as u64),
                format::size(wasted)
            );
            if skipped > 0 {
                line += &format!(
                    " · {} file{} skipped by Duplicate Exclusions",
                    format::count(skipped),
                    if skipped == 1 { "" } else { "s" }
                );
            }
            line.into_any_element()
        };
        v_flex()
            .id("duplicates")
            .size_full()
            .px(px(8.))
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(12.))
                    .items_center()
                    .pb(px(8.))
                    .child(div().flex_1().text_color(muted).child(status))
                    .child(
                        pill("dupes-rescan", "Rescan")
                            .small()
                            .busy(hashing)
                            .disabled(hashing.then_some("Hashing is running"))
                            .build(
                                cx,
                                cx.listener(|this, _, _, cx| {
                                    crate::engine::start_hashing(&this.shared);
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(if self.shown.is_empty() && !hashing {
                div()
                    .text_color(muted)
                    .child("No Duplicates among local files of 1 MB or more.")
                    .into_any_element()
            } else {
                uniform_list(
                    "duplicates-list",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| range.map(|row| this.row(row, cx)).collect::<Vec<_>>()),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            })
    }
}
