//! The Category breakdown panel: Filtered Size per Category, expandable to its extensions.

use crate::engine::Shared;
use crate::format;
use crate::treemap::category_color;
use crate::view::View;
use gpui_kit::component::input::InputState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::*;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use ve_core::Category;

const POLL: Duration = Duration::from_millis(50);

pub struct CategoryPanel {
    shared: Arc<Shared>,
    query: Entity<InputState>,
    view: Option<Arc<View>>,
    open: HashSet<Category>,
}

impl CategoryPanel {
    /// `query` is the Query input that clicking a Category or extension adds a term to.
    pub fn new(shared: Arc<Shared>, query: Entity<InputState>, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            shared,
            query,
            view: None,
            open: HashSet::new(),
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let latest = self.shared.view.lock().clone();
        let same = match (&self.view, &latest) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.view = latest;
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
        });
    }
}

impl Render for CategoryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (border, muted, hover, dark) = (
            theme.border,
            theme.muted_foreground,
            theme.accent,
            theme.is_dark(),
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
            list = list.child(
                v_flex()
                    .id(("category", category as usize))
                    .cursor_pointer()
                    .rounded_md()
                    .px_1()
                    .py_1()
                    .hover(move |s| s.bg(hover))
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
                            .child(
                                div()
                                    .id(("expand", category as usize))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
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
                                    ),
                            )
                            .child(div().size_3().rounded_sm().bg(color))
                            .child(div().flex_1().child(category.name()))
                            .child(
                                div()
                                    .font_family("Geist Mono")
                                    .text_sm()
                                    .child(format::size(row.totals.on_disk)),
                            ),
                    )
                    .child(
                        div().mt_1().h(px(3.)).rounded_full().bg(border).child(
                            div()
                                .h_full()
                                .w(relative(share.clamp(0.0, 1.0)))
                                .rounded_full()
                                .bg(color),
                        ),
                    ),
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
                                .font_family("Geist Mono")
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
