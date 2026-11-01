//! The Apps page: the installed programs (`apps.list`) with kind, version, install date and
//! On-disk Size from their Owner roots, or "—" when none of their folders is indexed. Sorts by
//! name, kind, date and size; a click opens the program in the Inspector, with its Owner roots
//! and Show in Browse.

use crate::format;
use crate::inspector::{Item, Panel};
use crate::theme::{FS_BODY, FS_SMALL, GAP, palette};
use crate::ui::{Tone, empty_state, title_row};
use gpui_kit::component::h_flex;
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::{Sizable as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;
use ve_engine::Engine;
use ve_engine::apps::App as InstalledApp;

/// How often the list is asked for again. The Engine answers with the same list while the
/// Index is unchanged, re-reads the sources only after Owner detection, and otherwise only
/// redoes the sizes.
const REFRESH: Duration = Duration::from_secs(5);

/// What stands in for a value no source gives or no Index folder holds.
const UNKNOWN: &str = "—";

/// A click on a program: inspect it, by [`InstalledApp::key`].
pub struct InspectApp(pub String);

impl EventEmitter<InspectApp> for AppsPage {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Kind,
    Installed,
    Size,
}

/// (title, width, sort, right-aligned)
const COLUMNS: [(&str, f32, Option<SortBy>, bool); 5] = [
    ("Name", 300., Some(SortBy::Name), false),
    ("Kind", 90., Some(SortBy::Kind), false),
    ("Version", 170., None, false),
    ("Installed", 140., Some(SortBy::Installed), false),
    ("On disk", 90., Some(SortBy::Size), true),
];

pub struct AppList {
    apps: Arc<Vec<InstalledApp>>,
    /// Indices into `apps`, in the order shown.
    order: Vec<usize>,
    sort: SortBy,
    descending: bool,
    /// The program the Inspector shows, by key.
    selected: Option<String>,
    page: WeakEntity<AppsPage>,
}

/// Unknown values sort after known ones whichever the direction.
fn known_first<T: Ord>(a: Option<T>, b: Option<T>, descending: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) if descending => b.cmp(&a),
        (Some(a), Some(b)) => a.cmp(&b),
        (a, b) => b.is_some().cmp(&a.is_some()),
    }
}

impl AppList {
    fn sorted(&mut self) {
        let apps = &self.apps;
        let (sort, descending) = (self.sort, self.descending);
        let by_name = |a: &InstalledApp, b: &InstalledApp| {
            a.program
                .name
                .to_lowercase()
                .cmp(&b.program.name.to_lowercase())
                .then_with(|| a.key().cmp(&b.key()))
        };
        self.order = (0..apps.len()).collect();
        self.order.sort_by(|&a, &b| {
            let (a, b) = (&apps[a], &apps[b]);
            match sort {
                SortBy::Name if descending => by_name(b, a),
                SortBy::Name => by_name(a, b),
                SortBy::Kind => {
                    let kind = a.program.kind.name().cmp(b.program.kind.name());
                    (if descending { kind.reverse() } else { kind }).then_with(|| known_first(a.size, b.size, true))
                }
                SortBy::Installed => known_first(a.program.installed, b.program.installed, descending),
                SortBy::Size => known_first(a.size, b.size, descending),
            }
            .then_with(|| by_name(a, b))
        });
    }

    fn app_at(&self, row: usize) -> Option<&InstalledApp> {
        self.order.get(row).map(|&i| &self.apps[i])
    }
}

/// An install date in local time; "—" when the source gives none.
fn installed(secs: Option<i64>) -> String {
    match secs {
        Some(s) => format::time(s.saturating_mul(1_000_000_000)),
        None => UNKNOWN.to_owned(),
    }
}

fn size(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| UNKNOWN.to_owned(), format::size)
}

