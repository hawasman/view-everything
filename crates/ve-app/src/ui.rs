//! Small building blocks of the design: panels, buttons, tabs, nav rows, tags.

use crate::theme::{
    CARD_PAD, CONTROL_H, CONTROL_H_SMALL, FS_BODY, FS_KPI, FS_SECTION, FS_SMALL, FS_TITLE, NAV_ROW, RADIUS_CARD, RADIUS_CHIP,
    RADIUS_CONTROL, palette,
};
use gpui_kit::component::h_flex;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::v_flex;
use gpui_kit::component::{Sizable as _, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::rc::Rc;

/// A panel: `surface` fill, 1px `border`, `RADIUS_CARD`, `CARD_PAD` padding, no shadow.
pub fn panel(cx: &App) -> Div {
    div()
        .bg(palette(cx).surface)
        .border_1()
        .border_color(palette(cx).border)
        .rounded(px(RADIUS_CARD))
        .p(px(CARD_PAD))
        .min_w_0()
}

/// A muted caption (`FS_SMALL`).
pub fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    div().text_size(px(FS_SMALL)).text_color(palette(cx).muted).child(text.into())
}

/// A page title row: `FS_TITLE`/600 title, line height 32, with `FS_BODY` muted facts beneath.
/// The caller puts the page's actions in an `h_flex` beside it.
pub fn title_row(title: impl Into<SharedString>, facts: Option<impl Into<SharedString>>, cx: &App) -> Div {
    let p = palette(cx);
    v_flex()
        .min_w_0()
        .child(
            div()
                .text_size(px(FS_TITLE))
                .line_height(px(32.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(title.into()),
        )
        .when_some(facts, |d, f| {
            d.child(div().text_size(px(FS_BODY)).text_color(p.muted).child(f.into()))
        })
}

/// A card or section title: `FS_SECTION`/500 ink.
pub fn section_title(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(FS_SECTION))
        .font_weight(FontWeight::MEDIUM)
        .text_color(palette(cx).text)
        .child(text.into())
}

/// A dialog title: `FS_SECTION`/600 ink.
pub fn dialog_title(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(FS_SECTION))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(palette(cx).text)
        .child(text.into())
}

/// A group label in a nav rail: `FS_SMALL` muted sentence case, 16px above, 4px below, 10px inset.
pub fn group_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .mt(px(16.))
        .mb(px(4.))
        .px(px(10.))
        .text_size(px(FS_SMALL))
        .text_color(palette(cx).muted)
        .child(text.into())
}

/// A text link: `FS_BODY` ink, underlined on hover.
pub fn link(id: impl Into<ElementId>, label: impl Into<SharedString>, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .text_size(px(FS_BODY))
        .text_color(palette(cx).text)
        .cursor_pointer()
        .whitespace_nowrap()
        .hover(|s| s.underline())
        .child(label.into())
}

/// An empty state: a 38px `FS_BODY` muted sentence in a 1px dashed `control` box.
pub fn empty_state(sentence: impl Into<SharedString>, cx: &App) -> Div {
    let p = palette(cx);
    div()
        .h(px(38.))
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .border_dashed()
        .border_color(p.control)
        .rounded(px(RADIUS_CARD))
        .text_size(px(FS_BODY))
        .text_color(p.muted)
        .child(sentence.into())
}

/// One container split by 1px `border` dividers into equal cells: label `FS_BODY`/500 muted over
/// the `FS_KPI`/400 value; the optional third item is a `FS_SMALL` muted subline.
pub fn kpi_strip(cells: Vec<(SharedString, SharedString, Option<SharedString>)>, cx: &App) -> Div {
    let p = palette(cx);
    let mut row = h_flex()
        .bg(p.surface)
        .border_1()
        .border_color(p.border)
        .rounded(px(RADIUS_CARD))
        .overflow_hidden();
    for (i, (label, value, sub)) in cells.into_iter().enumerate() {
        row = row.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .p(px(CARD_PAD))
                .when(i > 0, |d| d.border_l_1().border_color(p.border))
                .child(
                    div()
                        .text_size(px(FS_BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.muted)
                        .child(label),
                )
                .child(div().text_size(px(FS_KPI)).line_height(px(38.)).text_color(p.text).child(value))
                .when_some(sub, |d, s| d.child(div().text_size(px(FS_SMALL)).text_color(p.muted).child(s))),
        );
    }
    row
}

