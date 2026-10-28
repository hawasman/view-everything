//! The Category breakdown panel: Filtered Size per Category, expandable to its extensions.

use crate::engine::Shared;
use crate::format;
use crate::treemap::category_color;
use crate::view::View;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::*;
use std::collections::HashSet;
use std::sync::Arc;
use ve_core::Category;

/// Digits of equal width (OpenType `tnum`), so sizes line up down the panel.
fn tabular_figures() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// Every Category's bar has this height; small Categories are kept visible by a minimum length.
const BAR_HEIGHT: Pixels = px(6.);

pub struct CategoryPanel {
    shared: Arc<Shared>,
    query: Entity<InputState>,
    view: Option<Arc<View>>,
    open: HashSet<Category>,
}

impl CategoryPanel {
    /// `query` is the Query input that clicking a Category or extension adds a term to.
    pub fn new(shared: Arc<Shared>, query: Entity<InputState>) -> Self {
        Self {
            shared,
            query,
            view: None,
            open: HashSet::new(),
        }
    }

    pub fn set_view(&mut self, view: Option<Arc<View>>, cx: &mut Context<Self>) {
        if !crate::view::same(&self.view, &view) {
            self.view = view;
            cx.notify();
        }
    }

    /// Adds `term` to the Query unless it is already there.
    fn add_to_query(&self, term: String, window: &mut Window, cx: &mut App) {
        self.query.update(cx, |query, cx| {
            let mut text = query.value().to_string();
            if text
                .split_whitespace()
                .any(|t| t.eq_ignore_ascii_case(&term))
            {
                return;
            }
            if !text.trim().is_empty() {
                text.push(' ');
            }
            text.push_str(&term);
            query.set_value(text, window, cx);
            // set_value is silent; the Shell recomputes the View on Change.
            cx.emit(InputEvent::Change);
        });
    }
}

impl Render for CategoryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, hover, dark, track) = (
            theme.border,
            theme.muted_foreground,
            theme.accent,
            theme.is_dark(),
            theme.muted,
        );
        let panel = v_flex()
            .size_full()
            .border_l_1()
            .border_color(border)
            .p_3()
            .gap_2();
        let Some(view) = self.view.clone() else {
            return panel;
        };
        let largest = view
            .categories
            .first()
            .map_or(1, |r| r.totals.on_disk.max(1));
        let mut list = v_flex()
            .id("categories")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_0p5();
        for row in &view.categories {
            let category = row.category;
            let open = self.open.contains(&category);
            let color = category_color(category, dark);
            let share = row.totals.on_disk as f32 / largest as f32;
            // The chevron expands; the rest of the row adds the Category to the Query.
            let pick = v_flex()
                .id(("category", category as usize))
                .flex_1()
                .cursor_pointer()
                .tooltip({
                    let text = format!(
                        "{} files — show only this Category",
                        format::count(row.totals.files)
                    );
                    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.add_to_query(
                        format!("type:{}", category.name().to_lowercase()),
                        window,
                        cx,
                    );
                }))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().size_3().rounded_sm().bg(color))
                        .child(div().flex_1().child(category.name()))
                        .child(
                            div()
                                .text_sm()
                                .font_features(tabular_figures())
                                .child(format::size(row.totals.on_disk)),
                        ),
                )
                .child(
                    div().mt_1().h(BAR_HEIGHT).rounded_full().bg(track).child(
                        div()
                            .h_full()
                            .w(relative(share.clamp(0.0, 1.0)))
                            .min_w(px(6.))
                            .rounded_full()
                            .bg(color),
                    ),
                );
            let chevron = div()
                .id(("expand", category as usize))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.open.remove(&category) {
                        this.open.insert(category);
                    }
                    cx.notify();
                }))
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size_4()
                    .text_color(muted),
                );
            list = list.child(
                h_flex()
                    .gap_2()
                    .rounded_md()
                    .px_1()
                    .py_1()
                    .hover(move |s| s.bg(hover))
                    .child(chevron)
                    .child(pick),
            );
            if open {
                for (ext, totals) in &row.extensions {
                    let term = (ext != "(none)").then(|| format!("ext:{ext}"));
                    let label = if term.is_some() {
                        format!(".{ext}")
                    } else {
                        ext.clone()
                    };
                    let mut item = h_flex()
                        .id(SharedString::from(format!("ext-{category:?}-{ext}")))
                        .pl_8()
                        .pr_1()
                        .py_0p5()
                        .rounded_md()
                        .text_sm()
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .font_features(tabular_figures())
                                .text_color(muted)
                                .child(format::size(totals.on_disk)),
                        );
                    if let Some(term) = term {
                        item = item.cursor_pointer().hover(move |s| s.bg(hover)).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.add_to_query(term.clone(), window, cx);
                            }),
                        );
                    }
                    list = list.child(item);
                }
            }
        }
        panel
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Categories"),
            )
            .child(div().text_sm().text_color(muted).child(format!(
                "{} on disk in {} files",
                format::size(view.total.on_disk),
                format::count(view.total.files)
            )))
            .child(list)
    }
}
