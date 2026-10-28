//! The Explorer's tree: folders that expand on demand, each level sorted by Filtered Size.

use crate::engine::Shared;
use crate::format;
use crate::selection::Selection;
use crate::view::{self, View};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use ve_core::{EntryId, Index};

/// Rows kept in `TreeList::cells` before it starts over.
const CACHED_ROWS: usize = 512;
const INDENT: f32 = 16.;
/// Width of the columns after Name, which takes what is left of the list's width.
const FIXED_WIDTH: f32 = 110. + 110. + 70. + 90. + 140. + 16.;
const MIN_NAME_WIDTH: f32 = 160.;

const COLUMNS: [(&str, f32, bool); 6] = [
    ("Name", 360., false),
    ("On disk", 110., true),
    ("Size", 110., true),
    ("%", 70., true),
    ("Files", 90., true),
    ("Modified", 140., true),
];

#[derive(Clone, Copy)]
struct Row {
    id: EntryId,
    depth: usize,
}

/// Which size measure siblings are ordered by, and which way.
#[derive(Clone, Copy)]
struct Sort {
    on_disk: bool,
    descending: bool,
}

/// Everything one visible row shows, read under a single lock of the Index.
struct Cells {
    depth: usize,
    name: String,
    is_dir: bool,
    expandable: bool,
    open: bool,
    /// Shown only for what it contains, so drawn muted.
    muted: bool,
    texts: [String; 5],
}

pub struct TreeList {
    shared: Arc<Shared>,
    view: Option<Arc<View>>,
    expanded: HashSet<EntryId>,
    rows: Vec<Row>,
    sort: Sort,
    /// Rows drawn recently; see `FlatList::rows`. Dropped when the rows change.
    cells: HashMap<usize, Cells>,
    pub selection: Selection,
    /// Width the table has to fit in; 0 until first measured.
    available: f32,
}

impl TreeList {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            view: None,
            expanded: HashSet::new(),
            rows: Vec::new(),
            sort: Sort {
                on_disk: true,
                descending: true,
            },
            cells: HashMap::new(),
            selection: Selection::default(),
            available: 0.,
        }
    }

    pub fn entry_at(&self, row: usize) -> Option<EntryId> {
        self.rows.get(row).map(|r| r.id)
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

    /// Shows `view`; false when it already was the one shown.
    pub fn set_view(&mut self, view: Option<Arc<View>>) -> bool {
        if view::same(&self.view, &view) {
            return false;
        }
        self.view = view;
        self.rebuild();
        // The Entries the new View no longer shows fall out of the selection.
        if let Some(view) = self.view.clone() {
            self.selection.retain(|id| view.filter.is_shown(id));
        } else {
            self.selection.clear();
        }
        true
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
        let Some(view) = self.view.clone() else {
            return;
        };
        let index = self.shared.index.read();
        let roots: Vec<EntryId> = index.volumes().map(|(_, v)| v.root).collect();
        let mut stack = self.shown(&index, &view, roots, 0);
        stack.reverse();
        while let Some(row) = stack.pop() {
            self.rows.push(row);
            if self.expanded.contains(&row.id) {
                let children = index.children(row.id).to_vec();
                let mut children = self.shown(&index, &view, children, row.depth + 1);
                children.reverse();
                stack.extend(children);
            }
        }
    }

    /// The Entries the Query leaves in the tree, in order.
    fn shown(&self, index: &Index, view: &View, mut ids: Vec<EntryId>, depth: usize) -> Vec<Row> {
        ids.retain(|&id| index.is_live(id) && view.filter.is_shown(id));
        let totals = |id| view.filter.totals(index, id);
        if self.sort.on_disk {
            ids.sort_by_cached_key(|&id| totals(id).on_disk);
        } else {
            ids.sort_by_cached_key(|&id| totals(id).logical);
        }
        if self.sort.descending {
            ids.reverse();
        }
        ids.into_iter().map(|id| Row { id, depth }).collect()
    }
}

