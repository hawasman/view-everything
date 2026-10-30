//! The Inspector: the right pane that follows what was clicked, whatever the page. One shape for
//! an Entry, a Location or a Task: kind and tag, title, subtitle, facts, then actions. The
//! Shell builds the [`Panel`] of the inspected thing (it owns the Engine and the actions) and
//! this module draws it.

use crate::theme::{FS_SECTION, FS_SMALL, palette};
use crate::ui::{ButtonKind, Tone, button, group_label, tag};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ve_core::EntryId;

/// What the Inspector shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inspected {
    Entry(EntryId),
    /// A Volume's or Remote's identity, as `locations::Row` carries it.
    Location(String),
    /// A Task's id.
    Task(u64),
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

/// Draws the pane.
pub fn render(panel: Panel, cx: &App) -> Stateful<Div> {
    let p = palette(cx);
    let facts = panel.facts.into_iter().enumerate().map(|(i, (label, value))| {
        h_flex()
            .min_h(px(32.))
            .gap(px(12.))
            .items_center()
            .justify_between()
            .text_size(px(crate::theme::FS_BODY))
            .when(i > 0, |d| d.border_t_1().border_color(p.rule))
            .child(div().flex_none().text_color(p.muted).child(label))
            .child(div().min_w_0().truncate().text_color(p.text).child(value))
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
