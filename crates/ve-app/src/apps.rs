//! The Apps page: the installed programs (`apps.list`) with kind, version, install date and
//! On-disk Size from their Owner roots, or "—" when none of their folders is indexed, and the
//! Startup tab (`apps.startup`): what starts at login and the file that defines it. Programs
//! sort by name, kind, date and size; a click opens a program or startup item in the Inspector.

use crate::format;
use crate::inspector::{Item, Panel, long_fact};
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
use ve_engine::apps::{App as InstalledApp, StartupItem, StartupKind};

/// How often the lists are asked for again while the page shows, or until a first program or
/// startup item is known (the rail shows Apps only then). The Engine answers with the same
/// program list while the Index is unchanged, re-reads the sources only after Owner detection,
/// and otherwise redoes the sizes, which walks every program's folders under the Index lock:
/// hidden, the page skips it. Startup items are a few small files read afresh each time.
const REFRESH: Duration = Duration::from_secs(5);

/// What stands in for a value no source gives or no Index folder holds.
const UNKNOWN: &str = "—";

/// A click on a program: inspect it, by [`InstalledApp::key`].
pub struct InspectApp(pub String);

/// A click on a startup item: inspect it, by [`StartupItem::key`].
pub struct InspectStartup(pub String);

impl EventEmitter<InspectApp> for AppsPage {}
impl EventEmitter<InspectStartup> for AppsPage {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Installed,
    Startup,
}

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

/// (title, width): together as wide as the Installed columns, which fit beside the Inspector.
const STARTUP_COLUMNS: [(&str, f32); 5] = [("Name", 190.), ("Kind", 100.), ("Scope", 60.), ("Runs", 230.), ("Defined in", 210.)];

pub struct StartupList {
    items: Arc<Vec<StartupItem>>,
    /// Indices into `items`, by name.
    order: Vec<usize>,
    /// The item the Inspector shows, by key.
    selected: Option<String>,
    page: WeakEntity<AppsPage>,
}

impl StartupList {
    fn set_items(&mut self, items: Arc<Vec<StartupItem>>) {
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_cached_key(|&i| (items[i].name.to_lowercase(), items[i].key()));
        self.items = items;
        self.order = order;
    }

    fn item_at(&self, row: usize) -> Option<&StartupItem> {
        self.order.get(row).map(|&i| &self.items[i])
    }
}

impl TableDelegate for StartupList {
    fn columns_count(&self, _: &App) -> usize {
        STARTUP_COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let (name, width) = STARTUP_COLUMNS[ix];
        Column::new(name, name).width(width)
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
        let key = self.item_at(row).map(StartupItem::key);
        let selected = key.is_some() && key == self.selected;
        div()
            .id(("startup-row", row))
            .when(selected, |d| d.child(crate::selection::highlight(true, cx)))
            .on_click(cx.listener(move |table, _: &ClickEvent, _, cx| {
                let list = table.delegate_mut();
                let Some(key) = list.item_at(row).map(StartupItem::key) else {
                    return;
                };
                list.selected = Some(key.clone());
                let _ = list.page.update(cx, |_, cx| cx.emit(InspectStartup(key)));
                cx.notify();
            }))
    }

    fn render_td(&mut self, row: usize, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(item) = self.item_at(row) else {
            return div().into_any_element();
        };
        let p = palette(cx);
        let (text, color) = match col {
            0 => (item.name.clone(), p.text),
            1 => (item.kind.name().to_owned(), p.muted),
            2 => (item.scope.name().to_owned(), p.muted),
            3 => match &item.command {
                Some(command) => (command.clone(), p.text),
                None => (UNKNOWN.to_owned(), p.muted),
            },
            _ => (item.source.display().to_string(), p.muted),
        };
        h_flex()
            .size_full()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(px(FS_BODY))
            .text_color(color)
            .child(div().min_w_0().truncate().child(text))
            .into_any_element()
    }
}

