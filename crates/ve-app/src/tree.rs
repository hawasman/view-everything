//! The Explorer's tree: folders that expand on demand, each level sorted by Filtered Size.

use crate::engine::Shared;
use crate::format;
use crate::selection::Selection;
use crate::theme::palette;
use crate::ui::swatch;
use crate::view::{self, Focus, View};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{Icon, IconName, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use ve_core::snapshot::{Change, Comparison, Removed};
use ve_core::{Category, CategoryMap, EntryId, Index};

/// Rows kept in `TreeList::cells` before it starts over.
const CACHED_ROWS: usize = 512;
const INDENT: f32 = 16.;
/// Width of the columns after Name, which takes what is left of the list's width.
const FIXED_WIDTH: f32 = 72. + 72. + 108. + 76. + 136. + 28.;
/// The Change column, present while comparing with a Snapshot.
const CHANGE_WIDTH: f32 = 112.;
const MIN_NAME_WIDTH: f32 = 160.;

const COLUMNS: [(&str, f32, bool); 7] = [
    ("Name", 360., false),
    ("On disk", 72., true),
    ("Size", 72., true),
    ("%", 108., true),
    ("Files", 76., true),
    ("Modified", 136., true),
    ("Change", CHANGE_WIDTH, true),
];
const CHANGE: usize = 6;

#[derive(Clone, Copy)]
struct Row {
    id: EntryId,
    depth: usize,
    /// A folder the Snapshot had that is gone: its place in `TreeList::gone`, and `id` is the
    /// folder it was in. Not an Entry, so never selectable.
    gone: Option<usize>,
}

/// Which size measure siblings are ordered by, and which way.
#[derive(Clone, Copy)]
struct Sort {
    on_disk: bool,
    /// Ordered by how much a folder changed since the Snapshot, in On-disk Size.
    change: bool,
    descending: bool,
}

/// Everything one visible row shows, read under a single lock of the Index.
struct Cells {
    depth: usize,
    name: String,
    expandable: bool,
    open: bool,
    /// Shown only for what it contains, so drawn muted.
    muted: bool,
    /// A file's Category, or the Category holding most of a folder; None for a Volume's root.
    category: Option<Category>,
    /// Share of the parent, 0..=1.
    share: f32,
    texts: [String; 6],
}

pub struct TreeList {
    shared: Arc<Shared>,
    categories: CategoryMap,
    view: Option<Arc<View>>,
    expanded: HashSet<EntryId>,
    rows: Vec<Row>,
    sort: Sort,
    /// The Index compared with a Snapshot, while the Change column shows.
    comparison: Option<Arc<Comparison>>,
    /// The removed folders the rows list, which `Row::gone` points into.
    gone: Vec<Removed>,
    /// Rows drawn recently; see `FlatList::rows`. Dropped when the rows change.
    cells: HashMap<usize, Cells>,
    pub selection: Selection,
    /// Builds the right-click menu; set by the Shell, which owns what the items act on.
    pub menu: Option<crate::entry_actions::MenuFn>,
    /// Width the table has to fit in; 0 until first measured.
    available: f32,
    /// A new Query opened its focus; the Explorer has yet to scroll to it.
    scroll_to_focus: bool,
}

impl TreeList {
    pub fn new(shared: Arc<Shared>, categories: CategoryMap) -> Self {
        Self {
            shared,
            categories,
            view: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            sort: Sort {
                on_disk: true,
                change: false,
                descending: true,
            },
            comparison: None,
            gone: Vec::new(),
            cells: HashMap::new(),
            selection: Selection::default(),
            menu: None,
            available: 0.,
            scroll_to_focus: false,
        }
    }

    /// The size measure siblings are ordered by, for the Explorer's summary line.
    pub fn sort_label(&self) -> &'static str {
        match (self.sort.change, self.sort.on_disk) {
            (true, _) => "Change",
            (false, true) => "On-disk Size",
            (false, false) => "Size",
        }
    }

    /// Adds the Change column for `comparison`, or drops it for `None`.
    pub fn set_comparison(&mut self, comparison: Option<Arc<Comparison>>) {
        if comparison.is_none() && self.sort.change {
            self.sort.change = false;
        }
        self.comparison = comparison;
        self.rebuild();
    }

    pub fn entry_at(&self, row: usize) -> Option<EntryId> {
        self.rows.get(row).filter(|r| r.gone.is_none()).map(|r| r.id)
    }

    /// Arrow-key move of the selection; the row now focused.
    pub fn step(&mut self, down: bool, extend: bool) -> Option<usize> {
        let rows = &self.rows;
        self.selection
            .step(down, extend, rows.len(), |r| rows.get(r).filter(|r| r.gone.is_none()).map(|r| r.id))
    }

    /// The selected Entries in the order the rows show them.
    pub fn selected(&self) -> Vec<EntryId> {
        self.selection.in_row_order(self.rows.len(), |r| self.entry_at(r))
    }

    /// Selects every visible row.
    pub fn select_all(&mut self) {
        let ids: Vec<Option<EntryId>> = (0..self.rows.len()).map(|r| self.entry_at(r)).collect();
        self.selection.select_all(ids.len(), |r| ids[r]);
    }

    /// Width the columns need as designed: Name's own width plus the fixed columns.
    pub fn natural_width(&self) -> f32 {
        COLUMNS[0].1 + FIXED_WIDTH + if self.comparison.is_some() { CHANGE_WIDTH } else { 0. }
    }

    /// Fits the table into `width`: Name shrinks first, the size columns stay. True when the
    /// columns changed.
    pub fn set_available(&mut self, width: f32) -> bool {
        if (width - self.available).abs() < 1. {
            return false;
        }
        self.available = width;
        true
    }

    /// Shows `view`; false when it already was the one shown. When the Query changed, its focus
    /// (the folder the Treemap draws) is opened with its ancestors, so the tree shows what the
    /// Treemap does; on the whole Index, each Location is opened down to what it shows. A View
    /// recomputed only because the Index grew leaves folders as they are.
    pub fn set_view(&mut self, view: Option<Arc<View>>) -> bool {
        if view::same(&self.view, &view) {
            return false;
        }
        let old_query = self.view.as_ref().map(|v| v.query.clone());
        self.view = view;
        if let Some(view) = self.view.clone()
            && old_query.is_some_and(|q| q != view.query)
        {
            match view.focus {
                Focus::Folder(focus) => {
                    self.open_to(focus);
                    self.expanded.insert(focus);
                    self.scroll_to_focus = true;
                }
                // With an empty Query every Location is shown: opening them all would bury the list.
                Focus::Index if !view.query.trim().is_empty() => {
                    let index = self.shared.index.read();
                    for (_, volume) in index.volumes() {
                        if index.is_live(volume.root) && view.filter.is_shown(volume.root) {
                            let folder = view::descend(&index, &view.filter, volume.root);
                            self.expanded.extend(std::iter::successors(Some(folder), |&p| index.parent(p)));
                        }
                    }
                }
                Focus::Index => {}
            }
        }
        self.rebuild();
        // The Entries the new View no longer shows fall out of the selection.
        if let Some(view) = self.view.clone() {
            self.selection.retain(|id| view.filter.is_shown(id));
        } else {
            self.selection.clear();
        }
        true
    }

    /// The folders opened in the tree, to open again in a reopened window.
    pub fn expanded(&self) -> &HashSet<EntryId> {
        &self.expanded
    }

    /// Opens the folders a closed window had open.
    pub fn set_expanded(&mut self, expanded: HashSet<EntryId>) {
        self.expanded = expanded;
        self.rebuild();
    }

    /// The row of the focus a new Query opened, once; the Explorer scrolls to it.
    pub fn take_focus_row(&mut self) -> Option<usize> {
        if !std::mem::take(&mut self.scroll_to_focus) {
            return None;
        }
        let focus = self.view.as_ref()?.focus.folder()?;
        self.rows.iter().position(|r| r.id == focus && r.gone.is_none())
    }

    /// Expands every folder above `id`.
    fn open_to(&mut self, id: EntryId) {
        let index = self.shared.index.read();
        self.expanded.extend(std::iter::successors(index.parent(id), |&p| index.parent(p)));
    }

    /// Opens the folders above `id` and selects its row; None when the View does not show it.
    pub fn reveal(&mut self, id: EntryId) -> Option<usize> {
        self.open_to(id);
        self.rebuild();
        let row = self.rows.iter().position(|r| r.id == id && r.gone.is_none())?;
        self.selection.only(id);
        Some(row)
    }

    /// Screenshots (VE_DEMO): opens the first folder and selects the row after it.
    #[cfg(feature = "bench")]
    pub fn demo(&mut self) {
        if let Some(id) = self.entry_at(0) {
            self.toggle(id);
            if let Some(next) = self.entry_at(2) {
                self.selection.only(next);
            }
        }
    }

    fn toggle(&mut self, id: EntryId) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.rebuild();
    }

    /// Lists the Volumes and, under every expanded folder, its children. Collapsed folders are
    /// never read.
    fn rebuild(&mut self) {
        self.rows.clear();
        self.cells.clear();
        self.gone.clear();
        let Some(view) = self.view.clone() else {
            return;
        };
        let shared = self.shared.clone();
        let index = shared.index.read();
        let roots: Vec<EntryId> = index.volumes().map(|(_, v)| v.root).collect();
        let mut stack = self.shown(&index, &view, None, roots, 0);
        stack.reverse();
        while let Some(row) = stack.pop() {
            self.rows.push(row);
            if row.gone.is_none() && self.expanded.contains(&row.id) {
                let children = index.children(row.id).to_vec();
                let mut children = self.shown(&index, &view, Some(row.id), children, row.depth + 1);
                children.reverse();
                stack.extend(children);
            }
        }
    }

    /// The Entries the Query leaves in the tree, in order, and with no Query the folders that
    /// the Snapshot had inside `parent` and are gone.
    fn shown(&mut self, index: &Index, view: &View, parent: Option<EntryId>, mut ids: Vec<EntryId>, depth: usize) -> Vec<Row> {
        ids.retain(|&id| index.is_live(id) && view.filter.is_shown(id));
        let mut keyed: Vec<(i128, Row)> = Vec::with_capacity(ids.len());
        let sort = self.sort;
        for id in ids {
            let key = if sort.change {
                self.comparison
                    .as_ref()
                    .and_then(|c| c.change(index, id))
                    .map_or(0, |c| c.on_disk as i128)
            } else if sort.on_disk {
                view.filter.totals(index, id).on_disk as i128
            } else {
                view.filter.totals(index, id).logical as i128
            };
            keyed.push((key, Row { id, depth, gone: None }));
        }
        if let (Some(comparison), true) = (self.comparison.clone(), view.query.trim().is_empty()) {
            let removed = match parent {
                Some(parent) => comparison.removed_in(parent),
                None => comparison.removed_roots(),
            };
            for r in removed {
                // Gone folders have no size now; they only rank when ordering by Change.
                let key = if sort.change { -(r.on_disk as i128) } else { 0 };
                keyed.push((
                    key,
                    Row {
                        id: parent.unwrap_or(EntryId(0)),
                        depth,
                        gone: Some(self.gone.len()),
                    },
                ));
                self.gone.push(r.clone());
            }
        }
        keyed.sort_by_key(|&(key, _)| key);
        if sort.descending {
            keyed.reverse();
        }
        keyed.into_iter().map(|(_, row)| row).collect()
    }

    /// What the Change column says of a folder.
    fn change_text(change: Change) -> String {
        let (sign, size) = if change.on_disk < 0 {
            ('−', change.on_disk.unsigned_abs())
        } else {
            ('+', change.on_disk as u64)
        };
        match (change.new, size) {
            (true, _) => format!("+{} new", format::size(size)),
            (false, 0) => String::new(),
            _ => format!("{sign}{}", format::size(size)),
        }
    }
}

