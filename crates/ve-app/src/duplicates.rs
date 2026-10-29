//! The Duplicates panel: Duplicates grouped by content, largest wasted space first, with the
//! copy to keep chosen per group and the rest sent to the Trash through its confirmation.

use crate::engine::{Shared, refresh_duplicates};
use crate::format;
use crate::theme::palette;
use crate::trash::Trash;
use crate::ui::{caption, pill};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use ve_core::EntryId;
use ve_scan::VolumeInfo;

/// Groups drawn at once; the rest are a summary line.
const MAX_GROUPS: usize = 500;

pub struct DuplicatesPanel {
    shared: Arc<Shared>,
    trash: Trash,
    volumes: Vec<VolumeInfo>,
    /// The copy to keep per group, by the group's first copy; unset means that first copy.
    keep: HashMap<EntryId, EntryId>,
}

impl DuplicatesPanel {
    pub fn new(shared: Arc<Shared>, trash: Trash, volumes: Vec<VolumeInfo>) -> Self {
        Self {
            shared,
            trash,
            volumes,
            keep: HashMap::new(),
        }
    }

    pub fn set_volumes(&mut self, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.volumes = volumes;
        cx.notify();
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

impl Render for DuplicatesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        refresh_duplicates(&self.shared);
        let p = palette(cx);
        let (muted, rule, accent) = (p.muted, p.rule, p.accent);
        let groups = self.shared.duplicates.lock().groups.clone();
        let hashing = self.shared.hashing_now();
        // Copies trashed since the search are already gone from the Index: drop them now.
        let shown: Vec<Shown> = {
            let index = self.shared.index.read();
            groups
                .iter()
                .map(|g| Shown {
                    size: g.size,
                    copies: g.files.iter().copied().filter(|&id| index.is_live(id)).collect(),
                })
                .filter(|g| g.copies.len() > 1)
                .collect()
        };
        let mut shown = shown;
        shown.sort_by_key(|g| std::cmp::Reverse(g.wasted()));
        let wasted: u64 = shown.iter().map(Shown::wasted).sum();
        let progress = &self.shared.hash_progress;
        let status = if hashing {
            let what = if progress.stage.load(Ordering::Relaxed) == 2 {
                "Hashing whole files"
            } else {
                "Comparing file starts and ends"
            };
            format!(
                "{what} — {} of {}",
                progress.done.load(Ordering::Relaxed),
                progress.total.load(Ordering::Relaxed)
            )
        } else {
            format!("{} groups · {} wasted", format::count(shown.len() as u64), format::size(wasted))
        };
        let cards: Vec<_> = shown
            .iter()
            .take(MAX_GROUPS)
            .enumerate()
            .map(|(gi, group)| {
                let first = group.copies[0];
                let kept = self.keep.get(&first).copied().filter(|k| group.copies.contains(k)).unwrap_or(first);
                let rows = group.copies.iter().enumerate().map(|(ri, &id)| {
                    let path = self.shared.index.read().path(id).display().to_string();
                    h_flex()
                        .id(("copy", gi * 10_000 + ri))
                        .gap(px(8.))
                        .py(px(3.))
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
                });
                let others: Vec<EntryId> = group.copies.iter().copied().filter(|&id| id != kept).collect();
                v_flex()
                    .flex_none()
                    .gap(px(4.))
                    .py(px(10.))
                    .border_b_1()
                    .border_color(rule)
                    .child(
                        h_flex()
                            .gap(px(12.))
                            .items_center()
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
                            )),
                    )
                    .children(rows)
            })
            .collect();
        v_flex()
            .id("duplicates")
            .size_full()
            .overflow_y_scroll()
            .px(px(8.))
            .child(
                h_flex()
                    .gap(px(12.))
                    .items_center()
                    .pb(px(8.))
                    .child(div().flex_1().text_color(muted).child(status))
                    .child(
                        pill("dupes-rescan", "Rescan")
                            .small()
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
            .children(cards)
            .when(shown.len() > MAX_GROUPS, |d| {
                d.child(caption(format!("…and {} more groups", shown.len() - MAX_GROUPS), cx))
            })
            .when(shown.is_empty() && !hashing, |d| {
                d.child(div().text_color(muted).child("No Duplicates among local files of 1 MB or more."))
            })
    }
}
