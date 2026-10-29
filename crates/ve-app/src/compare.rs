//! "Compare with": the Explorer's choice of a Snapshot date, and the comparison of the Index
//! with it, computed off the UI thread and kept current while the Index changes.

use crate::engine::{Shared, snapshots_dir};
use crate::theme::{SERIF_FAMILY, palette};
use crate::ui::pill;
use chrono::{Local, NaiveDate};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ve_core::snapshot::{self, Comparison, Snapshot};

/// The least time between two recomputations while the Index keeps changing.
const REFRESH: Duration = Duration::from_secs(5);

pub enum CompareEvent {
    /// The comparison the tree shows, or none once comparing stopped.
    Result(Option<Arc<Comparison>>),
    Failed(String),
}

pub struct Compare {
    shared: Arc<Shared>,
    active: Option<NaiveDate>,
    /// The chosen date's Snapshot, read once.
    snapshot: Option<Arc<Snapshot>>,
    result: Option<Arc<Comparison>>,
    working: bool,
    last: Instant,
}

impl EventEmitter<CompareEvent> for Compare {}

impl Compare {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            active: None,
            snapshot: None,
            result: None,
            working: false,
            last: Instant::now(),
        }
    }

    pub fn active(&self) -> Option<NaiveDate> {
        self.active
    }

    /// Compares with the Snapshot of `date`, or stops comparing.
    pub fn choose(&mut self, date: Option<NaiveDate>, cx: &mut Context<Self>) {
        self.active = date;
        self.snapshot = None;
        self.result = None;
        if date.is_none() {
            cx.emit(CompareEvent::Result(None));
        }
        self.start(cx);
        cx.notify();
    }

    /// Recomputes when the Index has changed since the comparison was made.
    pub fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .result
            .as_ref()
            .is_some_and(|r| r.generation() != self.shared.index.read().generation());
        if stale && self.last.elapsed() >= REFRESH {
            self.start(cx);
        }
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(date) = self.active else { return };
        if self.working {
            return;
        }
        self.working = true;
        self.last = Instant::now();
        let (shared, known) = (self.shared.clone(), self.snapshot.clone());
        cx.spawn(async move |this, cx| {
            let work = cx.background_spawn(async move {
                let snap = match known {
                    Some(snap) => snap,
                    None => {
                        let dir = snapshots_dir().ok_or("no data folder")?;
                        let path = snapshot::path_for(&dir, date);
                        Arc::new(Snapshot::load(&path).map_err(|e| format!("Could not read the Snapshot of {date}: {e}"))?)
                    }
                };
                let comparison = snap.compare_with(|| shared.index.read());
                Ok::<_, String>((snap, Arc::new(comparison)))
            });
            let done = work.await;
            this.update(cx, |this, cx| {
                this.working = false;
                // The choice changed while this was running: its answer is for something else.
                if this.active != Some(date) {
                    this.start(cx);
                    return;
                }
                match done {
                    Ok((snap, comparison)) => {
                        this.snapshot = Some(snap);
                        this.result = Some(comparison.clone());
                        cx.emit(CompareEvent::Result(Some(comparison)));
                    }
                    Err(why) => {
                        this.active = None;
                        cx.emit(CompareEvent::Failed(why));
                        cx.emit(CompareEvent::Result(None));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The dialog listing the dates that have a Snapshot.
    pub fn open_dialog(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let this = this.clone();
        let dates = snapshots_dir().map(|d| snapshot::list(&d)).unwrap_or_default();
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = palette(cx);
            let (muted, border) = (p.muted, p.border);
            let active = this.read(cx).active;
            let today = Local::now().date_naive();
            let rows = dates.iter().enumerate().map(|(i, &date)| {
                let pick = {
                    let this = this.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        this.update(cx, |c, cx| c.choose(Some(date), cx));
                        window.close_dialog(cx);
                    }
                };
                let age = match (today - date).num_days() {
                    0 => "today".to_owned(),
                    1 => "yesterday".to_owned(),
                    n => format!("{n} days ago"),
                };
                h_flex()
                    .id(("snapshot-date", i))
                    .gap(px(8.))
                    .items_center()
                    .py(px(8.))
                    .border_b_1()
                    .border_color(border)
                    .cursor_pointer()
                    .on_click(pick)
                    .child(
                        div()
                            .flex_1()
                            .font_weight(if active == Some(date) {
                                FontWeight::MEDIUM
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(date.to_string()),
                    )
                    .child(div().text_xs().text_color(muted).child(age))
            });
            let list = if dates.is_empty() {
                div()
                    .py(px(8.))
                    .text_color(muted)
                    .child("No Snapshots yet. The first Index save of each day stores one.")
                    .into_any_element()
            } else {
                v_flex()
                    .id("snapshot-list")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .children(rows)
                    .into_any_element()
            };
            let stop = this.clone();
            dialog
                .w(px(420.))
                .centered()
                .p(px(24.))
                .close_button(false)
                .title(
                    div()
                        .font_family(SERIF_FAMILY)
                        .text_size(px(28.))
                        .line_height(px(32.))
                        .font_weight(FontWeight::NORMAL)
                        .child("Compare with"),
                )
                .child(
                    v_flex().gap(px(12.)).child(list).child(
                        div()
                            .text_xs()
                            .child("Adds a Change column: how each folder's Folder Size differs from that day's."),
                    ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap(px(8.))
                        .when(active.is_some(), |d| {
                            d.child(pill("snapshot-stop", "Stop comparing").build(cx, move |_, window, cx| {
                                stop.update(cx, |c, cx| c.choose(None, cx));
                                window.close_dialog(cx);
                            }))
                        })
                        .child(pill("snapshot-close", "Close").build(cx, |_, window, cx| window.close_dialog(cx))),
                )
        });
    }
}