pub struct AppsPage {
    table: Entity<TableState<AppList>>,
    startup: Entity<TableState<StartupList>>,
    tab: Tab,
    engine: Arc<Engine>,
    /// Whether the first lists have arrived.
    loaded: bool,
    /// Whether the Shell shows this page.
    shown: bool,
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
                    page: page.clone(),
                },
                window,
                cx,
            )
            .col_resizable(false)
        });
        let startup = cx.new(|cx| {
            TableState::new(
                StartupList {
                    items: Arc::new(Vec::new()),
                    order: Vec::new(),
                    selected: None,
                    page,
                },
                window,
                cx,
            )
            .col_resizable(false)
        });
        let refresh = cx.spawn(async move |this, cx| {
            loop {
                let Ok(wanted) = this.update(cx, |page, cx| page.shown || !page.has_apps(cx)) else {
                    return;
                };
                if wanted && !Self::fetch(this.clone(), cx).await {
                    return;
                }
                cx.background_executor().timer(REFRESH).await;
            }
        });
        Self {
            table,
            startup,
            tab: Tab::Installed,
            engine,
            loaded: false,
            shown: false,
            _refresh: refresh,
        }
    }

    /// Asks the Engine for the lists and shows them. The Engine works off the UI thread, and any
    /// wait for the Index lock happens there too. False once the page is gone.
    async fn fetch(this: WeakEntity<Self>, cx: &mut AsyncApp) -> bool {
        let Ok(engine) = this.read_with(cx, |page, _| page.engine.clone()) else {
            return false;
        };
        let (apps, startup) = cx
            .background_spawn(async move { (engine.apps_list(), engine.apps_startup()) })
            .await;
        this.update(cx, |page, cx| {
            page.set_startup(startup, cx);
            page.set_apps(apps, cx);
        })
        .is_ok()
    }

    /// Follows whether the Shell shows this page; on showing, the list is brought up to date
    /// at once.
    pub fn set_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        if shown && !self.shown {
            cx.spawn(async move |this, cx| Self::fetch(this, cx).await).detach();
        }
        self.shown = shown;
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

    /// Shows `items`; nothing is redrawn when they are the ones shown.
    fn set_startup(&mut self, items: Vec<StartupItem>, cx: &mut Context<Self>) {
        let changed = self.startup.update(cx, |t, cx| {
            let list = t.delegate_mut();
            if *list.items == items {
                return false;
            }
            list.set_items(Arc::new(items));
            cx.notify();
            true
        });
        if changed {
            cx.notify();
        }
    }

    /// Whether any installed program or startup item is known: the rail shows Apps only then.
    pub fn has_apps(&self, cx: &App) -> bool {
        !self.table.read(cx).delegate().apps.is_empty() || !self.startup.read(cx).delegate().items.is_empty()
    }

    /// The program with `key`, if still listed.
    pub fn app(&self, key: &str, cx: &App) -> Option<InstalledApp> {
        self.table.read(cx).delegate().apps.iter().find(|a| a.key() == key).cloned()
    }

    /// The startup item with `key`, if still listed.
    pub fn startup_item(&self, key: &str, cx: &App) -> Option<StartupItem> {
        self.startup.read(cx).delegate().items.iter().find(|i| i.key() == key).cloned()
    }

    /// Follows what the Inspector shows: the row of the program or startup item it shows, if
    /// any, is marked.
    pub fn set_selected(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let list = t.delegate_mut();
            if list.selected != key {
                list.selected = key.clone();
                cx.notify();
            }
        });
        self.startup.update(cx, |t, cx| {
            let list = t.delegate_mut();
            if list.selected != key {
                list.selected = key;
                cx.notify();
            }
        });
    }

    fn facts(&self, cx: &App) -> String {
        match self.tab {
            Tab::Installed => {
                let apps = &self.table.read(cx).delegate().apps;
                let sized: Vec<u64> = apps.iter().filter_map(|a| a.size).collect();
                format!(
                    "{} installed · {} with folders in the Index, {} on disk",
                    format::count(apps.len() as u64),
                    format::count(sized.len() as u64),
                    format::size(sized.iter().sum())
                )
            }
            Tab::Startup => {
                let items = &self.startup.read(cx).delegate().items;
                let units = items.iter().filter(|i| i.kind == StartupKind::Systemd).count();
                let counted = |n: usize, one: &str, many: &str| format!("{} {}", format::count(n as u64), if n == 1 { one } else { many });
                format!(
                    "{} at login · {}, {}",
                    counted(items.len(), "starts", "start"),
                    counted(items.len() - units, "autostart entry", "autostart entries"),
                    counted(units, "systemd user unit", "systemd user units")
                )
            }
        }
    }
}

