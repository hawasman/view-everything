//! "Compare with": the Explorer's choice of a Snapshot date, and the comparison of the Index
//! with it, computed off the UI thread and kept current while the Index changes.

use chrono::NaiveDate;
use gpui_kit::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use ve_core::snapshot::{self, Comparison, Snapshot};
use ve_engine::Shared;
use ve_engine::tasks::{Outcome, TaskHandle, TaskKind, Tasks};

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
    /// The dates that have a Snapshot, newest first.
    dates: Vec<NaiveDate>,
    dates_read: Option<Instant>,
    /// The date a failed comparison's Retry asked for; taken at the next look.
    retry: Arc<Mutex<Option<NaiveDate>>>,
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
            dates: Vec::new(),
            dates_read: None,
            retry: Arc::new(Mutex::new(None)),
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

    /// Runs the Retry of a failed comparison, and recomputes when the Index has changed since
    /// the comparison was made.
    pub fn refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        let asked = self.retry.lock().take();
        if let Some(date) = asked {
            self.choose(Some(date), cx);
            return;
        }
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
                        let dir = shared.snapshots_dir().ok_or("no data folder")?;
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
                            // The Task list calls Retry from anywhere; the comparison restarts at the next look.
                            let (asked, repaint) = (this.retry.clone(), this.shared.repaint.clone());
                            task.set_retry(Arc::new(move || {
                                *asked.lock() = Some(date);
                                repaint();
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

    /// The dates that have a Snapshot, newest first, as of the last [`Self::refresh_dates`].
    pub fn dates(&self) -> &[NaiveDate] {
        &self.dates
    }

    /// Lists the Snapshots on disk again, at most every 30 seconds (the first Index save of a
    /// day stores one).
    pub fn refresh_dates(&mut self) {
        if self.dates_read.is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        self.dates_read = Some(Instant::now());
        self.dates = self.shared.snapshots_dir().map(|d| snapshot::list(&d)).unwrap_or_default();
    }
}

/// Whether a comparison the user asked for (a Task, not a quiet recompute) is running.
fn user_compare_running(tasks: &Tasks) -> bool {
    tasks.is_running(TaskKind::Compare)
}

#[cfg(test)]
mod tests {
    use ve_engine::tasks::{Outcome, TaskKind, Tasks};

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