/// Underline tabs: 36px tall, labels `FS_BODY`/500; the selected one is ink with a 2px underline
/// exactly as wide as its label, the others muted; the row carries a full-width 1px `border` rule.
pub fn tabs(
    prefix: &'static str,
    labels: &[&str],
    selected: usize,
    cx: &App,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Div {
    let p = palette(cx);
    let on_select = Rc::new(on_select);
    let mut row = h_flex().h(px(CONTROL_H)).gap(px(20.)).border_b_1().border_color(p.border);
    for (i, label) in labels.iter().enumerate() {
        let on = i == selected;
        let on_select = on_select.clone();
        row = row.child(
            v_flex()
                .id((prefix, i))
                .h_full()
                .cursor_pointer()
                .text_size(px(FS_BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { p.text } else { p.muted })
                .when(!on, |d| d.hover(move |s| s.text_color(p.text)))
                .on_click(move |_, window, cx| on_select(i, window, cx))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .child(SharedString::from(label.to_string())),
                )
                .child(div().h(px(2.)).w_full().bg(if on { p.text } else { p.text.opacity(0.) })),
        );
    }
    row
}

/// A segmented control: a `raised` container (`RADIUS_CONTROL`, 32px tall), items with 12px
/// padding; the selected item is `surface` with ink, the others muted. No accent.
pub fn segmented(
    prefix: &'static str,
    labels: &[&str],
    selected: usize,
    cx: &App,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Div {
    let p = palette(cx);
    let on_select = Rc::new(on_select);
    let mut row = h_flex().h(px(32.)).p(px(2.)).gap(px(2.)).bg(p.raised).rounded(px(RADIUS_CONTROL));
    for (i, label) in labels.iter().enumerate() {
        let on = i == selected;
        let on_select = on_select.clone();
        row = row.child(
            h_flex()
                .id((prefix, i))
                .h_full()
                .px(px(12.))
                .items_center()
                .rounded(px(RADIUS_CONTROL - 2.))
                .text_size(px(FS_BODY))
                .font_weight(FontWeight::MEDIUM)
                .whitespace_nowrap()
                .cursor_pointer()
                .text_color(if on { p.text } else { p.muted })
                .when(on, |d| d.bg(p.surface))
                .when(!on, |d| d.hover(move |s| s.text_color(p.text)))
                .on_click(move |_, window, cx| on_select(i, window, cx))
                .child(SharedString::from(label.to_string())),
        );
    }
    row
}

/// A rail row: `NAV_ROW` tall, `RADIUS_CONTROL`, 10px inset (nested: 28px left), 16px icon,
/// `FS_BODY`/400 label muted until hover; the current row is `active` with ink. The caller
/// attaches `on_click`.
pub fn nav_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    icon: Option<&'static str>,
    current: bool,
    nested: bool,
    cx: &App,
) -> Stateful<Div> {
    let p = palette(cx);
    h_flex()
        .id(id)
        .h(px(NAV_ROW))
        .my(px(1.))
        .pl(px(if nested { 28. } else { 10. }))
        .pr(px(10.))
        .gap(px(8.))
        .items_center()
        .rounded(px(RADIUS_CONTROL))
        .text_size(px(FS_BODY))
        .cursor_pointer()
        .text_color(if current { p.text } else { p.muted })
        .when(current, |d| d.bg(p.active))
        .when(!current, |d| d.hover(move |s| s.text_color(p.text)))
        .when_some(icon, |d, path| {
            d.child(
                svg()
                    .path(path)
                    .size(px(16.))
                    .flex_none()
                    .text_color(if current { p.text } else { p.muted }),
            )
        })
        .child(div().min_w_0().truncate().child(label.into()))
}

/// What a button looks like.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Near-black fill, `on_primary` ink.
    Primary,
    /// `raised` fill, no border, ink.
    Secondary,
    /// No fill; `raised` on hover.
    Ghost,
    /// `raised` fill, `negative` ink.
    Danger,
}

/// A button: `CONTROL_H` (small: `CONTROL_H_SMALL`) tall, `RADIUS_CONTROL` corners, `FS_BODY`/500,
/// 14px side padding (small: 10).
pub struct Button {
    id: ElementId,
    label: SharedString,
    icon: Option<&'static str>,
    kind: ButtonKind,
    small: bool,
    pressed: bool,
    full_width: bool,
    disabled: Option<SharedString>,
    busy: bool,
}

pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button {
        id: id.into(),
        label: label.into(),
        icon: None,
        kind: ButtonKind::Secondary,
        small: false,
        pressed: false,
        full_width: false,
        disabled: None,
        busy: false,
    }
}