impl Render for AppsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let empty = !self.has_apps(cx);
        let facts = (!empty).then(|| self.facts(cx));
        let page = v_flex().size_full().gap(px(GAP)).child(title_row("Apps", facts, cx));
        if empty {
            return page.child(empty_state(
                if self.loaded {
                    "No installed programs or startup items found"
                } else {
                    "Reading installed programs…"
                },
                cx,
            ));
        }
        let me = cx.entity();
        let tabs = crate::ui::tabs(
            "apps-tab",
            &TABS.map(|(_, label)| label),
            TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0),
            cx,
            move |i, _, cx| {
                me.update(cx, |page, cx| {
                    page.tab = TABS[i].0;
                    cx.notify();
                })
            },
        );
        let (rows, none) = match self.tab {
            Tab::Installed => (self.table.read(cx).delegate().order.len(), "No installed programs found"),
            Tab::Startup => (self.startup.read(cx).delegate().order.len(), "Nothing starts at login"),
        };
        page.child(tabs).map(|d| {
            if rows == 0 {
                return d.child(empty_state(none, cx));
            }
            let body = crate::ui::panel(cx).flex_1().min_h_0().p(px(12.));
            d.child(match self.tab {
                Tab::Installed => body.child(data_table(&self.table)),
                Tab::Startup => body.child(data_table(&self.startup)),
            })
        })
    }
}

/// The page's tabs, in order.
const TABS: [(Tab, &str); 2] = [(Tab::Installed, "Installed"), (Tab::Startup, "Startup")];

/// A list of the page as a table, drawn the same way for both tabs.
fn data_table<D: TableDelegate>(state: &Entity<TableState<D>>) -> DataTable<D> {
    DataTable::new(state)
        .small()
        .bordered(false)
        .stripe(false)
        .header_row_height(px(30.))
}

/// One Inspector fact.
fn fact(label: impl Into<SharedString>, value: impl Into<SharedString>) -> (SharedString, SharedString) {
    (label.into(), value.into())
}

/// The Inspector's view of a startup item: what it runs and the file that defines it, wrapped
/// whole, with its scope, drop-ins and desktop limits.
pub fn startup_panel(item: &StartupItem) -> Panel {
    let mut facts = vec![
        long_fact("Runs", item.command.clone().unwrap_or_else(|| UNKNOWN.to_owned())),
        long_fact("Defined in", item.source.display().to_string()),
    ];
    facts.extend(item.drop_ins.iter().map(|p| long_fact("Drop-in", p.display().to_string())));
    facts.push(fact("Scope", item.scope.name().to_owned()));
    if !item.only_in.is_empty() {
        facts.push(fact("Only in", item.only_in.join(", ")));
    }
    if !item.not_in.is_empty() {
        facts.push(fact("Not in", item.not_in.join(", ")));
    }
    Panel {
        kind: "Startup item",
        tag: Some((item.kind.name().into(), Tone::Neutral)),
        title: item.name.clone().into(),
        subtitle: Some(item.id.clone().into()),
        facts,
        actions: Vec::new(),
    }
}

/// The Inspector's view of a program: kind, source, version, install date, On-disk Size and
/// each Owner root with its Folder Size. `show_in_browse` acts on the largest root and is
/// disabled when no folder of the program is indexed.
pub fn panel(app: &InstalledApp, show_in_browse: Item) -> Panel {
    let program = &app.program;
    let mut facts = vec![
        fact("Version", program.version.clone().unwrap_or_else(|| UNKNOWN.to_owned())),
        fact("Installed", installed(program.installed)),
        fact("On disk", size(app.size)),
    ];
    if app.roots.is_empty() {
        facts.push(fact("Owner roots", "None in the Index"));
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
                launch: None,
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
