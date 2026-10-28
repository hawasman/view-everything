//! The Explorer's flat list: every Entry that matches the Query, largest first, as a
//! virtualised table.

use crate::engine::Shared;
use crate::selection::Selection;
use crate::{format, view::View};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;
use ve_core::EntryId;

const COLUMNS: [(&str, f32, bool); 6] = [
    ("Name", 260., false),
    ("Folder", 380., false),
    ("On disk", 100., true),
    ("Size", 100., true),
    ("Files", 80., true),
    ("Modified", 140., true),
];

/// Width of the columns after Name and Folder, which share what is left of the list's width.
const FIXED_WIDTH: f32 = 100. + 100. + 80. + 140. + 16.;
const MIN_FLEX_WIDTH: f32 = 80.;

/// Rows kept in `FlatList::rows` before it starts over.
const CACHED_ROWS: usize = 512;

pub struct FlatList {
    shared: Arc<Shared>,
    pub view: Option<Arc<View>>,
    /// Cell texts of rows drawn recently, filled by `render_tr` so the Index is locked once per
    /// row rather than once per cell. Dropped when the View changes.
    rows: HashMap<usize, [String; 6]>,
    pub selection: Selection,
    /// Width the table has to fit in; 0 until first measured.
    available: f32,
}

impl FlatList {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            view: None,
            rows: HashMap::new(),
            selection: Selection::default(),
            available: 0.,
        }
    }

    pub fn entry_at(&self, row: usize) -> Option<EntryId> {
        self.view.as_ref()?.flat.get(row).copied()
    }

    /// Fits the table into `width`: Name and Folder shrink first, the size columns stay.
    /// True when the columns changed.
    pub fn set_available(&mut self, width: f32) -> bool {
        if (width - self.available).abs() < 1. {
            return false;
        }
        self.available = width;
        true
    }

    fn width_of(&self, ix: usize) -> f32 {
        if self.available <= 0. || ix >= 2 {
            return COLUMNS[ix].1;
        }
        let flexible = (self.available - FIXED_WIDTH).max(2. * MIN_FLEX_WIDTH);
        // Name gets two fifths, Folder the rest.
        if ix == 0 { flexible * 0.4 } else { flexible * 0.6 }
    }

    /// Shows `view`; false when it already was the one shown.
    pub fn set_view(&mut self, view: Option<Arc<View>>) -> bool {
        if crate::view::same(&self.view, &view) {
            return false;
        }
        self.view = view;
        self.rows.clear();
        // The Entries the new View no longer matches fall out of the selection.
        if let Some(view) = self.view.clone() {
            self.selection.retain(|id| view.filter.is_matched(id));
        } else {
            self.selection.clear();
        }
        true
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
        let (name, _, numeric) = COLUMNS[ix];
        let column = Column::new(name, name).width(self.width_of(ix));
        if numeric { column.text_right() } else { column }
    }

    fn render_tr(
        &mut self,
        row: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        if self.rows.len() > CACHED_ROWS {
            self.rows.clear();
        }
        if let (false, Some(view)) = (self.rows.contains_key(&row), self.view.clone()) {
            let index = self.shared.index.read();
            let id = view.flat.get(row).copied().filter(|&id| index.is_live(id));
            if let Some(id) = id {
                let totals = view.filter.totals(&index, id);
                let files = if index.is_dir(id) {
                    format::count(totals.files)
                } else {
                    String::new()
                };
                let folder = index
                    .parent(id)
                    .map(|p| index.path(p).display().to_string());
                self.rows.insert(
                    row,
                    [
                        index.name(id).to_owned(),
                        folder.unwrap_or_default(),
                        format::size(totals.on_disk),
                        format::size(totals.logical),
                        files,
                        format::time(index.modified(id)),
                    ],
                );
            }
        }
        let selected = self.entry_at(row).is_some_and(|id| self.selection.contains(id));
        let accent = cx.theme().accent;
        div()
            .id(("row", row))
            .when(selected, |d| d.bg(accent))
            .on_click(cx.listener(move |table, e: &ClickEvent, _, cx| {
                let list = table.delegate_mut();
                let view = list.view.clone();
                let entry_at = |r: usize| view.as_ref()?.flat.get(r).copied();
                list.selection.click(row, e.modifiers(), entry_at);
                cx.notify();
            }))
    }

    fn render_last_empty_col(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div().w_0()
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(text) = self.rows.get(&row).map(|cells| cells[col].clone()) else {
            return div().into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        let cell = h_flex().size_full().overflow_hidden().whitespace_nowrap();
        match col {
            1 => cell.text_color(muted),
            0 => cell,
            2 => cell.justify_end(),
            _ => cell.text_color(muted).justify_end(),
        }
        .child(div().min_w_0().truncate().child(text))
        .into_any_element()
    }
}
