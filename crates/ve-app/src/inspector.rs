//! The Inspector: the right pane that follows what was clicked, whatever the page. One shape for
//! an Entry, a Location, a Task or a Duplicate group: kind and tag, title, subtitle, facts, a
//! group's copies, then actions. The Shell builds the [`Panel`] of the inspected thing (it owns
//! the Engine and the actions) and this module draws it; [`Folders`] counts an inspected
//! folder's folders off the UI thread.

use crate::format;
use crate::theme::{FS_BODY, FS_SECTION, FS_SMALL, palette};
use crate::ui::{ButtonKind, Tone, button, group_label, tag};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use ve_core::EntryId;
use ve_engine::Shared;
use ve_engine::view::View;

/// What the Inspector shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspected {
    Entry(EntryId),
    /// A Volume's or Remote's identity, as `locations::Row` carries it.
    Location(String),
    /// A Task's id.
    Task(u64),
    /// A group of Duplicates, by its first copy.
    Duplicates(EntryId),
    /// An installed program, by its key (`ve_engine::apps::App::key`).
    App(String),
    /// A running or past process, by its PID and start time.
    Process(ve_engine::perf::ProcessId),
    /// A startup item, by its key (`ve_engine::apps::StartupItem::key`).
    Startup(String),
}

/// The width of the pane.
pub const WIDTH: f32 = 300.;

/// What an Inspector action does when clicked.
pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// One action button of the pane.
pub struct Item {
    pub id: &'static str,
    pub label: SharedString,
    pub kind: ButtonKind,
    pub busy: bool,
    pub disabled: Option<SharedString>,
    pub on_click: OnClick,
}

impl Item {
    pub fn new(id: &'static str, label: impl Into<SharedString>, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        Self {
            id,
            label: label.into(),
            kind: ButtonKind::Secondary,
            busy: false,
            disabled: None,
            on_click: Box::new(on_click),
        }
    }
    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }
    pub fn disabled(mut self, why: Option<&str>) -> Self {
        self.disabled = why.map(|w| SharedString::from(w.to_owned()));
        self
    }
}

/// Everything the pane draws.
pub struct Panel {
    /// "Entry", "Location" or "Task".
    pub kind: &'static str,
    pub tag: Option<(SharedString, Tone)>,
    pub title: SharedString,
    pub subtitle: Option<SharedString>,
    pub facts: Vec<(SharedString, SharedString)>,
    pub actions: Vec<Item>,
}

/// Marks a fact made by [`long_fact`]; never shown.
const LONG: char = '\u{2063}';

/// A fact whose value is long (a command, a path): drawn with the value wrapping under its
/// label instead of cut to one line. Other facts are unchanged.
pub fn long_fact(label: impl Into<SharedString>, value: impl Into<SharedString>) -> (SharedString, SharedString) {
    (format!("{LONG}{}", label.into()).into(), value.into())
}

/// One copy of a Duplicate group, listed under Copies; a click makes it the copy to keep.
pub struct Member {
    /// The copy's name, and the folder it sits in.
    pub name: SharedString,
    pub folder: SharedString,
    pub keep: bool,
    pub on_click: OnClick,
}

