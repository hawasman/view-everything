//! The Category breakdown panel: Filtered Size per Category, expandable to its extensions.

use crate::format;
use crate::theme::{RADIUS_CARD, palette};
use crate::ui::swatch;
use crate::view::{CategoryRow, View};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashSet;
use std::sync::Arc;
use ve_core::Category;

/// Column widths after the name, as in the design: bar, Size, Files, Share.
const BAR: f32 = 140.;
const SIZE: f32 = 80.;
const FILES: f32 = 80.;
const SHARE: f32 = 64.;

pub struct CategoryPanel {
    /// The Query input a click adds a term to; none, and rows do not click (the Cleaner's).
    query: Option<Entity<InputState>>,
    subtitle: SharedString,
    view: Option<Arc<View>>,
    /// The rows drawn (largest first) and the On-disk Size their shares are of.
    rows: Option<(Vec<CategoryRow>, u64)>,
    open: HashSet<Category>,
}

impl CategoryPanel {
    /// `query` is the Query input that clicking a Category or extension adds a term to.
    pub fn new(query: Option<Entity<InputState>>, subtitle: impl Into<SharedString>) -> Self {
        Self {
            query,
            subtitle: subtitle.into(),
            view: None,
            rows: None,
            open: HashSet::new(),
        }
    }

    pub fn set_view(&mut self, view: Option<Arc<View>>, cx: &mut Context<Self>) {
        if !crate::view::same(&self.view, &view) {
            self.rows = view.as_ref().map(|v| (v.categories.clone(), v.total.on_disk));
            self.view = view;
            cx.notify();
        }
    }

    /// Shows `rows` (largest first) as shares of `whole`, an On-disk Size.
    pub fn set_rows(&mut self, rows: Vec<CategoryRow>, whole: u64, subtitle: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.rows = Some((rows, whole));
        self.subtitle = subtitle.into();
        cx.notify();
    }

    /// Adds `term` to the Query unless it is already there.
    fn add_to_query(&self, term: String, window: &mut Window, cx: &mut App) {
        let Some(query) = &self.query else { return };
        query.update(cx, |query, cx| {
            let mut text = query.value().to_string();
            if text.split_whitespace().any(|t| t.eq_ignore_ascii_case(&term)) {
                return;
            }
            if !text.trim().is_empty() {
                text.push(' ');
            }
            text.push_str(&term);
            query.set_value(text, window, cx);
            // set_value is silent; the Shell recomputes the View on Change.
            cx.emit(InputEvent::PressEnter {
                secondary: false,
                shift: false,
            });
        });
    }
}

impl Render for CategoryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (muted, rule, free, hover, text) = (p.muted, p.rule, p.free, p.active, p.text);
        let cats = Category::ALL.map(|c| p.category(c));
        let panel = v_flex()
            .size_full()
            .bg(p.surface)
            .rounded(px(RADIUS_CARD))
            .pt(px(16.))
            .px(px(12.))
            .pb(px(8.));
        let heading = h_flex()
            .px(px(8.))
            .mb(px(4.))
            .justify_between()
            .items_baseline()
            .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child("Categories"))
            .child(div().text_xs().text_color(muted).child(self.subtitle.clone()));
        let Some((rows, whole)) = self.rows.clone() else {
            return panel.child(heading);
        };
        let clickable = self.query.is_some();
        let largest = rows.first().map_or(1, |r| r.totals.on_disk.max(1));
        let whole = whole.max(1);
        let cell = |w: f32| div().w(px(w)).flex_none().text_right().whitespace_nowrap();
        let header = h_flex()
            .h(px(30.))
            .px(px(8.))
            .items_center()
            .text_xs()
            .text_color(muted)
            .child(div().flex_1().child("Category"))
            .child(div().w(px(BAR)).flex_none())
            .child(cell(SIZE).child("Size"))
            .child(cell(FILES).child("Files"))
            .child(cell(SHARE).child("Share"));
        let mut list = v_flex().id("categories").flex_1().min_h_0().overflow_y_scroll();
        for row in &rows {
            let category = row.category;
            let open = self.open.contains(&category);
            let color = cats[category as usize];
            let share = row.totals.on_disk as f32 / largest as f32;
            // The chevron expands; the rest of the row adds the Category to the Query.
            let chevron = div()
                .id(("expand", category as usize))
                .w(px(14.))
                .flex_none()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.open.remove(&category) {
                        this.open.insert(category);
                    }
                    cx.notify();
                    cx.stop_propagation();
                }))
                .child(
                    Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                        .size(px(14.))
                        .text_color(muted),
                );
            list = list.child(
                h_flex()
                    .id(("category", category as usize))
                    .h(px(30.))
                    .px(px(8.))
                    .items_center()
                    .border_t_1()
                    .border_color(rule)
                    .text_size(px(13.))
                    .when(clickable, |d| {
                        d.cursor_pointer()
                            .hover(move |s| s.bg(hover).rounded(px(8.)))
                            .tooltip({
                                let text = format!("{} files — show only this Category", format::count(row.totals.files));
                                move |window, cx| Tooltip::new(text.clone()).build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_to_query(format!("type:{}", category.name().to_lowercase()), window, cx);
                            }))
                    })
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(6.))
                            .items_center()
                            .child(chevron)
                            .child(swatch(color).mx(px(2.)))
                            .child(category.name()),
                    )
                    .child(
                        div().w(px(BAR)).flex_none().px(px(12.)).child(
                            div()
                                .h(px(4.))
                                .rounded(px(2.))
                                .bg(free)
                                .overflow_hidden()
                                .child(div().h_full().w(relative(share.clamp(0.0, 1.0))).bg(color)),
                        ),
                    )
                    .child(cell(SIZE).child(format::size(row.totals.on_disk)))
                    .child(cell(FILES).child(format::count(row.totals.files)))
                    .child(cell(SHARE).child(format!("{:.1}%", row.totals.on_disk as f64 * 100.0 / whole as f64))),
            );
            if open {
                for (ext, totals) in &row.extensions {
                    let term = (ext != "(none)").then(|| format!("ext:{ext}"));
                    let label = if term.is_some() { format!(".{ext}") } else { ext.clone() };
                    let mut item = h_flex()
                        .id(SharedString::from(format!("ext-{category:?}-{ext}")))
                        .h(px(30.))
                        .pl(px(36.))
                        .pr(px(8.))
                        .items_center()
                        .border_t_1()
                        .border_color(rule)
                        .text_size(px(13.))
                        .child(div().flex_1().child(label))
                        .child(cell(SIZE).text_color(muted).child(format::size(totals.on_disk)))
                        .child(cell(FILES).text_color(muted).child(format::count(totals.files)))
                        .child(
                            cell(SHARE)
                                .text_color(muted)
                                .child(format!("{:.1}%", totals.on_disk as f64 * 100.0 / whole as f64)),
                        );
                    if let Some(term) = term.filter(|_| clickable) {
                        item = item
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover).text_color(text))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_to_query(term.clone(), window, cx);
                            }));
                    }
                    list = list.child(item);
                }
            }
        }
        panel.child(heading).child(header).child(list)
    }
}
