//! The Tasks page body: the engine's Tasks, running and recently finished, one row each.

use crate::theme::{RADIUS_CONTROL, palette};
use crate::ui::{button, caption, empty_state, section_title, task_progress};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::rc::Rc;
use std::time::Duration;
use ve_engine::tasks::{Outcome, Tasks};

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
                                    .build(cx, cx.listener(move |_, _, _, _| retry())),
                            )
                        }),
                )
                .when(!note.is_empty(), |d| d.child(caption(note, cx))),
        );
    }
    body
}
