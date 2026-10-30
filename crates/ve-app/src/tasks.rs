//! Task registry: background work (Sync, Duplicate search, Cleaner search, Clean, Trash,
//! Empty Trash, Compare) that the Tasks page and the Inspector list. Cheap to clone; usable
//! from any thread.

use crate::theme::{RADIUS_CONTROL, palette};
use crate::ui::{button, caption, empty_state, section_title, task_progress};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::rc::Rc;
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

    /// Replaces the Task's label, e.g. when it moves to a phase with no countable progress.
    pub fn set_label(&self, label: String) {
        {
            let mut g = self.tasks.0.lock().unwrap();
            if let Some(e) = g.running.iter_mut().find(|e| e.info.id == self.id) {
                e.info.label = label;
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
        let outcome = if self.is_cancelled() {
            Outcome::Cancelled
        } else {
            Outcome::Failed("stopped".into())
        };
        self.finish_inner(outcome);
    }
}

impl Tasks {
    /// A running or recently finished Task, by id.
    pub fn find(&self, id: u64) -> Option<TaskInfo> {
        let g = self.0.lock().unwrap();
        g.running
            .iter()
            .find(|e| e.info.id == id)
            .map(|e| {
                let mut i = e.info.clone();
                i.elapsed = e.started.elapsed();
                i
            })
            .or_else(|| g.finished.iter().find(|t| t.id == id).cloned())
    }
}

impl TaskKind {
    /// What the progress counter counts.
    pub fn unit(self) -> &'static str {
        match self {
            TaskKind::Sync | TaskKind::CatchUp => "Entries read",
            TaskKind::DuplicateSearch => "files",
            TaskKind::Clean => "Cleanables",
            TaskKind::Trash | TaskKind::EmptyTrash => "items",
            TaskKind::CleanerSearch | TaskKind::Compare => "",
        }
    }
}

pub fn duration_text(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{} ms", d.as_millis()),
        1..60 => format!("{secs} s"),
        _ => format!("{} min {} s", secs / 60, secs % 60),
    }
}

/// Called with a Task's id when its row is clicked.
pub type Inspect = Rc<dyn Fn(u64, &mut Window, &mut App)>;

/// The Tasks page body: the running Tasks and the last finished ones, one row each. Rebuilt on
/// every render, so it stays live while the Shell repaints. A click on a row inspects it.
pub fn list<T: 'static>(tasks: &Tasks, selected: Option<u64>, inspect: Inspect, cx: &mut Context<T>) -> Div {
    let p = palette(cx);
    let (muted, warning) = (p.muted, p.warning);
    let running = tasks.running();
    let finished = tasks.finished();
    let mut body = v_flex().gap(px(4.));
    if running.is_empty() && finished.is_empty() {
        return body.child(empty_state("Nothing has run yet. Syncs, searches, Clean and Trash show here.", cx));
    }
    let raised = p.raised;
    let row = |id: u64| {
        let inspect = inspect.clone();
        v_flex()
            .id(("task-row", id as usize))
            .gap(px(4.))
            .p(px(10.))
            .rounded(px(RADIUS_CONTROL))
            .cursor_pointer()
            .when(selected == Some(id), |d| d.bg(raised))
            .hover(move |s| s.bg(raised))
            .on_click(move |_, window, cx| inspect(id, window, cx))
    };
    if !running.is_empty() {
        body = body.child(section_title("Running", cx).mb(px(4.)));
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
                .child(caption("Working…", cx))
        };
        let tasks = tasks.clone();
        body = body.child(
            row(id)
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().flex_1().min_w_0().truncate().child(t.label.clone()))
                        .child(caption(duration_text(t.elapsed), cx))
                        .when(t.cancellable, |d| {
                            d.child(
                                button(("task-cancel", id as usize), if t.cancelling { "Cancelling…" } else { "Cancel" })
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
        body = body.child(section_title("Finished", cx).mt(px(16.)).mb(px(4.)));
    }
    for t in finished {
        let id = t.id as usize;
        // Grey for what went fine; colour only on the Task that needs a look.
        let (mark, tone, note) = match &t.outcome {
            Some(Outcome::Done(m)) => ("Done", muted, m.clone()),
            Some(Outcome::Cancelled) => ("Cancelled", muted, String::new()),
            Some(Outcome::Failed(m)) => ("Failed", warning, m.clone()),
            None => ("", muted, String::new()),
        };
        let retry = t.retry.clone();
        body = body.child(
            row(t.id)
                .child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(
                            div()
                                .w(px(72.))
                                .flex_none()
                                .text_color(tone)
                                .font_weight(FontWeight::MEDIUM)
                                .child(mark),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(t.label.clone()))
                        .child(caption(duration_text(t.elapsed), cx))
                        .when_some(retry, |d, retry| {
                            d.child(
                                button(("task-retry", id), "Retry")
                                    .small()
                                    .build(cx, cx.listener(move |_, _, _, cx| retry(cx))),
                            )
                        }),
                )
                .when(!note.is_empty(), |d| d.child(caption(note, cx))),
        );
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn a_dropped_cancelled_task_is_cancelled_not_failed() {
        let tasks = Tasks::new();
        let handle = tasks.begin(TaskKind::Sync, "x".into(), true);
        handle.cancel_flag().store(true, Ordering::Relaxed);
        drop(handle);
        assert_eq!(tasks.finished()[0].outcome, Some(Outcome::Cancelled));
        assert!(tasks.finished()[0].retry.is_none());
    }

    #[::core::prelude::v1::test]
    fn a_dropped_task_that_was_not_cancelled_failed() {
        let tasks = Tasks::new();
        drop(tasks.begin(TaskKind::Sync, "x".into(), true));
        assert_eq!(tasks.finished()[0].outcome, Some(Outcome::Failed("stopped".into())));
    }
}