impl TableDelegate for TreeList {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len() - usize::from(self.comparison.is_none())
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let (name, mut width, numeric) = COLUMNS[ix];
        if ix == 0 && self.available > 0. {
            let fixed = FIXED_WIDTH + if self.comparison.is_some() { CHANGE_WIDTH } else { 0. };
            width = (self.available - fixed).max(MIN_NAME_WIDTH);
        }
        let mut column = Column::new(name, name).width(width);
        if numeric {
            column = column.text_right();
        }
        match ix {
            1 | 2 | CHANGE => {
                let active = if ix == CHANGE {
                    self.sort.change
                } else {
                    !self.sort.change && self.sort.on_disk == (ix == 1)
                };
                column.sort(match (active, self.sort.descending) {
                    (false, _) => ColumnSort::Default,
                    (true, true) => ColumnSort::Descending,
                    (true, false) => ColumnSort::Ascending,
                })
            }
            _ => column,
        }
    }

    fn perform_sort(&mut self, col: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        self.sort = Sort {
            on_disk: col != 2,
            change: col == CHANGE,
            descending: sort != ColumnSort::Ascending,
        };
        self.rebuild();
    }

    fn context_menu(&mut self, _: usize, menu: PopupMenu, window: &mut Window, cx: &mut Context<TableState<Self>>) -> PopupMenu {
        match self.menu.clone() {
            Some(build) => build(menu, self.selected(), self.selection.last(), window, cx),
            None => menu,
        }
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let text = crate::theme::palette(cx).text;
        div()
            .size_full()
            .text_size(px(12.))
            .when(
                col_ix
                    == if self.sort.change {
                        CHANGE
                    } else if self.sort.on_disk {
                        1
                    } else {
                        2
                    },
                |d| d.text_color(text).font_weight(FontWeight::MEDIUM),
            )
            .child(self.column(col_ix, cx).name.clone())
    }

    fn render_last_empty_col(&mut self, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        div().w_0()
    }

    fn render_tr(&mut self, row: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        if self.cells.len() > CACHED_ROWS {
            self.cells.clear();
        }
        if let (false, Some(view)) = (self.cells.contains_key(&row), self.view.clone()) {
            let cells = (|| {
                let index = self.shared.index.read();
                let Row { id, depth, gone } = *self.rows.get(row)?;
                if let Some(gone) = gone {
                    let removed = self.gone.get(gone)?;
                    return Some(Cells {
                        depth,
                        name: format!("{} (removed)", removed.name),
                        expandable: false,
                        open: false,
                        muted: true,
                        category: Some(Category::Other),
                        share: 0.,
                        texts: [
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                            format!("−{}", format::size(removed.on_disk)),
                        ],
                    });
                }
                if !index.is_live(id) {
                    return None;
                }
                let totals = view.filter.totals(&index, id);
                let parent = match index.parent(id) {
                    Some(p) => view.filter.totals(&index, p).on_disk,
                    None => view.total.on_disk,
                };
                let share = if parent > 0 { totals.on_disk as f64 / parent as f64 } else { 1.0 };
                let is_dir = index.is_dir(id);
                Some(Cells {
                    depth,
                    name: index.display_name(id).to_owned(),
                    expandable: index.children(id).iter().any(|&c| view.filter.is_shown(c)),
                    open: self.expanded.contains(&id),
                    muted: !view.query.trim().is_empty() && !view.filter.is_matched(id),
                    category: match (is_dir, index.parent(id)) {
                        (_, None) => None,
                        (false, _) => Some(self.categories.of_name(index.name(id))),
                        (true, _) => Some(crate::treemap::dominant_category(&index, &view.filter, &self.categories, id)),
                    },
                    share: share as f32,
                    texts: [
                        format::size(totals.on_disk),
                        format::size(totals.logical),
                        format!("{:.1}%", share * 100.0),
                        if is_dir { format::count(totals.files) } else { String::new() },
                        format::time(index.modified(id)),
                        self.comparison
                            .as_ref()
                            .and_then(|c| c.change(&index, id))
                            .map(Self::change_text)
                            .unwrap_or_default(),
                    ],
                })
            })();
            if let Some(cells) = cells {
                self.cells.insert(row, cells);
            }
        }
        let selected = self.entry_at(row).is_some_and(|id| self.selection.contains(id));
        let is_last = selected && self.selection.last() == self.entry_at(row);
        div()
            .id(("row", row))
            .when(selected, |d| d.child(crate::selection::highlight(is_last, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |table, _, _, cx| {
                    let list = table.delegate_mut();
                    if let Some(id) = list.entry_at(row) {
                        if !list.selection.contains(id) {
                            list.selection.only(id);
                        }
                        list.selection.context = Some(id);
                        cx.notify();
                    }
                }),
            )
            .on_click(cx.listener(move |table, e: &ClickEvent, _, cx| {
                let list = table.delegate_mut();
                let ids: Vec<Option<EntryId>> = (0..list.rows.len()).map(|r| list.entry_at(r)).collect();
                list.selection
                    .click(row, e.modifiers(), ids.len(), |r| ids.get(r).copied().flatten());
                cx.notify();
            }))
    }

    fn render_td(&mut self, row: usize, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(c) = self.cells.get(&row) else {
            return div().into_any_element();
        };
        let p = palette(cx);
        let muted = p.muted;
        let root = c.category.is_none();
        let cell = h_flex()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(13.))
            .when(root, |d| d.font_weight(FontWeight::MEDIUM));
        if col == 3 {
            return cell
                .justify_end()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(36.))
                        .h(px(4.))
                        .flex_none()
                        .rounded(px(2.))
                        .bg(p.free)
                        .overflow_hidden()
                        .child(div().h_full().w(relative(c.share.clamp(0.0, 1.0))).bg(p.accent)),
                )
                .child(div().w(px(44.)).text_right().child(c.texts[2].clone()))
                .into_any_element();
        }
        if col > 0 {
            let text = &c.texts[col - 1];
            let cell = cell.justify_end();
            // Folders alone have a Files figure; a missing time reads as a dash.
            let blank = (col == 5 || col == CHANGE) && text.is_empty();
            return if blank {
                cell.text_color(muted).child("—")
            } else {
                cell.child(text.clone())
            }
            .into_any_element();
        }
        let id = self.rows[row].id;
        let chevron = if c.expandable {
            let icon = if c.open { IconName::ChevronDown } else { IconName::ChevronRight };
            div()
                .id(("toggle", row))
                .w(px(14.))
                .flex_shrink_0()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(Icon::new(icon).size(px(14.)).text_color(muted))
                .on_click(cx.listener(move |table, _, _, cx| {
                    table.delegate_mut().toggle(id);
                    table.refresh(cx);
                    cx.stop_propagation();
                }))
        } else {
            div().id(("toggle", row)).w(px(14.)).flex_shrink_0()
        };
        let mark = match c.category {
            None => Icon::new(IconName::Folder).size(px(14.)).text_color(muted).into_any_element(),
            Some(category) => swatch(p.category(category)).mx(px(3.)).into_any_element(),
        };
        let name = h_flex()
            .gap(px(6.))
            .min_w_0()
            .child(mark)
            .child(div().min_w_0().truncate().child(c.name.clone()));
        cell.pl(px(c.depth as f32 * INDENT))
            .gap(px(6.))
            .child(chevron)
            .child(if c.muted { name.text_color(muted) } else { name })
            .into_any_element()
    }
}
