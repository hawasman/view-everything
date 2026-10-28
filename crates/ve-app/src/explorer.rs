//! The Explorer's flat list: every Entry that matches the Query, largest first, as a
//! virtualised table.

use crate::engine::Shared;
use crate::{format, view::View};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;
use std::sync::Arc;

const COLUMNS: [(&str, f32, bool); 6] = [
    ("Name", 260., false),
    ("Folder", 380., false),
    ("On disk", 100., true),
    ("Size", 100., true),
    ("Files", 80., true),
    ("Modified", 140., true),
];

pub struct FlatList {
    shared: Arc<Shared>,
    pub view: Option<Arc<View>>,
}

impl FlatList {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared, view: None }
    }
}

impl TableDelegate for FlatList {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.view.as_ref().map_or(0, |v| v.flat.len())
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let (name, width, numeric) = COLUMNS[ix];
        let column = Column::new(name, name).width(width);
        if numeric { column.text_right() } else { column }
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(view) = &self.view else {
            return div().into_any_element();
        };
        let index = self.shared.index.read();
        let id = view.flat[row];
        if !index.is_live(id) {
            return div().into_any_element();
        }
        // Only the size columns need totals; computing them is the costly part of a cell.
        let totals = if (2..=4).contains(&col) {
            view.filter.totals(&index, id)
        } else {
            Default::default()
        };
        let is_dir = index.is_dir(id);
        let muted = cx.theme().muted_foreground;
        let text = match col {
            0 => index.name(id).to_owned(),
            1 => index
                .parent(id)
                .map(|p| index.path(p).display().to_string())
                .unwrap_or_default(),
            2 => format::size(totals.on_disk),
            3 => format::size(totals.logical),
            4 if is_dir => format::count(totals.files),
            4 => String::new(),
            _ => format::time(index.modified(id)),
        };
        let cell = h_flex().size_full().overflow_hidden().whitespace_nowrap();
        match col {
            1 => cell.text_color(muted),
            0 => cell,
            2 => cell.justify_end(),
            _ => cell.text_color(muted).justify_end(),
        }
        .child(text)
        .into_any_element()
    }
}
