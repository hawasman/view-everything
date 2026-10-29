//! "Compare with": the Explorer's choice of a Snapshot date, and the comparison of the Index
//! with it, computed off the UI thread and kept current while the Index changes.

use crate::engine::{Shared, snapshots_dir};
use crate::tasks::{Outcome, TaskHandle, TaskKind};
use crate::theme::{SERIF_FAMILY, palette};
use crate::ui::pill;
use chrono::{Local, NaiveDate};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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

    /// A comparison the user asked for is running as a Task (quiet recomputes as the Index
    /// changes do not count, or the control would spin every few seconds).
    pub fn working(&self) -> bool {
        user_compare_running(&self.shared.tasks)
    }

    /// Compares with the Snapshot of `date`, or stops comparing.
    pub fn choose(&mut self, date: Option<NaiveDate>, cx: &mut Context<Self>) {
        self.active = date;
        self.snapshot = None;
        self.result = None;
        if date.is_none() {
            cx.emit(CompareEvent::Result(None));
        }
        self.start(true, cx);
        cx.notify();
    }

    /// Recomputes when the Index has changed since the comparison was made.
    pub fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .result
            .as_ref()
            .is_some_and(|r| r.generation() != self.shared.index.read().generation());
        if stale && self.last.elapsed() >= REFRESH {
            self.start(false, cx);
        }
    }

    /// `user`: the comparison answers a choice the user made, so it is a Task they can see and
    /// cancel; recomputing as the Index changes is quiet.
    fn start(&mut self, user: bool, cx: &mut Context<Self>) {
        let Some(date) = self.active else { return };
        if self.working {
            return;
        }
        self.working = true;
        self.last = Instant::now();
        let (shared, known) = (self.shared.clone(), self.snapshot.clone());
        let task = user.then(|| shared.tasks.begin(TaskKind::Compare, format!("Compare with {date}"), true));
        let cancel = task.as_ref().map(TaskHandle::cancel_flag);
        cx.spawn(async move |this, cx| {
            let stopped = cancel.clone();
            let work = cx.background_spawn(async move {
                let stop = || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
                let snap = match known {
                    Some(snap) => snap,
                    None => {
                        let dir = snapshots_dir().ok_or("no data folder")?;
                        let path = snapshot::path_for(&dir, date);
                        Arc::new(Snapshot::load(&path).map_err(|e| format!("Could not read the Snapshot of {date}: {e}"))?)
                    }
                };
                if stop() {
                    return Err("cancelled".to_owned());
                }
                let comparison = snap
                    .compare_with(|| shared.index.read(), stop)
                    .ok_or_else(|| "cancelled".to_owned())?;
                Ok::<_, String>((snap, Arc::new(comparison)))
            });
            let done = work.await;
            let cancelled = stopped.is_some_and(|c| c.load(Ordering::Relaxed));
            this.update(cx, |this, cx| {
                this.working = false;
                // The choice changed while this was running: its answer is for something else.
                if this.active != Some(date) {
                    if let Some(task) = task {
                        task.finish(Outcome::Cancelled);
                    }
                    this.start(true, cx);
                    return;
                }
                if cancelled {
                    if let Some(task) = task {
                        task.finish(Outcome::Cancelled);
                    }
                    this.active = None;
                    this.result = None;
                    cx.emit(CompareEvent::Result(None));
                    cx.notify();
                    return;
                }
                match done {
                    Ok((snap, comparison)) => {
                        if let Some(task) = task {
                            task.finish(Outcome::Done(format!("Compared with {date}")));
                        }
                        this.snapshot = Some(snap);
                        this.result = Some(comparison.clone());
                        cx.emit(CompareEvent::Result(Some(comparison)));
                    }
                    Err(why) => {
                        if let Some(task) = task {
                            let again = cx.weak_entity();
                            task.set_retry(Arc::new(move |cx| {
                                again.update(cx, |c, cx| c.choose(Some(date), cx)).ok();
                            }));
                            task.finish(Outcome::Failed(why.clone()));
                        }
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

/// Whether a comparison the user asked for (a Task, not a quiet recompute) is running.
fn user_compare_running(tasks: &crate::tasks::Tasks) -> bool {
    tasks.is_running(TaskKind::Compare)
}

#[cfg(test)]
mod tests {
    use crate::tasks::{Outcome, TaskKind, Tasks};

    #[test]
    fn only_a_compare_task_makes_the_control_busy() {
        let tasks = Tasks::new();
        assert!(!super::user_compare_running(&tasks));
        let other = tasks.begin(TaskKind::Trash, "x".into(), false);
        assert!(!super::user_compare_running(&tasks));
        let task = tasks.begin(TaskKind::Compare, "Compare with 2026-01-01".into(), true);
        assert!(super::user_compare_running(&tasks));
        task.finish(Outcome::Cancelled);
        other.finish(Outcome::Cancelled);
        assert!(!super::user_compare_running(&tasks));
    }
}