impl Button {
    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }
    pub fn icon(mut self, path: &'static str) -> Self {
        self.icon = Some(path);
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    /// A Task this control started is running: shows a spinner and ignores clicks.
    pub fn busy(mut self, busy: bool) -> Self {
        self.busy = busy;
        self
    }
    /// Drawn pressed (`active` fill), as the Filters button while its panel is open.
    pub fn pressed(mut self, pressed: bool) -> Self {
        self.pressed = pressed;
        self
    }
    /// Stretches to the width of its parent.
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }
    /// Muted ink; the tooltip says why.
    pub fn disabled(mut self, why: Option<impl Into<SharedString>>) -> Self {
        self.disabled = why.map(Into::into);
        self
    }

    pub fn build(self, cx: &App, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div> {
        let p = palette(cx);
        let off = self.disabled.is_some();
        let (fill, hover, ink) = match self.kind {
            ButtonKind::Primary => (Some(p.primary), p.primary.opacity(0.85), p.on_primary),
            ButtonKind::Secondary => (Some(p.raised), p.active, p.text),
            ButtonKind::Ghost => (None, p.raised, p.text),
            ButtonKind::Danger => (Some(p.raised), p.active, p.negative),
        };
        let ink = if off { p.muted } else { ink };
        let fill = if self.pressed { Some(p.active) } else { fill };
        let mut el = h_flex()
            .id(self.id)
            .h(px(if self.small { CONTROL_H_SMALL } else { CONTROL_H }))
            .px(px(if self.small { 10. } else { 14. }))
            .gap(px(8.))
            .justify_center()
            .rounded(px(RADIUS_CONTROL))
            .text_color(ink)
            .text_size(px(FS_BODY))
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap()
            .when(self.full_width, |d| d.w_full())
            .when_some(fill, |d, c| d.bg(c))
            .when(off || self.busy, |d| d.cursor_default())
            .when(!off && !self.busy, |d| {
                d.cursor_pointer().hover(move |s| s.bg(hover)).on_click(on_click)
            })
            .when(self.busy, |d| d.child(Spinner::new().with_size(Size::XSmall).color(ink)))
            .when_some(self.icon.filter(|_| !self.busy), |d, path| {
                d.child(svg().path(path).size(px(14.)).text_color(ink))
            })
            .child(self.label);
        if let Some(why) = self.disabled {
            el = el.tooltip(move |window, cx| Tooltip::new(why.clone()).build(window, cx));
        }
        el
    }
}

/// Accent and Warning tag the Catch-up and Offline states of Locations (Offline Volumes).
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Tone {
    Neutral,
    Accent,
    Warning,
}

/// A small status tag (`.pill`).
pub fn tag(label: impl Into<SharedString>, tone: Tone, cx: &App) -> Div {
    let p = palette(cx);
    let (bg, ink) = match tone {
        Tone::Neutral => p.pill_neutral,
        Tone::Accent => p.pill_accent,
        Tone::Warning => p.pill_warning,
    };
    div()
        .h(px(20.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(px(RADIUS_CHIP))
        .bg(bg)
        .text_color(ink)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .child(label.into())
}

/// A state dot (`.dot`).
pub fn dot(color: Hsla) -> Div {
    div().size(px(8.)).flex_none().rounded_full().bg(color)
}

/// A category swatch (`.sw`).
pub fn swatch(color: Hsla) -> Div {
    div().size(px(8.)).flex_none().rounded(px(2.)).bg(color)
}

/// A Task's progress: a determinate bar with `done/total` when the total is known, otherwise a
/// spinner with the live counter. `what` names the unit ("Cleanables", "Entries").
pub fn task_progress(id: impl Into<ElementId>, done: u64, total: Option<u64>, what: &str, cx: &App) -> Div {
    let p = palette(cx);
    let row = h_flex().gap(px(10.)).items_center().text_xs().text_color(p.muted);
    match total {
        Some(total) if total > 0 => row
            .child(
                Progress::new(id)
                    .value((done as f32 / total as f32 * 100.).min(100.))
                    .color(p.accent)
                    .w(px(160.)),
            )
            .child(if what.is_empty() {
                format!("{}/{}", crate::format::count(done), crate::format::count(total))
            } else {
                format!("{}/{} {what}", crate::format::count(done), crate::format::count(total))
            }),
        _ => row
            .child(Spinner::new().with_size(Size::XSmall).color(p.muted))
            .child(format!("{} {what}", crate::format::count(done)).trim_end().to_owned()),
    }
}