/// Draws the pane, with a Copies list between the facts and the actions when `members` holds any.
pub fn render(panel: Panel, members: Vec<Member>, cx: &App) -> Stateful<Div> {
    let p = palette(cx);
    let facts = panel.facts.into_iter().enumerate().map(|(i, (label, value))| {
        let row = match label.strip_prefix(LONG) {
            Some(label) => v_flex()
                .py(px(6.))
                .gap(px(2.))
                .child(div().text_color(p.muted).child(SharedString::from(label.to_owned())))
                .child(div().w_full().text_color(p.text).child(value)),
            None => h_flex()
                .min_h(px(32.))
                .gap(px(12.))
                .items_center()
                .justify_between()
                .child(div().flex_none().text_color(p.muted).child(label))
                .child(div().min_w_0().truncate().text_color(p.text).child(value)),
        };
        row.text_size(px(FS_BODY)).when(i > 0, |d| d.border_t_1().border_color(p.rule))
    });
    let has_members = !members.is_empty();
    let members = members.into_iter().enumerate().map(|(i, m)| {
        h_flex()
            .id(("inspector-member", i))
            .min_h(px(44.))
            .py(px(4.))
            .gap(px(8.))
            .items_center()
            .text_size(px(FS_BODY))
            .cursor_pointer()
            .when(i > 0, |d| d.border_t_1().border_color(p.rule))
            .on_click(m.on_click)
            .child(
                div()
                    .flex_none()
                    .text_color(if m.keep { p.text } else { p.muted })
                    .child(if m.keep { "●" } else { "○" }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_color(p.text).child(m.name))
                    .child(div().truncate().text_size(px(FS_SMALL)).text_color(p.muted).child(m.folder)),
            )
            .when(m.keep, |d| {
                d.child(div().flex_none().text_size(px(FS_SMALL)).text_color(p.muted).child("keep"))
            })
    });
    let has_actions = !panel.actions.is_empty();
    let actions = panel
        .actions
        .into_iter()
        .map(|a| {
            let mut b = button(a.id, a.label).kind(a.kind).busy(a.busy).full_width();
            b = b.disabled(a.disabled);
            b.build(cx, a.on_click)
        })
        .collect::<Vec<_>>();
    v_flex()
        .id("inspector")
        .w(px(WIDTH))
        .h_full()
        .flex_none()
        .border_l_1()
        .border_color(p.border)
        .bg(p.ground)
        .child(
            v_flex()
                .id("inspector-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(px(20.))
                .gap(px(4.))
                .child(
                    h_flex()
                        .h(px(20.))
                        .justify_between()
                        .items_center()
                        .child(div().text_size(px(FS_SMALL)).text_color(p.muted).child(panel.kind))
                        .children(panel.tag.map(|(label, tone)| tag(label, tone, cx))),
                )
                .child(
                    div()
                        .mt(px(4.))
                        .text_size(px(FS_SECTION))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.text)
                        .truncate()
                        .child(panel.title),
                )
                .children(
                    panel
                        .subtitle
                        .map(|s| div().text_size(px(FS_SMALL)).text_color(p.muted).truncate().child(s)),
                )
                .child(v_flex().mt(px(12.)).children(facts))
                .when(has_members, |d| {
                    d.child(group_label("Copies", cx).px(px(0.))).child(v_flex().children(members))
                })
                .when(has_actions, |d| {
                    d.child(group_label("Actions", cx).px(px(0.)))
                        .child(v_flex().gap(px(8.)).children(actions))
                }),
        )
        .child(
            div()
                .flex_none()
                .px(px(20.))
                .py(px(10.))
                .border_t_1()
                .border_color(p.border)
                .text_size(px(FS_SMALL))
                .text_color(p.muted)
                .child("Follows your selection · Esc clears"),
        )
}

/// The count of folders under the inspected folder, for its Contains fact: counted off the UI
/// thread, kept per (Entry, View).
pub struct Folders {
    shared: Arc<Shared>,
    /// The last count: (Entry, View id, folders).
    counted: Option<(EntryId, u64, u64)>,
    /// The (Entry, View id) a count is running for.
    pending: Option<(EntryId, u64)>,
}

/// Folders shown under `dir` by `view`, at any depth. The Index is locked per folder, not for
/// the whole walk, so a writer never waits behind a large count.
fn count_folders(index: &ve_scan::SharedIndex, view: &View, dir: EntryId) -> u64 {
    let mut stack = vec![dir];
    let mut folders = 0;
    while let Some(id) = stack.pop() {
        let index = index.read();
        for child in index.children(id) {
            if index.is_dir(child) && view.filter.is_shown(child) {
                folders += 1;
                stack.push(child);
            }
        }
    }
    folders
}

impl Folders {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            counted: None,
            pending: None,
        }
    }

    /// "12 folders" for the folder `id` under `view`, or "counting folders…" while the count
    /// runs. A new View (a zoom) changes the key, so the count is asked for again.
    pub fn text(&mut self, id: EntryId, view: &Arc<View>, cx: &mut Context<Self>) -> String {
        let key = (id, view.id);
        if let Some((_, _, folders)) = self.counted.filter(|&(e, v, _)| (e, v) == key) {
            return format!("{} folder{}", format::count(folders), if folders == 1 { "" } else { "s" });
        }
        if self.pending != Some(key) {
            self.pending = Some(key);
            let (shared, view) = (self.shared.clone(), view.clone());
            cx.spawn(async move |this, cx| {
                let folders = cx
                    .background_executor()
                    .spawn(async move { count_folders(&shared.index, &view, id) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this.pending == Some(key) {
                        this.pending = None;
                    }
                    this.counted = Some((key.0, key.1, folders));
                    cx.notify();
                });
            })
            .detach();
        }
        "counting folders…".to_owned()
    }
}