impl TableDelegate for AppList {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let (name, width, sort, right) = COLUMNS[ix];
        let mut column = Column::new(name, name).width(width);
        if right {
            column = column.text_right();
        }
        match sort {
            Some(by) => column.sort(match (by == self.sort, self.descending) {
                (false, _) => ColumnSort::Default,
                (true, true) => ColumnSort::Descending,
                (true, false) => ColumnSort::Ascending,
            }),
            None => column,
        }
    }

    fn perform_sort(&mut self, col: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        let Some(by) = COLUMNS[col].2 else {
            return;
        };
        self.sort = by;
        self.descending = sort != ColumnSort::Ascending;
        self.sorted();
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        div()
            .size_full()
            .text_size(px(FS_SMALL))
            .child(self.column(col_ix, cx).name.clone())
    }

    fn render_last_empty_col(&mut self, _: &mut Window, _: &mut Context<TableState<Self>>) -> impl IntoElement {
        div().w_0()
    }

    fn render_tr(&mut self, row: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let key = self.app_at(row).map(InstalledApp::key);
        let selected = key.is_some() && key == self.selected;
        div()
            .id(("app-row", row))
            .when(selected, |d| d.child(crate::selection::highlight(true, cx)))
            .on_click(cx.listener(move |table, _: &ClickEvent, _, cx| {
                let list = table.delegate_mut();
                let Some(key) = list.app_at(row).map(InstalledApp::key) else {
                    return;
                };
                list.selected = Some(key.clone());
                let _ = list.page.update(cx, |_, cx| cx.emit(InspectApp(key)));
                cx.notify();
            }))
    }

    fn render_td(&mut self, row: usize, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(app) = self.app_at(row) else {
            return div().into_any_element();
        };
        let p = palette(cx);
        let program = &app.program;
        let (text, color) = match col {
            0 => (program.name.clone(), p.text),
            1 => (program.kind.name().to_owned(), p.muted),
            2 => (program.version.clone().unwrap_or_else(|| UNKNOWN.to_owned()), p.muted),
            3 => (installed(program.installed), p.muted),
            _ => (size(app.size), if app.size.is_some() { p.text } else { p.muted }),
        };
        h_flex()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(FS_BODY))
            .text_color(color)
            .when(COLUMNS[col].3, |d| d.justify_end())
            .child(div().min_w_0().truncate().child(text))
            .into_any_element()
    }
}

pub struct AppsPage {
    table: Entity<TableState<AppList>>,
    /// Whether the first list has arrived.
    loaded: bool,
    _refresh: Task<()>,
}

impl AppsPage {
    pub fn new(engine: Arc<Engine>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let page = cx.weak_entity();
        let table = cx.new(|cx| {
            TableState::new(
                AppList {
                    apps: Arc::new(Vec::new()),
                    order: Vec::new(),
                    sort: SortBy::Size,
                    descending: true,
                    selected: None,
                    page,
                },
                window,
                cx,
            )
            .col_resizable(false)
        });
        // The Engine works off the UI thread, and any wait for the Index lock happens there too.
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                let engine = engine.clone();
                let apps = cx.background_spawn(async move { engine.apps_list() }).await;
                if this.update(cx, |page, cx| page.set_apps(apps, cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        });
        Self {
            table,
            loaded: false,
            _refresh: refresh,
        }
    }

    /// Shows `apps`; nothing is redrawn when the Engine answered with the list shown.
    fn set_apps(&mut self, apps: Arc<Vec<InstalledApp>>, cx: &mut Context<Self>) {
        let changed = self.table.update(cx, |t, cx| {
            let list = t.delegate_mut();
            if Arc::ptr_eq(&list.apps, &apps) {
                return false;
            }
            list.apps = apps;
            list.sorted();
            cx.notify();
            true
        });
        if changed || !self.loaded {
            self.loaded = true;
            cx.notify();
        }
    }

    /// Whether any installed program is known: the rail shows Apps only then.
    pub fn has_apps(&self, cx: &App) -> bool {
        !self.table.read(cx).delegate().apps.is_empty()
    }

    /// The program with `key`, if still listed.
    pub fn app(&self, key: &str, cx: &App) -> Option<InstalledApp> {
        self.table.read(cx).delegate().apps.iter().find(|a| a.key() == key).cloned()
    }

