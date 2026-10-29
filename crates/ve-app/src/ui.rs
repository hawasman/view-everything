//! Small building blocks of the final design: tonal panels, pill controls, tags.

use crate::theme::{RADIUS_CARD, palette};
use gpui_kit::component::h_flex;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Sizable as _, Size};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// A tonal panel (`.panel`): surface fill, 22px radius, 20px padding.
pub fn panel(cx: &App) -> Div {
    div().bg(palette(cx).surface).rounded(px(RADIUS_CARD)).p(px(20.)).min_w_0()
}

/// A muted caption (12px).
pub fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    div().text_xs().text_color(palette(cx).muted).child(text.into())
}

/// A pill control (`.btn`): outlined, 30px (small) or 38px high.
pub struct Pill {
    id: ElementId,
    label: SharedString,
    icon: Option<&'static str>,
    primary: bool,
    small: bool,
    selected: bool,
    disabled: Option<SharedString>,
    busy: bool,
}

pub fn pill(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Pill {
    Pill {
        id: id.into(),
        label: label.into(),
        icon: None,
        primary: false,
        small: false,
        selected: false,
        disabled: None,
        busy: false,
    }
}

impl Pill {
    pub fn icon(mut self, path: &'static str) -> Self {
        self.icon = Some(path);
        self
    }
    pub fn primary(mut self) -> Self {
        self.primary = true;
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
    /// Drawn pressed, as the Filters button while the panel is open.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    /// Dashed and muted; the tooltip says why.
    pub fn disabled(mut self, why: Option<impl Into<SharedString>>) -> Self {
        self.disabled = why.map(Into::into);
        self
    }

    pub fn build(self, cx: &App, on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Stateful<Div> {
        let p = palette(cx);
        let (text, control, on_primary, primary) = (p.text, p.control, p.on_primary, p.primary);
        let muted = p.muted;
        let off = self.disabled.is_some();
        let (fill, ink, edge) = if self.primary {
            (primary, on_primary, primary)
        } else if self.selected {
            (p.active, text, control)
        } else {
            (p.ground, if off { muted } else { text }, control)
        };
        let mut el = h_flex()
            .id(self.id)
            .h(px(if self.small { 30. } else { 38. }))
            .px(px(if self.small { 12. } else { 16. }))
            .gap(px(8.))
            .justify_center()
            .rounded_full()
            .border_1()
            .border_color(edge)
            .bg(fill)
            .text_color(ink)
            .text_size(px(if self.small { 13. } else { 14. }))
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap()
            .when(off, |d| d.border_dashed())
            .when(!off && self.busy, |d| d.cursor_default())
            .when(!off && !self.busy, |d| {
                d.cursor_pointer().hover(move |s| s.border_color(text)).on_click(on_click)
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
        .rounded_full()
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
                    .color(p.primary)
                    .w(px(160.)),
            )
            .child(format!("{}/{} {what}", crate::format::count(done), crate::format::count(total))),
        _ => row
            .child(Spinner::new().with_size(Size::XSmall).color(p.muted))
            .child(format!("{} {what}", crate::format::count(done))),
    }
}