impl TableDelegate for TreeList {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let (name, mut width, numeric) = COLUMNS[ix];
        if ix == 0 && self.available > 0. {
            width = (self.available - FIXED_WIDTH).max(MIN_NAME_WIDTH);
        }
        let mut column = Column::new(name, name).width(width);
        if numeric {
            column = column.text_right();
        }
        match ix {
            1 | 2 => {
                let active = self.sort.on_disk == (ix == 1);
                column.sort(match (active, self.sort.descending) {
                    (false, _) => ColumnSort::Default,
                    (true, true) => ColumnSort::Descending,
                    (true, false) => ColumnSort::Ascending,
                })
            }
            _ => column,
        }
    }

    fn perform_sort(
        &mut self,
        col: usize,
        sort: ColumnSort,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        self.sort = Sort {
            on_disk: col != 2,
            descending: sort != ColumnSort::Ascending,
        };
        self.rebuild();
    }

    fn render_last_empty_col(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div().w_0()
    }

    fn render_tr(
        &mut self,
        row: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        if self.cells.len() > CACHED_ROWS {
            self.cells.clear();
        }
        if let (false, Some(view)) = (self.cells.contains_key(&row), self.view.clone()) {
            let cells = (|| {
                let index = self.shared.index.read();
                let Row { id, depth } = *self.rows.get(row)?;
                if !index.is_live(id) {
                    return None;
                }
                let totals = view.filter.totals(&index, id);
                let parent = match index.parent(id) {
                    Some(p) => view.filter.totals(&index, p).on_disk,
                    None => view.total.on_disk,
                };
                let share = if parent > 0 {
                    totals.on_disk as f64 / parent as f64
                } else {
                    1.0
                };
                let is_dir = index.is_dir(id);
                Some(Cells {
                    depth,
                    name: index.name(id).to_owned(),
                    is_dir,
                    expandable: index.children(id).iter().any(|&c| view.filter.is_shown(c)),
                    open: self.expanded.contains(&id),
                    muted: !view.query.trim().is_empty() && !view.filter.is_matched(id),
                    texts: [
                        format::size(totals.on_disk),
                        format::size(totals.logical),
                        format!("{:.1}%", share * 100.0),
                        if is_dir {
                            format::count(totals.files)
                        } else {
                            String::new()
                        },
                        format::time(index.modified(id)),
                    ],
                })
            })();
            if let Some(cells) = cells {
                self.cells.insert(row, cells);
            }
        }
        let selected = self.entry_at(row).is_some_and(|id| self.selection.contains(id));
        let accent = cx.theme().accent;
        div()
            .id(("row", row))
            .when(selected, |d| d.bg(accent))
            .on_click(cx.listener(move |table, e: &ClickEvent, _, cx| {
                let list = table.delegate_mut();
                let ids: Vec<EntryId> = list.rows.iter().map(|r| r.id).collect();
                list.selection.click(row, e.modifiers(), |r| ids.get(r).copied());
                cx.notify();
            }))
    }

    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(c) = self.cells.get(&row) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let cell = h_flex().size_full().overflow_hidden().whitespace_nowrap();
        if col > 0 {
            let cell = if col == 1 {
                cell
            } else {
                cell.text_color(theme.muted_foreground)
            };
            return cell
                .justify_end()
                .child(c.texts[col - 1].clone())
                .into_any_element();
        }
        let id = self.rows[row].id;
        let chevron = if c.expandable {
            let icon = if c.open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            };
            div()
                .id(("toggle", row))
                .w_4()
                .flex_shrink_0()
                .cursor_pointer()
                .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
                .on_click(cx.listener(move |table, _, _, cx| {
                    table.delegate_mut().toggle(id);
                    table.refresh(cx);
                    cx.stop_propagation();
                }))
        } else {
            div().id(("toggle", row)).w_4().flex_shrink_0()
        };
        let icon = if c.is_dir {
            IconName::Folder
        } else {
            IconName::File
        };
        let name = h_flex()
            .gap_1()
            .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
            .child(div().min_w_0().truncate().child(c.name.clone()));
        cell.pl(px(c.depth as f32 * INDENT))
            .child(chevron)
            .child(if c.muted {
                name.text_color(theme.muted_foreground)
            } else {
                name
            })
            .into_any_element()
    }
}