    /// Follows what the Inspector shows: the row of the program it shows, if any, is marked.
    pub fn set_selected(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let list = t.delegate_mut();
            if list.selected != key {
                list.selected = key;
                cx.notify();
            }
        });
    }

    fn facts(&self, cx: &App) -> String {
        let apps = &self.table.read(cx).delegate().apps;
        let sized: Vec<u64> = apps.iter().filter_map(|a| a.size).collect();
        format!(
            "{} installed · {} with folders in the Index, {} on disk",
            format::count(apps.len() as u64),
            format::count(sized.len() as u64),
            format::size(sized.iter().sum())
        )
    }
}

impl Render for AppsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = !self.has_apps(cx);
        let facts = (!empty).then(|| self.facts(cx));
        v_flex().size_full().gap(px(GAP)).child(title_row("Apps", facts, cx)).map(|d| {
            if empty {
                d.child(empty_state(
                    if self.loaded {
                        "No installed programs found"
                    } else {
                        "Reading installed programs…"
                    },
                    cx,
                ))
            } else {
                d.child(
                    crate::ui::panel(cx).flex_1().min_h_0().p(px(12.)).child(
                        DataTable::new(&self.table)
                            .small()
                            .bordered(false)
                            .stripe(false)
                            .header_row_height(px(30.)),
                    ),
                )
            }
        })
    }
}

/// The Inspector's view of a program: kind, source, version, install date, On-disk Size and
/// each Owner root with its Folder Size. `show_in_browse` acts on the largest root and is
/// disabled when no folder of the program is indexed.
pub fn panel(app: &InstalledApp, show_in_browse: Item) -> Panel {
    let program = &app.program;
    let fact = |label: String, value: String| (SharedString::from(label), SharedString::from(value));
    let mut facts = vec![
        fact("Version".into(), program.version.clone().unwrap_or_else(|| UNKNOWN.to_owned())),
        fact("Installed".into(), installed(program.installed)),
        fact("On disk".into(), size(app.size)),
    ];
    if app.roots.is_empty() {
        facts.push(fact("Owner roots".into(), "None in the Index".into()));
    }
    facts.extend(
        app.roots
            .iter()
            .map(|(path, bytes)| fact(format!("Root · {}", format::size(*bytes)), path.display().to_string())),
    );
    Panel {
        kind: "App",
        tag: Some((program.kind.name().into(), Tone::Neutral)),
        title: program.name.clone().into(),
        subtitle: Some(format!("{} · {}", program.source, program.id).into()),
        facts,
        actions: vec![show_in_browse.disabled(app.roots.is_empty().then_some("No folder of it is in the Index"))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ve_core::owners;
    use ve_engine::apps::{AppKind, Program};

    fn app(name: &str, kind: AppKind, installed: Option<i64>, size: Option<u64>) -> InstalledApp {
        InstalledApp {
            program: Program {
                name: name.into(),
                kind,
                source: "/opt",
                id: name.into(),
                version: None,
                installed,
                owner: owners::opt_owner(name),
            },
            roots: Vec::new(),
            size,
        }
    }

    fn names(sort: SortBy, descending: bool) -> Vec<String> {
        let mut list = AppList {
            apps: Arc::new(vec![
                app("vim", AppKind::Package, Some(30), Some(10)),
                app("Factorio", AppKind::Game, None, Some(500)),
                app("brave", AppKind::Program, Some(10), None),
                app("GIMP", AppKind::Flatpak, Some(20), Some(200)),
            ]),
            order: Vec::new(),
            sort,
            descending,
            selected: None,
            page: WeakEntity::new_invalid(),
        };
        list.sorted();
        list.order.iter().map(|&i| list.apps[i].program.name.clone()).collect()
    }

    #[::core::prelude::v1::test]
    fn sorts_by_each_column_with_unknown_values_last() {
        assert_eq!(names(SortBy::Size, true), ["Factorio", "GIMP", "vim", "brave"]);
        assert_eq!(names(SortBy::Size, false), ["vim", "GIMP", "Factorio", "brave"]);
        assert_eq!(names(SortBy::Name, false), ["brave", "Factorio", "GIMP", "vim"]);
        assert_eq!(names(SortBy::Kind, false), ["GIMP", "Factorio", "vim", "brave"]);
        assert_eq!(names(SortBy::Installed, true), ["vim", "GIMP", "brave", "Factorio"]);
        assert_eq!(names(SortBy::Installed, false), ["brave", "GIMP", "vim", "Factorio"]);
    }
}
