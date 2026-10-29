//! Task registry: background work (Sync, Duplicate search, Cleaner search, Clean, Trash,
//! Empty Trash, Compare) that the sidebar Tasks indicator lists. Cheap to clone; usable
//! from any thread.

use crate::theme::{RADIUS_CARD, palette};
use crate::ui::{caption, pill, task_progress};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Icon, IconName, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEEP_FINISHED: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Sync,
    CatchUp,
    DuplicateSearch,
    CleanerSearch,
    Clean,
    Trash,
    EmptyTrash,
    Compare,
}

impl TaskKind {
    pub fn name(self) -> &'static str {
        match self {
            TaskKind::Sync => "Sync",
            TaskKind::CatchUp => "Catch-up",
            TaskKind::DuplicateSearch => "Duplicate search",
            TaskKind::CleanerSearch => "Cleaner search",
            TaskKind::Clean => "Clean",
            TaskKind::Trash => "Trash",
            TaskKind::EmptyTrash => "Empty Trash",
            TaskKind::Compare => "Compare",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done(String),
    Cancelled,
    Failed(String),
}

/// Re-runs a failed Task. Dispatched by the Shell (gets the whole app context).
pub type RetryAction = Arc<dyn Fn(&mut gpui_kit::App) + Send + Sync>;

#[derive(Clone)]
pub struct TaskInfo {
    pub id: u64,
    pub kind: TaskKind,
    pub label: String,
    pub cancellable: bool,
    pub cancelling: bool,
    pub done: u64,
    pub total: Option<u64>,
    pub elapsed: Duration,
    pub outcome: Option<Outcome>,
    pub retry: Option<RetryAction>,
}

struct Entry {
    info: TaskInfo,
    started: Instant,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    running: Vec<Entry>,
    finished: Vec<TaskInfo>, // newest first
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Default)]
pub struct Tasks(Arc<Mutex<Inner>>);

impl Tasks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Closure called after every change, from whichever thread changed it.
    pub fn set_repaint(&self, f: impl Fn() + Send + Sync + 'static) {
        self.0.lock().unwrap().repaint = Some(Arc::new(f));
    }

    fn notify(&self) {
        let f = self.0.lock().unwrap().repaint.clone();
        if let Some(f) = f {
            f();
        }
    }

    pub fn begin(&self, kind: TaskKind, label: String, cancellable: bool) -> TaskHandle {
        let cancel = Arc::new(AtomicBool::new(false));
        let id = {
            let mut g = self.0.lock().unwrap();
            g.next_id += 1;
            let id = g.next_id;
            g.running.push(Entry {
                info: TaskInfo {
                    id,
                    kind,
                    label,
                    cancellable,
                    cancelling: false,
                    done: 0,
                    total: None,
                    elapsed: Duration::ZERO,
                    outcome: None,
                    retry: None,
                },
                started: Instant::now(),
                cancel: cancel.clone(),
            });
            id
        };
        self.notify();
        TaskHandle {
            tasks: self.clone(),
            id,
            cancel,
            finished: false,
        }
    }

    pub fn running(&self) -> Vec<TaskInfo> {
        self.0
            .lock()
            .unwrap()
            .running
            .iter()
            .map(|e| {
                let mut i = e.info.clone();
                i.elapsed = e.started.elapsed();
                i
            })
            .collect()
    }

    /// Newest first, at most the last 10.
    pub fn finished(&self) -> Vec<TaskInfo> {
        self.0.lock().unwrap().finished.clone()
    }

    pub fn is_running(&self, kind: TaskKind) -> bool {
        self.0.lock().unwrap().running.iter().any(|e| e.info.kind == kind)
    }

    pub fn running_count(&self) -> usize {
        self.0.lock().unwrap().running.len()
    }

    /// Raises the cancel flag of a running cancellable Task.
    pub fn cancel(&self, id: u64) {
        {
            let mut g = self.0.lock().unwrap();
            if let Some(e) = g.running.iter_mut().find(|e| e.info.id == id && e.info.cancellable) {
                e.cancel.store(true, Ordering::Relaxed);
                e.info.cancelling = true;
            }
        }
        self.notify();
    }

    /// Cancels every running Task of a kind.
    pub fn cancel_kind(&self, kind: TaskKind) {
        let ids: Vec<u64> = self
            .0
            .lock()
            .unwrap()
            .running
            .iter()
            .filter(|e| e.info.kind == kind)
            .map(|e| e.info.id)
            .collect();
        for id in ids {
            self.cancel(id);
        }
    }
}

pub struct TaskHandle {
    tasks: Tasks,
    id: u64,
    cancel: Arc<AtomicBool>,
    finished: bool,
}

