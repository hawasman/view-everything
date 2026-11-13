//! Task registry: background work (Sync, Duplicate search, Cleaner search, Clean, Trash,
//! Empty Trash, Compare) that clients list. Cheap to clone; usable from any thread.
//!
//! Every Task records who started it. One the app started on its own (Catch-up, a periodic
//! Sync, a Duplicate or Cleaner search) runs at the operating system's background priority;
//! one the user started runs at full speed.

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

/// Re-runs a failed Task. Called by whichever client offers Retry, on its own thread.
pub type RetryAction = Arc<dyn Fn() + Send + Sync>;

/// Who started a Task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartedBy {
    /// The app, on its own: runs at background priority.
    App,
    /// The user (Sync now, Clean, Trash, Rescan, ...): runs at full speed.
    User,
}

impl StartedBy {
    /// "The app", "You": who started it, as the Inspector names them.
    pub fn who(self) -> &'static str {
        match self {
            StartedBy::App => "The app",
            StartedBy::User => "You",
        }
    }

    /// "Started by the app", "Started by you": the Work page's line under a Task.
    pub fn text(self) -> &'static str {
        match self {
            StartedBy::App => "Started by the app",
            StartedBy::User => "Started by you",
        }
    }
}

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
    pub started_by: StartedBy,
    /// A thread of the Task runs at background priority (see [`TaskHandle::enter_thread`]).
    pub background: bool,
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

    /// Begins a Task the user started: it runs at full speed.
    pub fn begin(&self, kind: TaskKind, label: String, cancellable: bool) -> TaskHandle {
        self.begin_as(kind, label, cancellable, StartedBy::User)
    }

    /// Begins a Task the app started on its own: each thread that does its work calls
    /// [`TaskHandle::enter_thread`] to drop to background priority.
    pub fn begin_by_app(&self, kind: TaskKind, label: String, cancellable: bool) -> TaskHandle {
        self.begin_as(kind, label, cancellable, StartedBy::App)
    }

    pub fn begin_as(&self, kind: TaskKind, label: String, cancellable: bool, started_by: StartedBy) -> TaskHandle {
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
                    started_by,
                    background: false,
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
            started_by,
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
    started_by: StartedBy,
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

    pub fn started_by(&self) -> StartedBy {
        self.started_by
    }

    /// Called first on each thread spawned to do this Task's work: a Task the app started
    /// drops that thread to background CPU and disk priority (see [`ve_scan::priority`]), and
    /// the threads a walk or a hash search spawns from it follow. Only for a thread of the
    /// Task's own: a lowered thread cannot be raised again.
    pub fn enter_thread(&self) {
        if self.started_by != StartedBy::App {
            return;
        }
        match ve_scan::priority::lower_current_thread() {
            Ok(()) => {
                let mut g = self.tasks.0.lock().unwrap();
                if let Some(e) = g.running.iter_mut().find(|e| e.info.id == self.id) {
                    e.info.background = true;
                }
            }
            Err(e) => eprintln!("Could not lower the priority of a background Task: {e}"),
        }
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