impl TaskHandle {
    pub fn set_progress(&self, done: u64, total: Option<u64>) {
        {
            let mut g = self.tasks.0.lock().unwrap();
            if let Some(e) = g.running.iter_mut().find(|e| e.info.id == self.id) {
                e.info.done = done;
                e.info.total = total;
            }
        }
        self.tasks.notify();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Attach the action the Tasks list offers as Retry if this Task fails.
    pub fn set_retry(&self, retry: RetryAction) {
        let mut g = self.tasks.0.lock().unwrap();
        if let Some(e) = g.running.iter_mut().find(|e| e.info.id == self.id) {
            e.info.retry = Some(retry);
        }
    }

    pub fn finish(mut self, outcome: Outcome) {
        self.finish_inner(outcome);
    }

    fn finish_inner(&mut self, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        {
            let mut g = self.tasks.0.lock().unwrap();
            if let Some(pos) = g.running.iter().position(|e| e.info.id == self.id) {
                let e = g.running.remove(pos);
                let mut info = e.info;
                info.elapsed = e.started.elapsed();
                if !matches!(outcome, Outcome::Failed(_)) {
                    info.retry = None;
                }
                info.outcome = Some(outcome);
                g.finished.insert(0, info);
                g.finished.truncate(KEEP_FINISHED);
            }
        }
        self.tasks.notify();
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.finish_inner(Outcome::Failed("stopped".into()));
    }
}

impl TaskKind {
    /// What the progress counter counts.
    fn unit(self) -> &'static str {
        match self {
            TaskKind::Sync | TaskKind::CatchUp => "Entries read",
            TaskKind::DuplicateSearch => "files",
            TaskKind::Clean => "Cleanables",
            TaskKind::Trash | TaskKind::EmptyTrash => "items",
            TaskKind::CleanerSearch | TaskKind::Compare => "",
        }
    }
}

fn duration_text(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{} ms", d.as_millis()),
        1..60 => format!("{secs} s"),
        _ => format!("{} min {} s", secs / 60, secs % 60),
    }
}

/// The sidebar's Tasks row: a spinner and the count while any Task runs, the plain word otherwise.
pub fn indicator_row(tasks: &Tasks, open: bool, cx: &App) -> Stateful<Div> {
    let p = palette(cx);
    let (muted, text, active) = (p.muted, p.text, p.active);
    let running = tasks.running_count();
    h_flex()
        .id("nav-tasks")
        .h(px(32.))
        .my(px(1.))
        .px(px(12.))
        .gap(px(10.))
        .items_center()
        .rounded(px(10.))
        .cursor_pointer()
        .text_color(if running > 0 || open { text } else { muted })
        .when(open, |d| d.bg(active))
        .hover(move |s| s.text_color(text))
        .child(if running > 0 {
            Spinner::new().with_size(Size::Small).color(text).into_any_element()
        } else {
            Icon::new(IconName::Check).size(px(16.)).into_any_element()
        })
        .child(div().flex_1().child("Tasks"))
        .when(running > 0, |d| d.child(div().text_xs().child(running.to_string())))
}

/// The list of running Tasks and the last finished ones. Rebuilt on every render, so it stays
/// live while the Shell repaints.
pub fn list_panel<T: 'static>(tasks: &Tasks, cx: &mut Context<T>) -> Stateful<Div> {
    let p = palette(cx);
    let (muted, warning, positive) = (p.muted, p.warning, p.positive);
    let running = tasks.running();
    let finished = tasks.finished();
    let mut body = v_flex().gap(px(10.));
    if running.is_empty() && finished.is_empty() {
        body = body.child(caption("Nothing has run yet. Syncs, searches, Clean and Trash show here.", cx));
    }
    for t in running {
        let id = t.id;
        let unit = t.kind.unit();
        let progress = if t.total.is_some() || !unit.is_empty() {
            task_progress(("task-progress", id as usize), t.done, t.total, unit, cx)
        } else {
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(Spinner::new().with_size(Size::XSmall).color(muted))
                .child(div().text_xs().text_color(muted).child("Working…"))
        };
        let tasks = tasks.clone();
        body = body.child(
            v_flex()
                .gap(px(4.))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().flex_1().min_w_0().truncate().child(t.label.clone()))
                        .child(div().text_xs().text_color(muted).child(duration_text(t.elapsed)))
                        .when(t.cancellable, |d| {
                            d.child(
                                pill(("task-cancel", id as usize), if t.cancelling { "Cancelling…" } else { "Cancel" })
                                    .small()
                                    .disabled(t.cancelling.then_some("Cancel was requested"))
                                    .build(cx, cx.listener(move |_, _, _, _| tasks.cancel(id))),
                            )
                        }),
                )
                .child(progress),
        );
    }
    if !finished.is_empty() {
        body = body.child(div().mt(px(4.)).child(caption("Finished", cx)));
    }
    for t in finished {
        let id = t.id as usize;
        let (mark, tone, note) = match &t.outcome {
            Some(Outcome::Done(m)) => ("Done", positive, m.clone()),
            Some(Outcome::Cancelled) => ("Cancelled", muted, String::new()),
            Some(Outcome::Failed(m)) => ("Failed", warning, m.clone()),
            None => ("", muted, String::new()),
        };
        let retry = t.retry.clone();
        body = body.child(
            v_flex()
                .gap(px(2.))
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().text_color(tone).font_weight(FontWeight::MEDIUM).child(mark))
                        .child(div().flex_1().min_w_0().truncate().child(t.label.clone()))
                        .child(div().text_xs().text_color(muted).child(duration_text(t.elapsed)))
                        .when_some(retry, |d, retry| {
                            d.child(
                                pill(("task-retry", id), "Retry")
                                    .small()
                                    .build(cx, cx.listener(move |_, _, _, cx| retry(cx))),
                            )
                        }),
                )
                .when(!note.is_empty(), |d| d.child(div().text_xs().text_color(muted).child(note))),
        );
    }
    v_flex()
        .id("tasks-panel")
        .w(px(380.))
        .max_h(px(480.))
        .gap(px(12.))
        .p(px(16.))
        .rounded(px(RADIUS_CARD))
        .border_1()
        .border_color(p.border)
        .bg(p.surface)
        .shadow_lg()
        .occlude()
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Tasks"))
        .child(v_flex().id("tasks-scroll").flex_1().min_h_0().overflow_y_scroll().child(body))
}
