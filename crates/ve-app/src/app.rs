//! The window: the Module rail, the top bar with the Query, the pages (Overview, Browse,
//! Cleaner, Duplicates, Tasks, Settings), the Timeline dock, the status strip and the Inspector.

use crate::categories::CategoryPanel;
use crate::cleaner::{CleanerPage, ShowInExplorer};
use crate::compare::{Compare, CompareEvent};
use crate::dashboard::{Act, DashboardPage};
use crate::duplicates::{DuplicatesPanel, InspectGroup};
use crate::engine::{Engine, Phase, last_synced_label};
use crate::explorer::FlatList;
use crate::filters::FiltersPanel;
use crate::format;
use crate::host::{Attached, Host};
use crate::inspector::{self, Inspected, Item, Panel};
use crate::locations;
use crate::selection_panel::SelectionPanel;
use crate::settings::Settings;
#[cfg(feature = "updater")]
use crate::settings_page::CheckNow;
use crate::settings_page::{Changed, Forget, Refresh, SettingsPage};
use crate::tasks::Outcome;
use crate::theme::{CONTROL_H, FS_BODY, FS_SMALL, GAP, PAD, RADIUS_CARD, RADIUS_CONTROL, ROW_H, palette, tabular};
use crate::trash::{self, Trash};
use crate::tree::TreeList;
use crate::treemap::{Treemap, TreemapEvent};
use crate::ui::{
    ButtonKind, Tone, button, caption, dialog_title, dot, empty_state, group_label, nav_item, panel, section_title, tag, task_progress,
    title_row,
};
use crate::view::Focus;
use chrono::Local;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

/// The app's name as people read it (the binary, app id and data folders stay `view-everything`).
pub const APP_NAME: &str = "View Everything";

/// Most Entries "Open" and "Show in file manager" act on at once; more would launch a process
/// or window each.
const MAX_OPEN: usize = 10;

/// What the cached selected size was computed from: (tree shown, selection version, View id,
/// Index generation).
type SelectedSizeKey = (bool, u64, u64, u64);

/// The app icon the packages install, rasterized at exactly `size` points on a display of
/// `scale`. gpui rasterizes an `Image` SVG once, at the SVG's own size (here 256 px, doubled
/// by its smoothing factor), and the GPU then shrinks that bitmap to the 24-pt slot with
/// plain bilinear sampling, which at 1x and 2x discards most of the texels and blurs the
/// edges. Declaring the SVG's size as the device pixels it will cover (the viewBox scales
/// the drawing) makes the bitmap land on the pixel grid 1:1.
fn app_icon(size: f32, scale: f32) -> Arc<Image> {
    static CACHE: std::sync::Mutex<Vec<(u32, Arc<Image>)>> = std::sync::Mutex::new(Vec::new());
    const SVG: &str = include_str!("../../../packaging/icons/view-everything.svg");
    let device = (size * scale).round().max(1.) as u32;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, image)) = cache.iter().find(|(d, _)| *d == device) {
        return image.clone();
    }
    // gpui renders at twice the SVG's declared size (SMOOTH_SVG_SCALE_FACTOR) and treats the
    // bitmap as that many logical pixels, so declare half the device size.
    let declared = device as f32 / 2.;
    let from = "width=\"256\" height=\"256\"";
    debug_assert!(SVG.contains(from), "the app icon SVG no longer declares width and height 256");
    let sized = SVG.replacen(from, &format!("width=\"{declared}\" height=\"{declared}\""), 1);
    let image = Arc::new(Image::from_bytes(ImageFormat::Svg, sized.into_bytes()));
    cache.push((device, image.clone()));
    image
}

/// A Volume's label, or its mount point when it has none (a device path is no name).
pub fn volume_name(v: &VolumeInfo) -> String {
    if v.label.is_empty() || v.label.starts_with("/dev/") {
        v.mount.display().to_string()
    } else {
        v.label.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Browse,
    Cleaner,
    Duplicates,
    /// Tasks and Locations, from the rail's Work block.
    Work,
    Settings,
}

/// The self-update of the release in the sidebar row (ADR-0007).
#[cfg(feature = "updater")]
enum SelfUpdate {
    /// This install cannot update itself: the row links to the release.
    Link,
    Downloading,
    /// Downloaded and verified: "Restart to update".
    Ready(crate::update::Staged),
    Failed(String),
}

/// How long typing in the Query pauses before the engine is asked.
const QUERY_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// Width of the Filters column on Browse.
const FILTERS_WIDTH: f32 = 260.;

/// Least width of the Treemap beside the open Filters column.
const TREEMAP_MIN_WIDTH: f32 = 160.;

/// How the Explorer looked when its window closed to the tray; a reopened window starts there.
/// The Query itself is the Engine's ([`crate::engine::Shared::query`]).
pub struct Remembered {
    page: Page,
    show_tree: bool,
    compare: Option<chrono::NaiveDate>,
    expanded: HashSet<ve_core::EntryId>,
    tree_selection: crate::selection::Selection,
    list_selection: crate::selection::Selection,
}

pub struct Shell {
    engine: Arc<Engine>,
    page: Page,
    query: Entity<InputState>,
    /// The pending send of typed text to the engine; dropping it cancels the send.
    query_debounce: Option<gpui::Task<()>>,
    table: Entity<TableState<FlatList>>,
    tree: Entity<TableState<TreeList>>,
    /// The Snapshot the tree's Change column compares with.
    compare: Entity<Compare>,
    /// Whether the Explorer shows the tree rather than the flat list.
    show_tree: bool,
    duplicates: Entity<DuplicatesPanel>,
    filters: Entity<FiltersPanel>,
    show_filters: bool,
    /// The name field of the Saved Queries dialog, and the saved Query it is renaming.
    saved_name: Entity<InputState>,
    renaming_saved: Option<usize>,
    settings_page: Entity<SettingsPage>,
    dashboard: Entity<DashboardPage>,
    cleaner: Entity<CleanerPage>,
    trash: Trash,
    treemap: Entity<Treemap>,
    selection_panel: Entity<SelectionPanel>,
    categories: Entity<CategoryPanel>,
    category_map: CategoryMap,
    volumes: Vec<VolumeInfo>,
    /// Identities of the Volumes already offered for indexing this session.
    offered: HashSet<String>,
    startup_problems: Vec<String>,
    /// A newer release than the running app, as the last update check found.
    #[cfg(feature = "updater")]
    update: Option<crate::update::Release>,
    /// Where the self-update of `update` stands (ADR-0007).
    #[cfg(feature = "updater")]
    self_update: SelfUpdate,
    /// The Explorer lists' width in pixels, as of the last paint.
    list_width: Rc<Cell<f32>>,
    /// The selected size shown in the status bar, with what it was computed from: (tree shown,
    /// selection version, View id, Index generation).
    selected_size: Cell<Option<(SelectedSizeKey, u64)>>,
    /// What the Inspector shows.
    inspected: Option<Inspected>,
    /// The Entry a list selection last put in the Inspector.
    last_outlined: Option<ve_core::EntryId>,
    /// The pending repaint of the Tasks page's elapsed times.
    tick: Option<gpui::Task<()>>,
    #[cfg(feature = "bench")]
    demoed: bool,
}

impl Shell {
    pub fn new(settings: Settings, attached: Attached, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Attached {
            wake: woken,
            mounts: mounted,
            problems: startup_problems,
        } = attached;
        let (engine, categories, volumes, repaint) = {
            let host = cx.global::<Host>();
            (
                host.engine.clone(),
                host.categories.clone(),
                host.volumes.clone(),
                host.repaint.clone(),
            )
        };

        let trash = Trash::new(engine.shared.clone(), repaint.clone());

        let cleaner = cx.new(|cx| {
            CleanerPage::new(
                engine.shared.clone(),
                repaint.clone(),
                volumes.clone(),
                categories.clone(),
                &settings,
                cx,
            )
        });
        let dashboard = cx.new(|cx| {
            let mut page = DashboardPage::new(
                engine.shared.clone(),
                cleaner.clone(),
                trash.clone(),
                categories.clone(),
                &settings,
                repaint.clone(),
                window,
                cx,
            );
            page.set_context(&settings, volumes.clone(), cx);
            page
        });
        let duplicates = cx.new(|_| DuplicatesPanel::new(engine.shared.clone()));
        cx.subscribe(&duplicates, |this, _, group: &InspectGroup, cx| {
            this.inspected = Some(Inspected::Duplicates(group.0));
            cx.notify();
        })
        .detach();
        let treemap_depth = settings.treemap_depth;
        let treemap_colour_by = settings.treemap_colour_by;
        let settings_page = cx.new(|cx| SettingsPage::new(settings, volumes.clone(), window, cx));
        cx.subscribe_in(&dashboard, window, |this, _, act: &Act, window, cx| {
            this.dashboard_act(act, window, cx)
        })
        .detach();
        cx.subscribe_in(&cleaner, window, |this, _, show: &ShowInExplorer, window, cx| {
            this.dashboard_act(&Act::Explore(show.0.clone().or(Some(String::new()))), window, cx);
            cx.notify();
        })
        .detach();
        cx.subscribe(&settings_page, |this, _, changed: &Changed, cx| {
            this.engine
                .set_volumes(changed.mounted.clone(), changed.indexed.clone(), changed.remotes.clone());
            this.engine.set_intervals(changed.intervals.clone());
            let paused = cx.global::<Host>().live_paused();
            // A login start holds Live updates back until the user starts them.
            if !paused {
                this.engine.set_live_helper(changed.live_helper);
            }
            cx.global_mut::<Host>().live_setting_changed(changed.live_helper);
            let keep_in_tray = this.settings_page.read(cx).settings().keep_in_tray;
            cx.global_mut::<Host>().set_keep_in_tray(keep_in_tray);
            let settings = this.settings_page.read(cx).settings().clone();
            this.engine.set_duplicate_exclusions(settings.duplicate_exclusions.clone());
            this.engine.set_remote_names(&settings.remote_names);
            this.dashboard
                .update(cx, |d, cx| d.set_context(&settings, changed.mounted.clone(), cx));
            this.cleaner.update(cx, |c, cx| c.set_settings(&settings, cx));
            // Saved fine, so the settings file read at startup has been replaced.
            if !this.settings_page.read(cx).has_problem() {
                this.startup_problems.retain(|p| !p.starts_with("Settings ignored"));
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&settings_page, |this, _, forget: &Forget, _| this.engine.forget(forget.0.clone()))
            .detach();
        cx.subscribe(&settings_page, |this, _, refresh: &Refresh, _| {
            this.engine.sync_location(refresh.0.clone())
        })
        .detach();

        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Find a file by name"));
        let filters = cx.new(|cx| FiltersPanel::new(window, cx));
        // A window reopened from the tray shows the Query the Engine is still answering.
        let text = engine.shared.query.lock().clone();
        if !text.is_empty() {
            query.update(cx, |q, cx| q.set_value(text.as_str(), window, cx));
            filters.update(cx, |f, cx| f.reload(&text, window, cx));
        }
        cx.subscribe_in(&query, window, |this, query, event: &InputEvent, window, cx| {
            // Typing (Change) waits for a pause; Enter, and the programmatic setters that emit
            // PressEnter after the silent set_value, send at once.
            let now = match event {
                InputEvent::Change => query.read(cx).value().is_empty(),
                InputEvent::PressEnter { .. } => true,
                _ => return,
            };
            let text = query.read(cx).value().to_string();
            this.filters.update(cx, |f, cx| f.reload(&text, window, cx));
            if now {
                this.query_debounce = None;
                this.engine.set_query(text);
            } else {
                this.query_debounce = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(QUERY_DEBOUNCE).await;
                    let _ = this.update(cx, |this, _| {
                        this.query_debounce = None;
                        this.engine.set_query(text);
                    });
                }));
            }
            // Searching from anywhere shows the results.
            if !matches!(this.page, Page::Browse | Page::Cleaner | Page::Duplicates) {
                this.page = Page::Browse;
            }
            cx.notify();
        })
        .detach();
        cx.subscribe_in(&filters, window, |this, _, text: &String, window, cx| {
            this.query.update(cx, |q, cx| q.set_value(text.as_str(), window, cx));
            this.query_debounce = None;
            this.engine.set_query(text.clone());
        })
        .detach();
        let list = FlatList::new(engine.shared.clone());
        let table = cx.new(|cx| {
            TableState::new(list, window, cx)
                .col_resizable(false)
                .select_on_modifier_click(false)
        });
        let treemap = cx.new(|cx| Treemap::new(engine.shared.clone(), categories.clone(), treemap_depth, treemap_colour_by, cx));
        let selection_panel = cx.new(|_| SelectionPanel::new(engine.shared.clone()));
        // A finished folder count repaints the Inspector.
        cx.observe(&selection_panel, |_, _, cx| cx.notify()).detach();
        let category_panel = cx.new(|_| CategoryPanel::new(Some(query.clone()), "All Locations · click a row to add type: to the Query"));

        let tree = cx.new(|cx| {
            TableState::new(TreeList::new(engine.shared.clone(), categories.clone()), window, cx)
                .col_resizable(false)
                .select_on_modifier_click(false)
        });

        let compare = cx.new(|_| Compare::new(engine.shared.clone()));
        cx.subscribe_in(&compare, window, |this, _, event: &CompareEvent, window, cx| {
            match event {
                CompareEvent::Result(result) => this.tree.update(cx, |t, cx| {
                    t.delegate_mut().set_comparison(result.clone());
                    t.refresh(cx);
                }),
                CompareEvent::Failed(why) => window.push_notification(Notification::error(why.clone()), cx),
            }
            cx.notify();
        })
        .detach();

        let menu: crate::entry_actions::MenuFn = {
            let shell = cx.weak_entity();
            Rc::new(move |menu, ids, last, _, cx| match shell.upgrade() {
                Some(this) => this
                    .read(cx)
                    .explorer_menu_items(&shell, ids, last)
                    .into_iter()
                    .fold(menu, |menu, item| menu.item(item)),
                None => menu,
            })
        };
        table.update(cx, |t, _| t.delegate_mut().menu = Some(menu.clone()));
        tree.update(cx, |t, _| t.delegate_mut().menu = Some(menu));

        // Keyboard moves select a single row; clicks go through the lists' own handlers. A
        // double-click on a folder zooms the Query into it, as on the Treemap.
        cx.subscribe_in(&table, window, |this, table, event: &TableEvent, window, cx| {
            match event {
                TableEvent::SelectRow(row) => table.update(cx, |t, _| {
                    if let Some(id) = t.delegate().entry_at(*row) {
                        t.delegate_mut().selection.only(id);
                    }
                }),
                TableEvent::ClearSelection => this.clear_selection(cx),
                TableEvent::DoubleClickedRow(row) => {
                    if let Some(id) = table.read(cx).delegate().entry_at(*row) {
                        this.scope_to_folder(id, window, cx);
                    }
                }
                _ => {}
            }
            cx.notify();
        })
        .detach();
        cx.subscribe_in(&tree, window, |this, tree, event: &TableEvent, window, cx| {
            match event {
                TableEvent::SelectRow(row) => tree.update(cx, |t, _| {
                    if let Some(id) = t.delegate().entry_at(*row) {
                        t.delegate_mut().selection.only(id);
                    }
                }),
                TableEvent::ClearSelection => this.clear_selection(cx),
                TableEvent::DoubleClickedRow(row) => {
                    if let Some(id) = tree.read(cx).delegate().entry_at(*row) {
                        this.scope_to_folder(id, window, cx);
                    }
                }
                _ => {}
            }
            cx.notify();
        })
        .detach();
        // A modifier-click changes the selection without the tables knowing; whatever the
        // shown list selects, the Treemap outlines.
        cx.observe(&table, |this, _, cx| {
            this.outline_selection(cx);
            cx.notify();
        })
        .detach();
        cx.observe(&tree, |this, _, cx| {
            this.outline_selection(cx);
            cx.notify();
        })
        .detach();
        cx.subscribe_in(&treemap, window, |this, _, event: &TreemapEvent, window, cx| match event {
            TreemapEvent::Select(id) => {
                this.inspected = Some(Inspected::Entry(*id));
                this.reveal(*id, cx)
            }
            TreemapEvent::Scope(folder) => this.scope(*folder, window, cx),
            TreemapEvent::Depth(depth) => {
                let depth = *depth;
                this.settings_page
                    .update(cx, |page, cx| page.apply(|s| s.treemap_depth = depth, cx));
            }
            TreemapEvent::ColourBy(colour_by) => {
                let colour_by = *colour_by;
                this.settings_page
                    .update(cx, |page, cx| page.apply(|s| s.treemap_colour_by = colour_by, cx));
            }
        })
        .detach();

        #[cfg(feature = "bench")]
        if std::env::var_os("VE_BENCH").is_some() {
            crate::bench::start(table.clone(), window);
        }

        // Screenshots: VE_PAGE picks the page, VE_OPEN_DIALOG opens the Add Remote dialog.
        #[cfg(feature = "bench")]
        let page = match std::env::var("VE_PAGE").as_deref() {
            Ok("settings") => Page::Settings,
            Ok("browse" | "explorer") => Page::Browse,
            Ok("cleaner") => Page::Cleaner,
            Ok("duplicates") => Page::Duplicates,
            Ok("work" | "tasks") => Page::Work,
            Ok("overview" | "dashboard") => Page::Overview,
            // The scroll measurement needs the Browse list on screen.
            _ if std::env::var_os("VE_BENCH").is_some() => Page::Browse,
            _ => Page::Overview,
        };
        #[cfg(not(feature = "bench"))]
        let page = Page::Overview;
        #[cfg(feature = "bench")]
        if std::env::var_os("VE_OPEN_DIALOG").is_some() {
            cx.defer_in(window, |this, window, cx| {
                this.settings_page
                    .clone()
                    .update(cx, |page, cx| page.open_remote_dialog(None, window, cx))
            });
        }

        cx.spawn(async move |this, cx| {
            while woken.recv().await.is_ok() {
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Volumes mounted while running: a returning Offline Volume catches up at once, a new
        // removable one is offered for indexing. The host follows the mounts; this window applies them.
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(now) = mounted.recv().await {
                if this.update_in(cx, |this, window, cx| this.mounts_changed(now, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // A newer version: once at startup if a day has passed, then hourly (the app may run for days).
        #[cfg(feature = "updater")]
        {
            crate::update::cleanup();
            cx.subscribe(&settings_page, |this, _, _: &CheckNow, cx| this.check_update(cx))
                .detach();
            cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.check_update_if_due(cx)).is_err() {
                        break;
                    }
                    cx.background_executor().timer(std::time::Duration::from_secs(3600)).await;
                }
            })
            .detach();
        }

        let mut shell = Self {
            engine,
            page,
            query,
            query_debounce: None,
            table,
            tree,
            compare,
            // The 10M-Entry measurement scrolls the flat list, so it must be the one on screen.
            show_tree: !cfg!(feature = "bench") || std::env::var_os("VE_BENCH").is_none(),
            filters,
            show_filters: false,
            saved_name: cx.new(|cx| InputState::new(window, cx).placeholder("Name for the current Query")),
            renaming_saved: None,
            settings_page,
            dashboard,
            cleaner,
            duplicates,
            trash,
            treemap,
            selection_panel,
            categories: category_panel,
            category_map: categories.clone(),
            volumes,
            offered: HashSet::new(),
            startup_problems,
            #[cfg(feature = "updater")]
            update: None,
            #[cfg(feature = "updater")]
            self_update: SelfUpdate::Link,
            list_width: Rc::default(),
            selected_size: Cell::new(None),
            inspected: None,
            last_outlined: None,
            tick: None,
            #[cfg(feature = "bench")]
            demoed: false,
        };
        if let Some(remembered) = cx.global_mut::<Host>().remembered.take() {
            shell.restore(remembered, cx);
        }
        cx.on_release(|this, cx| {
            // Text typed within the debounce has not reached the Engine yet; send it now.
            if this.query_debounce.take().is_some() {
                this.engine.set_query(this.query.read(cx).value().to_string());
            }
            if cx.has_global::<Host>() {
                let remembered = this.remember(cx);
                cx.global_mut::<Host>().remembered = Some(remembered);
            }
        })
        .detach();
        shell
    }

    /// What a reopened window needs to look like this one.
    fn remember(&self, cx: &App) -> Remembered {
        let tree = self.tree.read(cx).delegate();
        Remembered {
            page: self.page,
            show_tree: self.show_tree,
            compare: self.compare.read(cx).active(),
            expanded: tree.expanded().clone(),
            tree_selection: tree.selection.clone(),
            list_selection: self.table.read(cx).delegate().selection.clone(),
        }
    }

    /// Puts back how the Explorer looked when the previous window closed.
    fn restore(&mut self, r: Remembered, cx: &mut Context<Self>) {
        self.page = r.page;
        self.show_tree = r.show_tree;
        if r.compare.is_some() {
            self.compare.update(cx, |c, cx| c.choose(r.compare, cx));
        }
        self.tree.update(cx, |t, _| {
            let tree = t.delegate_mut();
            tree.set_expanded(r.expanded);
            tree.selection = r.tree_selection;
        });
        self.table.update(cx, |t, _| t.delegate_mut().selection = r.list_selection);
        self.outline_selection(cx);
    }

    #[cfg(feature = "updater")]
    fn check_update_if_due(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings_page.read(cx).settings();
        if settings.update_checks && crate::update::due(settings.last_update_check, chrono::Utc::now().timestamp()) {
            self.check_update(cx);
        }
    }

    /// Asks GitHub Releases for the latest version, off the UI thread.
    #[cfg(feature = "updater")]
    fn check_update(&mut self, cx: &mut Context<Self>) {
        let url = crate::update::latest_url();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move { crate::update::check(&url, crate::update::CURRENT) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let status = match &found {
                    Ok(Some(r)) => format!("Version {} is available", r.version),
                    Ok(None) => format!("Up to date ({})", crate::update::CURRENT),
                    Err(e) => format!("Could not check for updates: {e}"),
                };
                // A failed check keeps showing the row of an earlier one.
                if let Ok(release) = found {
                    // A staged or failed update belongs to the version it was for.
                    if this.update.as_ref().map(|r| &r.version) != release.as_ref().map(|r| &r.version) {
                        this.self_update = SelfUpdate::Link;
                    }
                    this.update = release;
                    this.start_download(cx);
                }
                let now = chrono::Utc::now().timestamp();
                this.settings_page.update(cx, |page, cx| page.update_checked(status, now, cx));
                cx.notify();
            });
        })
        .detach();
    }

    /// Downloads and verifies the update in the background when this install can update itself (a failure is
    /// not retried until a newer version is out);
    /// otherwise (or on failure) the row keeps the release link.
    #[cfg(feature = "updater")]
    fn start_download(&mut self, cx: &mut Context<Self>) {
        let (Some(release), SelfUpdate::Link) = (self.update.clone(), &self.self_update) else {
            return;
        };
        let Some(kind) = crate::update::Kind::detect() else { return };
        self.self_update = SelfUpdate::Downloading;
        cx.spawn(async move |this, cx| {
            let staged = cx.background_spawn(async move { crate::update::prepare(&kind, &release) }).await;
            let _ = this.update(cx, |this, cx| {
                this.self_update = match staged {
                    Ok(staged) => SelfUpdate::Ready(staged),
                    Err(e) => SelfUpdate::Failed(e),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// "Restart to update": swaps the verified update in, saves the Index and starts the new version.
    #[cfg(feature = "updater")]
    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let SelfUpdate::Ready(staged) = &self.self_update else { return };
        let engine = &self.engine;
        match crate::update::apply(staged, || engine.shutdown()) {
            Ok(()) => cx.quit(),
            Err(e) => {
                self.self_update = SelfUpdate::Failed(e);
                cx.notify();
            }
        }
    }

    /// The row above Settings when a newer version is out.
    #[cfg(feature = "updater")]
    fn update_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let release = self.update.as_ref()?;
        let p = palette(cx);
        let url = release.notes_url.clone();
        let action = match &self.self_update {
            SelfUpdate::Link => None,
            SelfUpdate::Downloading => Some(
                div()
                    .text_xs()
                    .text_color(p.muted)
                    .child("Downloading the update…")
                    .into_any_element(),
            ),
            SelfUpdate::Ready(_) => Some(
                div()
                    .id("restart-to-update")
                    .cursor_pointer()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(p.accent)
                    .child("Restart to update")
                    .on_click(cx.listener(|this, _, _, cx| this.restart_to_update(cx)))
                    .into_any_element(),
            ),
            SelfUpdate::Failed(e) => Some(
                div()
                    .text_xs()
                    .text_color(p.muted)
                    .child(format!("Could not update: {e}"))
                    .into_any_element(),
            ),
        };
        Some(
            v_flex()
                .mt(px(16.))
                .px(px(12.))
                .py(px(8.))
                .rounded(px(12.))
                .border_1()
                .border_color(p.border)
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!("Version {} is out", release.version)),
                )
                .child(
                    div()
                        .id("whats-new")
                        .cursor_pointer()
                        .text_xs()
                        .text_color(p.accent)
                        .child("What's new")
                        .on_click(move |_, _, _| {
                            let _ = opener::open_browser(&url);
                        }),
                )
                .children(action)
                .into_any_element(),
        )
    }

    #[cfg(not(feature = "updater"))]
    fn update_row(&self, _: &mut Context<Self>) -> Option<AnyElement> {
        None
    }

    /// The mounted Volumes changed: the Settings page re-applies them to the Engine, and each
    /// removable Volume never seen before is offered for indexing.
    fn mounts_changed(&mut self, now: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) {
        let before = std::mem::replace(&mut self.volumes, now.clone());
        self.cleaner.update(cx, |c, cx| c.set_volumes(now.clone(), cx));
        // An unmounted Volume's 'Index this?' no longer applies; a replug asks again.
        let gone: Vec<String> = self
            .offered
            .iter()
            .filter(|id| !now.iter().any(|v| &v.identity == *id))
            .cloned()
            .collect();
        for id in gone {
            window.remove_notification1::<Shell>(id.clone(), cx);
            self.offered.remove(&id);
        }
        let offers = {
            let index = self.engine.shared.index.read();
            self.settings_page
                .read(cx)
                .settings()
                .index_offers(&before, &now, |id| index.volume_with_identity(id).is_some())
        };
        self.settings_page.update(cx, |page, cx| page.set_mounted(now, window, cx));
        for volume in offers {
            if self.offered.insert(volume.identity.clone()) {
                self.offer_to_index(volume, window, cx);
            }
        }
        cx.notify();
    }

    /// A non-blocking notification: Index adds the Volume, Not now remembers the answer.
    fn offer_to_index(&mut self, volume: VolumeInfo, window: &mut Window, cx: &mut Context<Self>) {
        let shell = cx.entity().downgrade();
        let name = if volume.label.is_empty() {
            volume.mount.display().to_string()
        } else {
            volume.label.clone()
        };
        let note = Notification::info(format!("{name} was mounted. Index it so its files can be searched and sized?"))
            .title("Index this?")
            .id1::<Shell>(volume.identity.clone())
            .autohide(false)
            .content(move |_, _, cx| {
                let answer = |id: &'static str, label: &'static str, index_it: bool, cx: &mut Context<Notification>| {
                    let (shell, volume) = (shell.clone(), volume.clone());
                    let click = cx.listener(move |note, _, window, cx| {
                        shell.update(cx, |shell, cx| shell.answer_offer(&volume, index_it, cx)).ok();
                        note.dismiss(window, cx);
                    });
                    let b = button(id, label).small();
                    if index_it { b.kind(ButtonKind::Primary) } else { b }.build(cx, click)
                };
                h_flex()
                    .gap_2()
                    .mt_2()
                    .child(answer("index-this", "Index", true, cx))
                    .child(answer("not-now", "Not now", false, cx))
                    .into_any_element()
            });
        window.push_notification(note, cx);
    }

    fn answer_offer(&mut self, volume: &VolumeInfo, index_it: bool, cx: &mut Context<Self>) {
        self.settings_page.update(cx, |page, cx| {
            page.apply(|s| if index_it { s.set_indexed(volume, true) } else { s.decline(volume) }, cx)
        });
    }

    /// Zooms the Query into `id` if it is a folder; files have nothing to zoom into.
    fn scope_to_folder(&mut self, id: ve_core::EntryId, window: &mut Window, cx: &mut Context<Self>) {
        let is_dir = {
            let index = self.engine.shared.index.read();
            index.is_live(id) && index.is_dir(id)
        };
        if is_dir {
            self.scope(Some(id), window, cx);
        }
    }

    /// Puts `folder`'s scope clause in the Query in place of any other, or takes it out, so every
    /// Explorer panel follows and the rest of the Query still applies.
    fn scope(&mut self, folder: Option<ve_core::EntryId>, window: &mut Window, cx: &mut Context<Self>) {
        let term = folder.and_then(|id| crate::scope::term(&self.engine.shared.index.read(), id));
        let text = crate::scope::set(&self.query.read(cx).value(), term.as_deref());
        self.query.update(cx, |q, cx| {
            q.set_value(text.as_str(), window, cx);
            // set_value is silent; Change recomputes the View.
            cx.emit(InputEvent::PressEnter {
                secondary: false,
                shift: false,
            });
        });
    }

    /// Selects what was clicked on the Treemap in both lists, opening the tree down to it.
    fn reveal(&mut self, id: ve_core::EntryId, cx: &mut Context<Self>) {
        self.tree.update(cx, |t, cx| {
            let row = t.delegate_mut().reveal(id);
            t.refresh(cx);
            if let Some(row) = row {
                t.scroll_to_row(row, cx);
            }
        });
        self.table.update(cx, |t, cx| {
            if let Some(row) = t.delegate_mut().reveal(id) {
                t.scroll_to_row(row, cx);
            }
            cx.notify();
        });
    }

    /// Outlines on the Treemap what the shown list selects, the last-clicked Entry boldest, and
    /// shows that Entry in the Selection panel. A list that cannot show what the Treemap selected
    /// leaves its outline alone.
    fn outline_selection(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_entries(cx);
        let Some(last) = self.last_clicked(cx).or_else(|| ids.first().copied()) else {
            return;
        };
        let rest: Vec<_> = ids.into_iter().filter(|&id| id != last).collect();
        self.treemap.update(cx, |t, cx| {
            t.set_selected(Some(last), cx);
            t.set_also_selected(rest, cx);
        });
        // A new selection on Browse is what the Inspector follows; a refresh of the same one,
        // or a selection kept while another page is shown, leaves it alone.
        if self.page == Page::Browse && self.last_outlined != Some(last) {
            self.inspected = Some(Inspected::Entry(last));
        }
        self.last_outlined = Some(last);
    }

    /// The Entry last clicked in whichever list the Explorer shows.
    fn last_clicked(&self, cx: &App) -> Option<ve_core::EntryId> {
        if self.show_tree {
            self.tree.read(cx).delegate().selection.last()
        } else {
            self.table.read(cx).delegate().selection.last()
        }
    }

    /// Picks up what the Engine's threads changed since the last look.
    fn poll(&mut self, cx: &mut Context<Self>) {
        self.compare.update(cx, |c, cx| c.refresh_if_stale(cx));
        let latest = self.engine.shared.view.lock().clone();
        self.table.update(cx, |table, cx| {
            if table.delegate_mut().set_view(latest.clone()) {
                table.refresh(cx);
            }
        });
        self.tree.update(cx, |table, cx| {
            if table.delegate_mut().set_view(latest.clone()) {
                table.refresh(cx);
            }
            // A new Query opened the tree at the Treemap's folder; bring it into sight.
            if let Some(row) = table.delegate_mut().take_focus_row() {
                table.scroll_to_row(row, cx);
            }
        });
        #[cfg(feature = "bench")]
        if std::env::var_os("VE_DEMO").is_some() && !self.demoed && latest.as_ref().is_some_and(|v| !v.flat.is_empty()) {
            self.demoed = true;
            self.tree.update(cx, |t, cx| {
                t.delegate_mut().demo();
                t.refresh(cx);
                cx.notify();
            });
        }
        self.categories.update(cx, |panel, cx| panel.set_view(latest, cx));
        let offline: Vec<_> = self
            .engine
            .shared
            .index
            .read()
            .volumes()
            .filter(|(_, v)| !v.online)
            .map(|(_, v)| (v.identity.clone(), v.mount.clone()))
            .collect();
        let synced = self
            .engine
            .shared
            .index
            .read()
            .volumes()
            .filter(|(_, v)| v.is_remote())
            .map(|(_, v)| (v.identity.clone(), v.last_synced))
            .collect();
        let mounts = self.engine.shared.syncing_mounts();
        let syncing: std::collections::HashSet<String> = self
            .engine
            .shared
            .index
            .read()
            .volumes()
            .filter(|(_, v)| v.online && mounts.contains(&v.mount))
            .map(|(_, v)| v.identity.clone())
            .collect();
        self.settings_page.update(cx, |page, cx| {
            page.set_offline(offline, cx);
            page.set_remote_synced(synced, cx);
            page.set_syncing(syncing, cx);
        });
        self.dashboard.update(cx, |d, cx| d.poll(cx));
        cx.notify();
    }

    /// Replaces the Query's text; every Explorer panel follows.
    fn set_query_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query.update(cx, |q, cx| {
            q.set_value(text, window, cx);
            // set_value is silent; Change recomputes the View.
            cx.emit(InputEvent::PressEnter {
                secondary: false,
                shift: false,
            });
        });
    }

    /// The Saved Queries dialog: save the current Query under a name, and apply, rename or
    /// delete the saved ones.
    fn open_saved_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (shell, page, name_input, query) = (cx.entity(), self.settings_page.clone(), self.saved_name.clone(), self.query.clone());
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = palette(cx);
            let (muted, border, text) = (p.muted, p.border, p.text);
            let saved = page.read(cx).settings().saved_queries.clone();
            let renaming = shell.read(cx).renaming_saved.filter(|&i| i < saved.len());
            let done = {
                let (shell, page, name_input, query) = (shell.clone(), page.clone(), name_input.clone(), query.clone());
                move |window: &mut Window, cx: &mut App| {
                    let name = name_input.read(cx).value().to_string();
                    let current = query.read(cx).value().to_string();
                    let mut outcome = Ok(());
                    let renaming = shell.read(cx).renaming_saved;
                    page.update(cx, |page, cx| {
                        page.apply(
                            |s| {
                                outcome = match renaming {
                                    Some(i) => s.rename_query(i, &name),
                                    None => s.save_query(&name, &current),
                                }
                            },
                            cx,
                        )
                    });
                    match outcome {
                        Ok(()) => {
                            shell.update(cx, |s, _| s.renaming_saved = None);
                            name_input.update(cx, |i, cx| i.set_value("", window, cx));
                            window.refresh();
                        }
                        Err(why) => window.push_notification(Notification::error(why), cx),
                    }
                }
            };
            let rows = saved.iter().enumerate().map(|(i, q)| {
                let (apply, rename, delete) = (
                    {
                        let (shell, text) = (shell.clone(), q.query.clone());
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            shell.update(cx, |s, cx| s.set_query_text(&text, window, cx));
                            window.close_dialog(cx);
                        }
                    },
                    {
                        let (shell, name_input, name) = (shell.clone(), name_input.clone(), q.name.clone());
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            shell.update(cx, |s, _| s.renaming_saved = Some(i));
                            name_input.update(cx, |input, cx| input.set_value(name.as_str(), window, cx));
                            window.refresh();
                        }
                    },
                    {
                        let (shell, page) = (shell.clone(), page.clone());
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            shell.update(cx, |s, _| s.renaming_saved = None);
                            page.update(cx, |page, cx| page.apply(|s| s.delete_query(i), cx));
                            window.refresh();
                        }
                    },
                );
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .py(px(8.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .id(("saved-apply", i))
                            .flex_1()
                            .min_w_0()
                            .cursor_pointer()
                            .on_click(apply)
                            .child(div().font_weight(FontWeight::MEDIUM).truncate().child(q.name.clone()))
                            .child(div().text_xs().text_color(muted).truncate().child(q.query.clone())),
                    )
                    .child(
                        button(("saved-rename", i), "Rename")
                            .small()
                            .pressed(renaming == Some(i))
                            .build(cx, rename),
                    )
                    .child(
                        button(("saved-delete", i), "Delete")
                            .small()
                            .kind(ButtonKind::Danger)
                            .build(cx, delete),
                    )
            });
            let list = if saved.is_empty() {
                div().py(px(8.)).text_color(muted).child("Nothing saved yet.").into_any_element()
            } else {
                v_flex()
                    .id("saved-list")
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .children(rows)
                    .into_any_element()
            };
            let done_click = {
                let done = done.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| done(window, cx)
            };
            dialog
                .w(px(480.))
                .centered()
                .p(px(24.))
                .close_button(false)
                .title(dialog_title("Saved Queries", cx))
                .child(
                    v_flex()
                        .gap(px(12.))
                        .child(list)
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .items_center()
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .h(px(CONTROL_H))
                                        .px(px(6.))
                                        .items_center()
                                        .rounded(px(RADIUS_CONTROL))
                                        .border_1()
                                        .border_color(p.control)
                                        .bg(p.ground)
                                        .child(Input::new(&name_input).appearance(false).bordered(false)),
                                )
                                .child(
                                    button("saved-save", if renaming.is_some() { "Rename" } else { "Save current" })
                                        .kind(ButtonKind::Primary)
                                        .build(cx, done_click),
                                ),
                        )
                        .child(div().text_xs().text_color(text).child(if renaming.is_some() {
                            "Type the new name, then Rename."
                        } else {
                            "Saving under an existing name replaces that Query. Click a saved Query to apply it."
                        })),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .child(button("saved-close", "Close").build(cx, |_, window, cx| window.close_dialog(cx))),
                )
        });
    }

    /// What a click on the Overview asked for.
    fn dashboard_act(&mut self, act: &Act, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            Act::Explore(query) => {
                if let Some(text) = query {
                    self.set_query_text(text, window, cx);
                }
                self.page = Page::Browse;
            }
            Act::OpenCleaner => self.page = Page::Cleaner,
            Act::Inspect(what) => self.inspected = Some(what.clone()),
            Act::StartLive => Host::start_live(cx),
            Act::SaveQuery(text) => {
                self.settings_page
                    .update(cx, |page, cx| page.apply(|s| s.recent_query = text.clone(), cx));
            }
        }
        cx.notify();
    }

    /// The one-line bar under the Explorer: sync state, Last Synced, problems, totals, Trash.
    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (muted, text, border, accent, warn) = (p.muted, p.text, p.border, p.accent, p.warning);
        let shared = &self.engine.shared;
        let status = shared.status.lock().clone();
        let progress = &shared.progress;
        let sep = || div().w(px(1.)).h(px(14.)).flex_none().bg(border);
        let item = || h_flex().gap(px(8.)).items_center();
        let mut bar = h_flex()
            .flex_none()
            .h(px(32.))
            .bg(p.ground)
            .px(px(24.))
            .gap(px(16.))
            .items_center()
            .border_t_1()
            .border_color(border)
            .text_xs()
            .text_color(muted)
            .whitespace_nowrap()
            .overflow_hidden();
        match &status.phase {
            Phase::LoadingIndex => {
                bar = bar.child(item().child(dot(accent)).child("Loading the saved Index…"));
            }
            Phase::Syncing {
                volumes,
                catching_up,
                started,
            } => {
                let what = if *catching_up { "Catching up" } else { "Syncing" };
                let names: Vec<String> = {
                    let index = shared.index.read();
                    volumes
                        .iter()
                        .map(|m| {
                            let named = index.volumes().find(|(_, v)| v.mount == *m).and_then(|(_, v)| v.name.clone());
                            named.unwrap_or_else(|| m.display().to_string())
                        })
                        .collect()
                };
                bar = bar.child(item().min_w_0().child(dot(accent)).child(div().min_w_0().truncate().child(format!(
                    "{what} {} — {} Entries read, {}s",
                    names.join(", "),
                    format::count(progress.entries()),
                    started.elapsed().as_secs()
                ))));
            }
            Phase::Idle => {}
        }
        if shared.hashing_now() {
            let hashing = &shared.hash_progress;
            let (stage, done, total) = (
                hashing.stage.load(Ordering::Relaxed),
                hashing.done.load(Ordering::Relaxed),
                hashing.total.load(Ordering::Relaxed),
            );
            let what = if stage == 2 {
                "Hashing whole files"
            } else {
                "Comparing file starts and ends"
            };
            bar = bar
                .child(sep())
                .child(item().min_w_0().child(dot(accent)).child(div().min_w_0().truncate().child(format!(
                    "{what} for Duplicates — {} of {}",
                    format::count(done),
                    format::count(total)
                ))));
        }
        // One look at the Index per frame: three separate read locks queue behind any writer.
        let (oldest, offline, live) = {
            let index = shared.index.read();
            (
                index.oldest_synced(),
                index.volumes().filter(|(_, v)| !v.online).count(),
                index.live_count(),
            )
        };
        let synced = last_synced_label(oldest, chrono::Utc::now().timestamp());
        if let Some(label) = synced.filter(|_| !matches!(status.phase, Phase::LoadingIndex)) {
            bar = bar.child(sep()).child(item().child(label));
        }
        if offline > 0 {
            bar = bar.child(sep()).child(item().child(dot(warn)).child(format!("{offline} Offline")));
        }
        if progress.unreadable() > 0 {
            bar = bar
                .child(sep())
                .child(item().child(format!("{} folders unreadable", format::count(progress.unreadable()))));
        }
        if let Some(view) = shared.view.lock().as_ref().filter(|v| !v.query.trim().is_empty()) {
            bar = bar.child(sep()).child(item().child(format!(
                "{} matched in {} ms",
                format::count(view.flat.len() as u64),
                view.took.as_millis()
            )));
        }
        let problems = status
            .problems
            .into_iter()
            .map(|(_, message)| message)
            .chain(shared.query_error.lock().clone())
            .chain(self.startup_problems.iter().cloned());
        for problem in problems {
            bar = bar.child(sep()).child(div().min_w_0().truncate().text_color(warn).child(problem));
        }
        bar.child(div().flex_1().min_w_0())
            .child(
                item().flex_none().child(
                    h_flex()
                        .gap(px(4.))
                        .child(div().text_color(text).child(format::count(live as u64)))
                        .child("Entries"),
                ),
            )
            .child(sep())
            .child(div().flex_none().child(self.trash_controls(cx)))
    }

    fn trash_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (text, hover) = (p.text, p.active);
        let size = self.trash.size();
        let label = size.map_or_else(|| "Trash".to_owned(), |s| format!("Trash {}", format::size(s)));
        let mut controls = h_flex().gap(px(8.)).items_center().child(
            div()
                .id("open-trash")
                .cursor_pointer()
                .rounded(px(6.))
                .px(px(4.))
                .hover(move |s| s.bg(hover).text_color(text))
                .child(label)
                .tooltip(|window, cx| {
                    Tooltip::new("Trashed files still take up space until the Trash is emptied. Click to open the Trash.").build(window, cx)
                })
                .on_click(cx.listener(|this, _, window, cx| this.trash.open(window, cx))),
        );
        if trash::CAN_EMPTY {
            let emptying = self.trash.emptying();
            let empty = size == Some(0);
            controls = controls.child(
                h_flex()
                    .id("empty-trash")
                    .gap(px(6.))
                    .items_center()
                    .rounded(px(6.))
                    .px(px(4.))
                    .when(empty && !emptying, |d| d.opacity(0.5))
                    .when(!empty && !emptying, |d| {
                        d.cursor_pointer()
                            .hover(move |s| s.bg(hover).text_color(text))
                            .on_click(cx.listener(|this, _, window, cx| this.trash.ask_empty(window, cx)))
                    })
                    .when(emptying, |d| {
                        d.child(
                            gpui_kit::component::spinner::Spinner::new()
                                .with_size(gpui_kit::component::Size::XSmall)
                                .color(p.muted),
                        )
                    })
                    .child("Empty Trash"),
            );
        }
        controls
    }

    /// Writes what the Explorer shows (the flat list: every Entry matching the Query, which a
    /// zoomed folder is part of) to a CSV file.
    fn export_csv(&self, window: &mut Window, cx: &mut App) {
        let Some(view) = self.engine.shared.view.lock().clone() else {
            window.push_notification(Notification::warning("Nothing to export yet."), cx);
            return;
        };
        let (shared, categories) = (self.engine.shared.clone(), self.category_map.clone());
        crate::export::save(window, cx, "view-everything.csv", move |path| {
            let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
            ve_core::export::write_csv(&mut out, &shared.index.read(), &view.filter, &categories, &view.flat)?;
            std::io::Write::flush(&mut out)
        });
    }

    /// The Entries selected in whichever list the Explorer shows.
    fn selected_entries(&self, cx: &App) -> Vec<ve_core::EntryId> {
        if self.show_tree {
            self.tree.read(cx).delegate().selected()
        } else {
            self.table.read(cx).delegate().selected()
        }
    }

    /// The right-click menu of the tree and flat list. It acts on the selection, which a
    /// right-click outside it has already replaced with the clicked row.
    fn explorer_menu_items(
        &self,
        shell: &WeakEntity<Shell>,
        ids: Vec<ve_core::EntryId>,
        last: Option<ve_core::EntryId>,
    ) -> Vec<PopupMenuItem> {
        let Some(last) = last.filter(|id| ids.contains(id)).or_else(|| ids.first().copied()) else {
            return Vec::new();
        };
        let (can_sync, syncing, can_zoom_out) = {
            let index = self.engine.shared.index.read();
            if !index.is_live(last) {
                return Vec::new();
            }
            let folder = crate::engine::folder_of(&index, last);
            let root = self
                .engine
                .shared
                .view
                .lock()
                .clone()
                .map(|v| crate::treemap::zoom_root(&index, &v));
            (
                crate::engine::can_sync_folder(&index, folder),
                self.engine.shared.folder_syncing(folder),
                root.is_some_and(|r| r.folder().is_some()),
            )
        };
        let blocker = self.trash.blocker_of(&ids, &self.volumes);
        let many = ids.len() > 1;
        let too_many = ids.len() > MAX_OPEN;
        let item = |label: &str, act: MenuAct, disabled: bool| {
            let (shell, ids) = (shell.clone(), ids.clone());
            PopupMenuItem::new(label.to_owned())
                .disabled(disabled)
                .on_click(move |_, window, cx| {
                    shell.update(cx, |this, cx| this.menu_act(act, &ids, last, window, cx)).ok();
                })
        };
        let mut items = vec![
            item("Open", MenuAct::Open, too_many),
            item("Show in file manager", MenuAct::Reveal, too_many),
            item(
                if many {
                    "Open terminal at last clicked"
                } else {
                    "Open terminal here"
                },
                MenuAct::Terminal,
                false,
            ),
            item(if many { "Copy paths" } else { "Copy path" }, MenuAct::CopyPath, false),
            item("Copy name and size", MenuAct::CopyNameSize, false),
            PopupMenuItem::separator(),
            item(
                if many { "Zoom in last clicked" } else { "Zoom in Treemap" },
                MenuAct::ZoomIn,
                false,
            ),
            item("Zoom out Treemap", MenuAct::ZoomOut, !can_zoom_out),
            PopupMenuItem::separator(),
            item(
                if many { "Sync last clicked folder" } else { "Sync this folder" },
                MenuAct::Sync,
                !can_sync || syncing,
            ),
        ];
        if !can_sync {
            items.push(PopupMenuItem::label(crate::engine::WHOLE_ONLY));
        }
        if too_many {
            items.push(PopupMenuItem::label("Open and Show in file manager take up to 10 Entries"));
        }
        items.push(item(
            if many { "Export selection to CSV" } else { "Export to CSV" },
            MenuAct::Export,
            false,
        ));
        items.push(PopupMenuItem::separator());
        items.push(item("Move to Trash", MenuAct::Trash, blocker.is_some()));
        if let Some(why) = blocker {
            items.push(PopupMenuItem::label(why));
        }
        items
    }

    fn menu_act(&mut self, act: MenuAct, ids: &[ve_core::EntryId], last: ve_core::EntryId, window: &mut Window, cx: &mut Context<Self>) {
        let (paths, folder, zoom_out) = {
            let index = self.engine.shared.index.read();
            let live: Vec<_> = ids.iter().copied().filter(|&id| index.is_live(id)).collect();
            let paths: Vec<_> = live.iter().map(|&id| index.path(id)).collect();
            let folder = crate::entry_actions::terminal_folder(&index, last);
            // Out of the folder drawn: into its parent, or out to the whole Index from a Location.
            let zoom_out = self
                .engine
                .shared
                .view
                .lock()
                .clone()
                .and_then(|v| crate::treemap::zoom_root(&index, &v).folder())
                .map(|r| index.parent(r));
            (paths, folder, zoom_out)
        };
        let fail = |what: &str, path: &std::path::Path, e: &dyn std::fmt::Display, window: &mut Window, cx: &mut App| {
            window.push_notification(Notification::error(format!("Could not {what} {}: {e}", path.display())), cx);
        };
        match act {
            MenuAct::Open => {
                for path in &paths {
                    if let Err(e) = opener::open(path) {
                        fail("open", path, &e, window, cx);
                    }
                }
            }
            MenuAct::Reveal => {
                for path in &paths {
                    if let Err(e) = opener::reveal(path) {
                        fail("show", path, &e, window, cx);
                    }
                }
            }
            MenuAct::Terminal => {
                if let Some(dir) = folder
                    && let Err(e) = crate::entry_actions::open_terminal(&dir)
                {
                    window.push_notification(Notification::error(format!("Could not open a terminal: {e}")), cx);
                }
            }
            MenuAct::CopyPath => {
                let text = crate::entry_actions::paths_text(&self.engine.shared.index.read(), ids);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                window.push_notification(
                    Notification::info(if paths.len() > 1 {
                        format!("Copied {} paths", paths.len())
                    } else {
                        "Copied path".to_owned()
                    }),
                    cx,
                );
            }
            MenuAct::CopyNameSize => {
                let Some(view) = self.engine.shared.view.lock().clone() else {
                    return;
                };
                let text = crate::entry_actions::names_and_sizes_text(&self.engine.shared.index.read(), &view.filter, ids);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                window.push_notification(Notification::info("Copied name and size"), cx);
            }
            MenuAct::ZoomIn => {
                let folder = {
                    let index = self.engine.shared.index.read();
                    crate::engine::folder_of(&index, last)
                };
                self.scope(Some(folder), window, cx);
            }
            MenuAct::ZoomOut => {
                if let Some(parent) = zoom_out {
                    self.scope(parent, window, cx);
                }
            }
            MenuAct::Sync => {
                let folder = crate::engine::folder_of(&self.engine.shared.index.read(), last);
                self.engine.sync_folder(folder);
            }
            MenuAct::Export => {
                let Some(view) = self.engine.shared.view.lock().clone() else {
                    return;
                };
                let (shared, categories) = (self.engine.shared.clone(), self.category_map.clone());
                let mut ids = ids.to_vec();
                {
                    let index = shared.index.read();
                    ids.sort_by_key(|&id| std::cmp::Reverse(view.filter.totals(&index, id).on_disk));
                }
                crate::export::save(window, cx, "view-everything-selection.csv", move |path| {
                    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
                    ve_core::export::write_csv(&mut out, &shared.index.read(), &view.filter, &categories, &ids)?;
                    std::io::Write::flush(&mut out)
                });
            }
            MenuAct::Trash => self.ask_trash_selected(window, cx),
        }
    }

    /// Drops the selection of the shown list, and its outline on the Treemap and Selection panel.
    fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.show_tree {
            self.tree.update(cx, |t, _| t.delegate_mut().selection.clear());
        } else {
            self.table.update(cx, |t, _| t.delegate_mut().selection.clear());
        }
        self.treemap.update(cx, |t, cx| {
            t.set_selected(None, cx);
            t.set_also_selected(Vec::new(), cx);
        });
        if matches!(self.inspected, Some(Inspected::Entry(_))) {
            self.inspected = None;
        }
        self.last_outlined = None;
        cx.notify();
    }

    /// Selects every row of the shown list.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        if self.show_tree {
            self.tree.update(cx, |t, _| t.delegate_mut().select_all());
        } else {
            self.table.update(cx, |t, _| t.delegate_mut().select_all());
        }
        self.outline_selection(cx);
        cx.notify();
    }

    fn ask_trash_selected(&self, window: &mut Window, cx: &mut App) {
        let ids = self.selected_entries(cx);
        if !ids.is_empty() {
            self.trash.ask_trash(&ids, &self.volumes, window, cx);
        }
    }

    /// Lets the lists' name columns take the width the panel gives them.
    fn fit_lists(&self, cx: &mut Context<Self>) {
        // The measured box includes the list card's 12px side padding.
        let width = (self.list_width.get() - 24.).max(0.);
        self.table.update(cx, |t, cx| {
            if t.delegate_mut().set_available(width) {
                t.refresh(cx);
            }
        });
        self.tree.update(cx, |t, cx| {
            if t.delegate_mut().set_available(width) {
                t.refresh(cx);
            }
        });
    }

    /// A rail row that switches to `page`.
    fn nav(
        &self,
        id: &'static str,
        label: &'static str,
        icon: Option<&'static str>,
        page: Page,
        nested: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        nav_item(id, label, icon, self.page == page, nested, cx).on_click(cx.listener(move |this, _, _, cx| {
            // What was selected belongs to the page being left; the new page selects anew.
            if this.page != page {
                this.inspected = None;
            }
            this.page = page;
            cx.notify();
        }))
    }

    /// A clickable block of the rail's Work section.
    fn work_row(&self, id: &'static str, current: bool, cx: &App) -> Stateful<Div> {
        let p = palette(cx);
        v_flex()
            .id(id)
            .gap(px(4.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(RADIUS_CONTROL))
            .cursor_pointer()
            .text_size(px(FS_BODY))
            .when(current, |d| d.bg(p.active))
            .when(!current, |d| d.hover(move |s| s.bg(p.raised)))
    }

    /// The Work block: running Tasks (the first one's progress) and the Locations' health, as
    /// one button that opens the Work page.
    fn work_block(&self, rows: &[locations::Row], now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let running = self.engine.shared.tasks.running();
        let first = running.first().cloned();
        let tasks_title = if running.is_empty() {
            "Tasks".to_owned()
        } else {
            format!("Tasks · {} running", running.len())
        };
        let (health, tip) = locations::summary(rows, now);
        let colour = match health {
            locations::Health::Green => p.positive,
            locations::Health::Yellow => p.warning,
            locations::Health::Red => p.negative,
        };
        self.work_row("work", self.page == Page::Work, cx)
            .gap(px(6.))
            .child(v_flex().gap(px(4.)).child(div().child(tasks_title)).when_some(first, |d, t| {
                let unit = t.kind.unit();
                d.when(t.total.is_some() || !unit.is_empty(), |d| {
                    d.child(task_progress("rail-task-progress", t.done, t.total, unit, cx))
                })
                .child(div().truncate().child(caption(t.label.clone(), cx)))
            }))
            .child(
                v_flex()
                    .gap(px(4.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().flex_1().child(format!("Locations · {}", rows.len())))
                            .child(dot(colour)),
                    )
                    .child(caption(locations::rail_status(rows), cx)),
            )
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.page != Page::Work {
                    this.inspected = None;
                }
                this.page = Page::Work;
                cx.notify();
            }))
    }

    /// The Module rail: brand, Overview, Files (Browse, Cleaner, Duplicates), then Work, the
    /// update notice and Settings at the bottom.
    fn rail(&self, rows: &[locations::Row], now: i64, scale: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("Shell::rail");
        let p = palette(cx);
        v_flex()
            .w(px(240.))
            .h_full()
            .flex_none()
            .pt(px(20.))
            .px(px(12.))
            .pb(px(12.))
            .bg(p.ground)
            .border_r_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .h(px(32.))
                    .px(px(8.))
                    .gap(px(10.))
                    .items_center()
                    .text_size(px(FS_BODY))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(img(app_icon(24., scale)).size(px(24.)).flex_none())
                    .child(APP_NAME),
            )
            .child(
                self.nav(
                    "nav-overview",
                    "Overview",
                    Some("icons/layout-dashboard.svg"),
                    Page::Overview,
                    false,
                    cx,
                )
                .mt(px(12.)),
            )
            .child(group_label("Files", cx))
            .child(self.nav("nav-browse", "Browse", None, Page::Browse, true, cx))
            .child(self.nav("nav-cleaner", "Cleaner", None, Page::Cleaner, true, cx))
            .child(self.nav("nav-duplicates", "Duplicates", None, Page::Duplicates, true, cx))
            .child(div().flex_1().min_h_0())
            .child(self.work_block(rows, now, cx))
            .children(self.update_row(cx))
            .child(
                self.nav("nav-settings", "Settings", Some("icons/settings.svg"), Page::Settings, false, cx)
                    .mt(px(8.)),
            )
    }

    /// The Query: wide enough to read whole. `/` focuses it from any page.
    fn find_box(&self, cx: &mut Context<Self>) -> Div {
        let p = palette(cx);
        h_flex()
            .flex_1()
            .min_w_0()
            .max_w(px(720.))
            .h(px(CONTROL_H))
            .pl(px(8.))
            .pr(px(14.))
            .gap(px(8.))
            .items_center()
            .rounded(px(RADIUS_CONTROL))
            .border_1()
            .border_color(p.border)
            .bg(p.surface)
            // Capture, so Escape is ours before the input sees it: first drop the non-scope
            // terms, then (nothing but the scope left, or nothing) clear it all.
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" {
                    return;
                }
                let text = this.query.read(cx).value().to_string();
                if text.is_empty() {
                    return;
                }
                let kept = crate::scope::keep_scope(&text);
                this.set_query_text(if kept == text { "" } else { &kept }, window, cx);
                cx.stop_propagation();
            }))
            .child(tag("Files · Index", Tone::Neutral, cx).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.query).appearance(false).bordered(false)),
            )
            .when(!self.query.read(cx).value().is_empty(), |d| {
                d.child(
                    div()
                        .id("query-clear")
                        .cursor_pointer()
                        .tooltip(|window, cx| Tooltip::new("Clear the Query").build(window, cx))
                        .child(Icon::new(IconName::Close).small().text_color(p.muted))
                        .on_click(cx.listener(|this, _, window, cx| this.set_query_text("", window, cx))),
                )
            })
            .child(div().text_size(px(FS_SMALL)).text_color(p.muted).child("/"))
    }

    /// The top bar: the global Query box.
    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        h_flex()
            .h(px(48.))
            .flex_none()
            .px(px(PAD))
            .items_center()
            .bg(p.ground)
            .border_b_1()
            .border_color(p.border)
            .child(self.find_box(cx))
    }

    /// The Timeline dock: slim on every page; on Browse a days lane with a tick per Snapshot.
    fn dock(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (active, working, dates) = {
            let c = self.compare.read(cx);
            (c.active(), c.working(), c.dates().to_vec())
        };
        let browse = self.page == Page::Browse;
        let moment = active.map_or_else(|| "Moment: Live".to_owned(), |d| format!("At {d}"));
        let snapshots = match dates.first() {
            Some(latest) => format!("Snapshots {} · latest {latest}", dates.len()),
            None => "Snapshots 0".to_owned(),
        };
        let compare = self.compare.clone();
        let header = h_flex()
            .h(px(40.))
            .flex_none()
            .gap(px(16.))
            .items_center()
            .text_size(px(FS_BODY))
            .child(div().font_weight(FontWeight::MEDIUM).child("Timeline"))
            .child(div().text_color(if active.is_some() { p.text } else { p.muted }).child(moment))
            .child(caption(snapshots, cx))
            .child(div().flex_1())
            .when(working, |d| d.child(caption("Comparing…", cx)))
            .when(browse && active.is_some(), |d| {
                d.child(
                    button("back-to-live", "Back to Live")
                        .small()
                        .kind(ButtonKind::Ghost)
                        .build(cx, move |_, _, cx| compare.update(cx, |c, cx| c.choose(None, cx))),
                )
            });
        let lane = browse.then(|| {
            let Some(&oldest) = dates.last() else {
                return div()
                    .h(px(48.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(empty_state("No Snapshots yet: the first is taken once every Location has synced.", cx).flex_1())
                    .into_any_element();
            };
            let today = Local::now().date_naive().max(dates[0]);
            let span = (today - oldest).num_days().max(1) as f32;
            let ticks = dates.iter().enumerate().map(|(i, &date)| {
                let at = ((date - oldest).num_days() as f32 / span).clamp(0., 1.);
                let chosen = active == Some(date);
                let compare = self.compare.clone();
                div()
                    .id(("snapshot-tick", i))
                    .absolute()
                    .top(px(0.))
                    .left(relative(at))
                    .ml(px(-6.))
                    .w(px(12.))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .tooltip(move |window, cx| Tooltip::new(date.to_string()).build(window, cx))
                    .on_click(move |_, _, cx| compare.update(cx, |c, cx| c.choose(Some(date), cx)))
                    .child(
                        div()
                            .w(px(2.))
                            .h(px(if chosen { 24. } else { 18. }))
                            .bg(if chosen { p.accent } else { p.text }),
                    )
            });
            v_flex()
                .h(px(48.))
                .flex_none()
                .px(px(6.))
                .child(
                    div()
                        .relative()
                        .h(px(28.))
                        .child(div().absolute().top(px(14.)).left_0().right_0().h(px(1.)).bg(p.border))
                        .children(ticks),
                )
                .child(
                    h_flex()
                        .justify_between()
                        .child(caption(oldest.to_string(), cx))
                        .child(caption("Today", cx)),
                )
                .into_any_element()
        });
        v_flex()
            .flex_none()
            .px(px(PAD))
            .bg(p.ground)
            .border_t_1()
            .border_color(p.border)
            .child(header)
            .children(lane)
    }

    /// The Browse page: title and actions, Tree / Flat list, then the list with the Categories
    /// beside the Treemap.
    fn browse(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("Shell::browse");
        self.fit_lists(cx);
        let p = palette(cx);
        let (text, muted) = (p.text, p.muted);
        let (tree_sort, entries, total) = {
            let view = self.engine.shared.view.lock().clone();
            let sort = self.tree.read(cx).delegate().sort_label();
            (
                sort,
                view.as_ref().map_or(0, |v| v.flat.len() as u64),
                view.as_ref().map_or(0, |v| v.total.on_disk),
            )
        };
        let summary = format!(
            "{} Entries · Filtered Size {} · sorted by {}",
            format::count(entries),
            format::size(total),
            if self.show_tree { tree_sort } else { "On-disk Size" }
        );
        let (sel_version, sel_len) = {
            let s = if self.show_tree {
                &self.tree.read(cx).delegate().selection
            } else {
                &self.table.read(cx).delegate().selection
            };
            (s.version(), s.len())
        };
        let selected_size = if sel_len == 0 {
            0
        } else {
            let view = self.engine.shared.view.lock().clone();
            let index = self.engine.shared.index.read();
            match view {
                None => 0,
                Some(v) => {
                    // Recomputed only when the selection, the View or the Index changed.
                    let key = (self.show_tree, sel_version, v.id, index.generation());
                    match self.selected_size.get() {
                        Some((k, size)) if k == key => size,
                        _ => {
                            let selected = if self.show_tree {
                                self.tree.read(cx).delegate().selection.unordered()
                            } else {
                                self.table.read(cx).delegate().selection.unordered()
                            };
                            // A folder takes what is under it, and a hard-linked file counts once.
                            let chosen: std::collections::HashSet<_> = selected.iter().copied().collect();
                            let mut links = std::collections::HashSet::new();
                            let size = selected
                                .iter()
                                .filter(|&&id| index.is_live(id))
                                .filter(|&&id| !std::iter::successors(index.parent(id), |&p| index.parent(p)).any(|a| chosen.contains(&a)))
                                .filter(|&&id| !index.is_hardlinked(id) || links.insert(index.key(id)))
                                .map(|&id| v.filter.totals(&index, id).on_disk)
                                .sum::<u64>();
                            self.selected_size.set(Some((key, size)));
                            size
                        }
                    }
                }
            }
        };
        let width = self.list_width.clone();
        let measure = canvas(move |b, _, _| width.set(f32::from(b.size.width)), |_, _, _, _| {})
            .absolute()
            .size_full();
        let active = self.filters.read(cx).active();
        let label = if active > 0 {
            format!("Filters · {active}")
        } else {
            "Filters".into()
        };
        let filters = button("filters", label).icon("icons/filters.svg").pressed(self.show_filters).build(
            cx,
            cx.listener(|this, _, _, cx| {
                this.show_filters = !this.show_filters;
                cx.notify();
            }),
        );
        let saved_count = self.settings_page.read(cx).settings().saved_queries.len();
        let saved = button(
            "saved",
            if saved_count > 0 {
                format!("Saved · {saved_count}")
            } else {
                "Saved".into()
            },
        )
        .build(
            cx,
            cx.listener(|this, _, window, cx| {
                this.renaming_saved = None;
                this.open_saved_dialog(window, cx);
            }),
        );
        let export = button("export", "Export CSV").build(cx, cx.listener(|this, _, window, cx| this.export_csv(window, cx)));
        let selection = (sel_len > 0).then(|| {
            h_flex()
                .gap(px(4.))
                .text_size(px(FS_SMALL))
                .text_color(muted)
                .child(div().font_weight(FontWeight::MEDIUM).text_color(text).child(sel_len.to_string()))
                .child(format!("selected · {}", format::size(selected_size)))
        });
        let list = if self.show_tree {
            DataTable::new(&self.tree)
                .small()
                .bordered(false)
                .stripe(false)
                .header_row_height(px(30.))
                .into_any_element()
        } else {
            DataTable::new(&self.table)
                .small()
                .bordered(false)
                .stripe(false)
                .header_row_height(px(30.))
                .into_any_element()
        };
        // The list takes the width its columns need (plus its padding), shrinking Name on narrow
        // windows; the Treemap column takes everything else.
        let natural = if self.show_tree {
            self.tree.read(cx).delegate().natural_width()
        } else {
            FlatList::natural_width()
        } + 24.;
        let left = v_flex()
            // Wide enough for the Categories' columns, a little less beside the open Filters so
            // the Treemap keeps room; the Treemap takes what is left.
            .min_w(px(if self.show_filters { 400. } else { 448. }))
            .h_full()
            .gap(px(GAP))
            .flex_shrink(1.)
            .flex_grow(0.)
            .flex_basis(px(natural))
            .child(
                div()
                    .id("browse-list")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(p.surface)
                    .border_1()
                    .border_color(p.border)
                    .rounded(px(RADIUS_CARD))
                    .pt(px(12.))
                    .px(px(12.))
                    .pb(px(8.))
                    .child(measure)
                    .child(list),
            )
            .child(div().h(px(301.)).flex_none().child(self.categories.clone()));
        // Filters is a column beside the Treemap, never in its place: the Treemap stays in view and
        // re-lays out as the Query narrows. With Filters open the column asks for room for both, so
        // the list gives up width too instead of squeezing the Treemap to its least.
        let filters_column = self
            .show_filters
            .then(|| div().w(px(FILTERS_WIDTH)).h_full().flex_none().child(self.filters.clone()));
        let right = h_flex()
            .h_full()
            .flex_1()
            .gap(px(GAP))
            .map(|d| {
                if self.show_filters {
                    d.min_w(px(FILTERS_WIDTH + GAP + TREEMAP_MIN_WIDTH))
                        .flex_basis(px(FILTERS_WIDTH + GAP + 360.))
                } else {
                    d.min_w(px(360.))
                }
            })
            .children(filters_column)
            .child(div().flex_1().min_w(px(TREEMAP_MIN_WIDTH)).h_full().child(self.treemap.clone()));
        let me = cx.entity();
        let tabs = crate::ui::tabs(
            "browse-tab",
            &["Tree", "Flat list"],
            if self.show_tree { 0 } else { 1 },
            cx,
            move |i, _, cx| {
                me.update(cx, |this, cx| {
                    this.show_tree = i == 0;
                    cx.notify();
                })
            },
        );
        v_flex()
            .size_full()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let focus = if this.show_tree {
                    this.tree.focus_handle(cx)
                } else {
                    this.table.focus_handle(cx)
                };
                if !focus.is_focused(window) {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "delete" => this.ask_trash_selected(window, cx),
                    key @ ("up" | "down") if !event.keystroke.modifiers.control && !event.keystroke.modifiers.platform => {
                        let (down, extend) = (key == "down", event.keystroke.modifiers.shift);
                        // Our Selection, not the Table's own index, says where the focus is.
                        if this.show_tree {
                            this.tree.update(cx, |t, cx| {
                                let row = t.delegate_mut().step(down, extend);
                                if let Some(row) = row {
                                    t.scroll_to_row(row, cx);
                                }
                                cx.notify();
                            });
                        } else {
                            this.table.update(cx, |t, cx| {
                                if let Some(row) = t.delegate_mut().step(down, extend) {
                                    t.scroll_to_row(row, cx);
                                }
                                cx.notify();
                            });
                        }
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.clear_selection(cx);
                        cx.stop_propagation();
                    }
                    "a" if event.keystroke.modifiers.control || event.keystroke.modifiers.platform => {
                        this.select_all(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap(px(GAP))
                    .child(title_row("Browse", Some(summary), cx).min_w_0())
                    .child(
                        h_flex()
                            .flex_none()
                            .gap(px(8.))
                            .items_center()
                            .children(selection)
                            .child(saved)
                            .child(filters)
                            .child(export),
                    ),
            )
            .child(div().mt(px(GAP)).child(tabs))
            .child(h_flex().flex_1().min_h_0().gap(px(GAP)).pt(px(GAP)).child(left).child(right))
    }

    /// The Duplicates page: title with the search's facts, Rescan, and the groups; a group opens
    /// in the Inspector.
    fn duplicates_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.inspected {
            Some(Inspected::Duplicates(first)) => Some(first),
            _ => None,
        };
        let (facts, hashing) = self.duplicates.update(cx, |d, _| {
            d.selected = selected;
            d.facts()
        });
        let duplicates = self.duplicates.clone();
        v_flex()
            .size_full()
            .gap(px(GAP))
            .child(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap(px(GAP))
                    .child(title_row("Duplicates", Some(facts), cx).min_w_0())
                    .child(
                        button("dupes-rescan", "Rescan")
                            .busy(hashing)
                            .disabled(hashing.then_some("Hashing is running"))
                            .build(cx, move |_, _, cx| {
                                duplicates.read(cx).rescan();
                                cx.refresh_windows();
                            }),
                    ),
            )
            .child(crate::ui::panel(cx).flex_1().min_h_0().p(px(12.)).child(self.duplicates.clone()))
    }

    /// The Work page: running and recently finished Tasks beside every Location and its state;
    /// a click on either inspects it.
    fn work_page(&self, rows: &[locations::Row], now: i64, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let tasks = self.engine.shared.tasks.clone();
        let (running, finished) = (tasks.running_count(), tasks.finished().len());
        let shell = cx.weak_entity();
        let inspect: crate::tasks::Inspect = Rc::new(move |id, _, cx| {
            shell
                .update(cx, |s, cx| {
                    s.inspected = Some(Inspected::Task(id));
                    cx.notify();
                })
                .ok();
        });
        let selected_task = match &self.inspected {
            Some(Inspected::Task(id)) => Some(*id),
            _ => None,
        };
        let selected_location = match &self.inspected {
            Some(Inspected::Location(id)) => Some(id.as_str()),
            _ => None,
        };
        let offline = rows.iter().filter(|r| !r.online).count();
        let facts = format!(
            "{running} running · {finished} finished · {} Location{} · {offline} offline",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" }
        );
        let locations: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let identity = r.identity.clone();
                let current = selected_location == Some(r.identity.as_str());
                let state = r.state.text(now);
                let tone = match &r.state {
                    locations::State::Failed(_) => p.warning,
                    s if !s.healthy() => p.muted,
                    _ if r.syncing => p.text,
                    _ => p.muted,
                };
                h_flex()
                    .id(("work-location", i))
                    .h(px(ROW_H))
                    .px(px(8.))
                    .gap(px(12.))
                    .items_center()
                    .rounded(px(RADIUS_CONTROL))
                    .cursor_pointer()
                    .when(i > 0, |d| d.border_t_1().border_color(p.rule))
                    .when(current, |d| d.bg(p.active))
                    .when(!current, |d| d.hover(move |s| s.bg(p.raised)))
                    .child(div().flex_1().min_w_0().truncate().child(r.name.clone()))
                    .when(!r.online, |d| d.child(tag("Offline Volume", Tone::Warning, cx)))
                    .child(div().flex_none().text_color(tone).child(state))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.inspected = Some(Inspected::Location(identity.clone()));
                        cx.notify();
                    }))
            })
            .collect();
        v_flex().size_full().gap(px(GAP)).child(title_row("Work", Some(facts), cx)).child(
            h_flex()
                .flex_1()
                .min_h_0()
                .gap(px(GAP))
                .items_start()
                .child(
                    div()
                        .id("tasks-scroll")
                        .flex_grow(3.)
                        .flex_basis(relative(0.))
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .child(crate::tasks::list(&tasks, selected_task, inspect, cx)),
                )
                .child(
                    panel(cx)
                        .id("work-locations")
                        .flex_grow(2.)
                        .flex_basis(relative(0.))
                        .min_w(px(320.))
                        .max_h_full()
                        .overflow_y_scroll()
                        .child(section_title("Locations", cx).mb(px(8.)))
                        .children(locations),
                ),
        )
    }

    /// Forgets an Offline Volume: Settings drops its choices, the Engine drops its Entries.
    fn forget_location(&mut self, identity: &str, mount: &std::path::Path, cx: &mut Context<Self>) {
        self.settings_page
            .update(cx, |page, cx| page.apply(|s| s.forget(identity, mount), cx));
        self.engine.forget(identity.to_owned());
        self.inspected = None;
        cx.notify();
    }

    /// What the Inspector draws for what is inspected, or None when it is gone.
    fn inspector_panel(&mut self, what: &Inspected, rows: &[locations::Row], now: i64, cx: &mut Context<Self>) -> Option<Panel> {
        match what {
            Inspected::Entry(id) => self.entry_panel(*id, cx),
            Inspected::Location(identity) => self.location_panel(identity, rows, now, cx),
            Inspected::Task(id) => self.task_panel(*id, cx),
            Inspected::Duplicates(first) => self.duplicates_panel(*first, cx),
        }
    }

    /// A Duplicate group: size, copies and wasted space; Open, Show in Browse and Trash others…
    fn duplicates_panel(&mut self, first: ve_core::EntryId, cx: &mut Context<Self>) -> Option<Panel> {
        let group = self.duplicates.update(cx, |d, _| d.group(first))?;
        let (name, folder) = {
            let index = self.engine.shared.index.read();
            let kept = group.kept;
            (index.display_name(kept).to_owned(), index.parent(kept))
        };
        let kept = group.kept;
        let others = group.others();
        let blocker = self.trash.blocker_of(&others, &self.volumes);
        let fact = |l: &'static str, v: String| (SharedString::from(l), SharedString::from(v));
        Some(Panel {
            kind: "Duplicates",
            tag: Some((SharedString::from(format!("{} copies", group.copies.len())), Tone::Neutral)),
            title: name.into(),
            subtitle: Some("Identical content, confirmed by hash".into()),
            facts: vec![
                fact("Size each", format::size(group.size)),
                fact("Wasted", format::size(group.wasted())),
            ],
            actions: vec![
                Item::new(
                    "inspector-dupes-open",
                    "Open",
                    cx.listener(move |this, _, window, cx| this.menu_act(MenuAct::Open, &[kept], kept, window, cx)),
                ),
                Item::new(
                    "inspector-dupes-browse",
                    "Show in Browse",
                    cx.listener(move |this, _, window, cx| {
                        this.scope(folder, window, cx);
                        this.page = Page::Browse;
                        this.inspected = Some(Inspected::Entry(kept));
                        cx.notify();
                    }),
                ),
                Item::new(
                    "inspector-dupes-trash",
                    "Trash others…",
                    cx.listener(move |this, _, window, cx| this.trash.ask_trash(&others, &this.volumes, window, cx)),
                )
                .kind(ButtonKind::Danger)
                .disabled(blocker.as_deref()),
            ],
        })
    }

    /// The copies a Duplicate group lists in the Inspector, each a click away from being kept.
    fn inspector_members(&mut self, what: &Inspected, cx: &mut Context<Self>) -> Vec<inspector::Member> {
        let Inspected::Duplicates(first) = *what else {
            return Vec::new();
        };
        let Some(group) = self.duplicates.update(cx, |d, _| d.group(first)) else {
            return Vec::new();
        };
        let index = self.engine.shared.index.read();
        group
            .copies
            .iter()
            .map(|&id| inspector::Member {
                name: index.display_name(id).to_owned().into(),
                folder: index
                    .parent(id)
                    .map_or_else(String::new, |f| index.path(f).display().to_string())
                    .into(),
                keep: id == group.kept,
                on_click: Box::new(cx.listener(move |this, _, _, cx| {
                    this.duplicates.update(cx, |d, cx| {
                        d.set_keep(first, id);
                        cx.notify();
                    });
                    cx.notify();
                })),
            })
            .collect()
    }

    /// An Entry: size, share, contents and last write; Open, Show in file manager, Sync this
    /// folder and Move to Trash….
    fn entry_panel(&mut self, id: ve_core::EntryId, cx: &mut Context<Self>) -> Option<Panel> {
        let shared = self.engine.shared.clone();
        let view = shared.view.lock().clone()?;
        let (is_dir, totals, share, name, path, when, target, can_sync) = {
            let index = shared.index.read();
            if !index.is_live(id) {
                return None;
            }
            let totals = view.filter.totals(&index, id);
            // Share of what the Treemap draws; a selection outside it is measured against its Location.
            let drawn = crate::treemap::zoom_root(&index, &view);
            let inside = drawn
                .folder()
                .is_none_or(|root| std::iter::successors(Some(id), |&e| index.parent(e)).any(|e| e == root));
            let base = if inside { drawn } else { Focus::Folder(index.root_of(id)) };
            let whole = base.totals(&index, &view.filter).on_disk.max(1);
            let target = crate::engine::folder_of(&index, id);
            (
                index.is_dir(id),
                totals,
                format!("{:.1}% of {}", totals.on_disk as f64 * 100.0 / whole as f64, base.name(&index)),
                index.display_name(id).to_owned(),
                index.path(id).display().to_string(),
                format::time(index.modified(id)),
                target,
                crate::engine::can_sync_folder(&index, target),
            )
        };
        let contains = if is_dir {
            let folders = self.selection_panel.update(cx, |p, cx| p.folders_text(id, &view, cx));
            format!("{} files · {folders}", format::count(totals.files))
        } else {
            "1 file".to_owned()
        };
        let blocker = self.trash.blocker_of(&[id], &self.volumes);
        let sync_why = (!can_sync).then_some(crate::engine::WHOLE_ONLY);
        let fact = |l: &'static str, v: String| (SharedString::from(l), SharedString::from(v));
        Some(Panel {
            kind: "Entry",
            tag: Some((SharedString::from(if is_dir { "Folder" } else { "File" }), Tone::Neutral)),
            title: name.into(),
            subtitle: Some(path.into()),
            facts: vec![
                fact("On-disk Size", format::size(totals.on_disk)),
                fact("Share", share),
                fact("Contains", contains),
                fact("Last write", if when.is_empty() { "—".into() } else { when }),
            ],
            actions: vec![
                Item::new(
                    "inspector-open",
                    "Open",
                    cx.listener(move |this, _, window, cx| this.menu_act(MenuAct::Open, &[id], id, window, cx)),
                ),
                Item::new(
                    "inspector-reveal",
                    "Show in file manager",
                    cx.listener(move |this, _, window, cx| this.menu_act(MenuAct::Reveal, &[id], id, window, cx)),
                ),
                Item::new(
                    "inspector-sync",
                    "Sync this folder",
                    cx.listener(move |this, _, _, _| this.engine.sync_folder(target)),
                )
                .busy(shared.folder_syncing(target))
                .disabled(sync_why),
                Item::new(
                    "inspector-trash",
                    "Move to Trash…",
                    cx.listener(move |this, _, window, cx| this.trash.ask_trash(&[id], &this.volumes, window, cx)),
                )
                .kind(ButtonKind::Danger)
                .disabled(blocker.as_deref()),
            ],
        })
    }

    /// A Location: state, Used, Capacity, Files, Last Synced; Sync now, Open in Browse and, for
    /// an Offline Volume, Forget….
    fn location_panel(&mut self, identity: &str, rows: &[locations::Row], now: i64, cx: &mut Context<Self>) -> Option<Panel> {
        let row = rows.iter().find(|r| r.identity == identity)?;
        let (root, mount, remote, capacity, last_synced, subtitle) = {
            let index = self.engine.shared.index.read();
            let v = index.volume(index.volume_with_identity(identity)?)?;
            (
                v.root,
                v.mount.clone(),
                v.is_remote(),
                v.capacity,
                v.last_synced,
                v.remote().map_or_else(|| v.mount.display().to_string(), |r| r.label()),
            )
        };
        let dash = || SharedString::from("—");
        let files = self.dashboard.read(cx).files_on(identity);
        let (kind_tag, tone) = if remote {
            ("Remote", Tone::Neutral)
        } else if !row.online {
            ("Offline Volume", Tone::Warning)
        } else {
            ("Local Volume", Tone::Neutral)
        };
        let loading = self.engine.shared.is_loading();
        let why = if loading {
            Some("Loading the saved Index")
        } else if !row.online {
            Some("Offline")
        } else {
            None
        };
        let fact = |l: &'static str, v: Option<String>| (SharedString::from(l), v.map_or_else(dash, SharedString::from));
        let mut actions = vec![
            Item::new("inspector-sync-now", "Sync now", {
                let identity = identity.to_owned();
                cx.listener(move |this, _, _, _| this.engine.sync_location(identity.clone()))
            })
            .busy(row.syncing)
            .disabled(why),
            Item::new(
                "inspector-open-browse",
                "Open in Browse",
                cx.listener(move |this, _, window, cx| {
                    this.scope(Some(root), window, cx);
                    this.page = Page::Browse;
                    cx.notify();
                }),
            )
            .disabled(self.engine.shared.is_loading().then_some("Loading the saved Index")),
        ];
        if !row.online && !remote {
            let (identity, mount) = (identity.to_owned(), mount.clone());
            let shell = cx.entity();
            actions.push(
                Item::new("inspector-forget", "Forget…", move |_, window, cx| {
                    let (shell, identity, mount) = (shell.clone(), identity.clone(), mount.clone());
                    window.open_alert_dialog(cx, move |dialog, _, _| {
                        let (shell, identity, mount) = (shell.clone(), identity.clone(), mount.clone());
                        dialog
                            .confirm()
                            .title("Forget this Offline Volume?")
                            .description(
                                "Its Entries leave the Index and it is no longer searchable. Plugging it in again indexes it afresh.",
                            )
                            .ok_text("Forget")
                            .ok_variant(ButtonVariant::Danger)
                            .on_ok(move |_, _, cx| {
                                shell.update(cx, |s, cx| s.forget_location(&identity, &mount, cx));
                                true
                            })
                    });
                })
                .kind(ButtonKind::Danger),
            );
        }
        Some(Panel {
            kind: "Location",
            tag: Some((kind_tag.into(), tone)),
            title: row.name.clone().into(),
            subtitle: Some(subtitle.into()),
            facts: vec![
                fact("State", Some(row.state.text(now))),
                fact("Used", capacity.map(|c| format::size(c.total.saturating_sub(c.free)))),
                fact("Capacity", capacity.map(|c| format::size(c.total))),
                fact("Files", files.map(format::count)),
                fact("Last Synced", last_synced.map(|at| crate::dashboard_summary::ago(now, at))),
            ],
            actions,
        })
    }

    /// A Task: kind, progress, elapsed time and result; Cancel while it runs.
    fn task_panel(&mut self, id: u64, cx: &mut Context<Self>) -> Option<Panel> {
        let t = self.engine.shared.tasks.find(id)?;
        let unit = t.kind.unit();
        let progress = match (t.total, unit) {
            (Some(total), _) => Some(format!("{} of {}", format::count(t.done), format::count(total))),
            (None, "") => None,
            (None, unit) => Some(format!("{} {unit}", format::count(t.done))),
        };
        let (tag_label, tone, result) = match &t.outcome {
            None => ("Running", Tone::Accent, None),
            Some(Outcome::Done(m)) => ("Done", Tone::Neutral, Some(m.clone()).filter(|m| !m.is_empty())),
            Some(Outcome::Cancelled) => ("Cancelled", Tone::Neutral, None),
            Some(Outcome::Failed(m)) => ("Failed", Tone::Warning, Some(m.clone())),
        };
        let fact = |l: &'static str, v: Option<String>| (SharedString::from(l), SharedString::from(v.unwrap_or_else(|| "—".into())));
        let mut actions = Vec::new();
        if t.outcome.is_none() && t.cancellable {
            let tasks = self.engine.shared.tasks.clone();
            actions.push(
                Item::new(
                    "inspector-cancel",
                    if t.cancelling { "Cancelling…" } else { "Cancel" },
                    move |_, _, _| tasks.cancel(id),
                )
                .kind(ButtonKind::Danger)
                .disabled(t.cancelling.then_some("Cancel was requested")),
            );
        }
        let _ = cx;
        Some(Panel {
            kind: "Task",
            tag: Some((tag_label.into(), tone)),
            title: t.label.clone().into(),
            subtitle: None,
            facts: vec![
                fact("Kind", Some(t.kind.name().to_owned())),
                fact("Progress", progress),
                fact("Elapsed", Some(crate::tasks::duration_text(t.elapsed))),
                fact("Result", result),
            ],
            actions,
        })
    }

    /// Whether no text field has the keyboard (so `/` and Escape are the window's): nothing
    /// focused, or one of the lists.
    fn keys_are_ours(&self, window: &Window, cx: &App) -> bool {
        match window.focused(cx) {
            None => true,
            Some(h) => h == self.tree.focus_handle(cx) || h == self.table.focus_handle(cx),
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("Shell::render");
        // One read lock for the whole frame: the status strip and the Locations rows read the Index
        // again (the lock is recursive), and no writer can slip in between them.
        let shared = self.engine.shared.clone();
        let _frame_index = shared.index.read();
        let now = chrono::Utc::now().timestamp();
        self.compare.update(cx, |c, _| c.refresh_dates());
        // The Tasks page shows each running Task's elapsed time; nothing else repaints a Task that
        // reports no progress, so tick once a second while it is shown and Tasks run.
        if self.page == Page::Work && self.tick.is_none() && self.engine.shared.tasks.running_count() > 0 {
            self.tick = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let _ = this.update(cx, |this, cx| {
                    this.tick = None;
                    cx.notify();
                });
            }));
        }
        let rows = locations::rows(&self.engine.shared, &self.volumes, self.settings_page.read(cx).settings(), now);
        let inspector = self.inspected.clone().and_then(|what| {
            let members = self.inspector_members(&what, cx);
            self.inspector_panel(&what, &rows, now, cx)
                .map(|panel| inspector::render(panel, members, cx))
        });
        let page = match self.page {
            Page::Overview => self.dashboard.clone().into_any_element(),
            Page::Browse => self.browse(cx).into_any_element(),
            Page::Cleaner => self.cleaner.clone().into_any_element(),
            Page::Duplicates => self.duplicates_page(cx).into_any_element(),
            Page::Work => self.work_page(&rows, now, cx).into_any_element(),
            Page::Settings => self.settings_page.clone().into_any_element(),
        };
        // Settings lays out its own column; every other page sits in the canvas gutter.
        let gutter = self.page != Page::Settings;
        let p = palette(cx);
        h_flex()
            .size_full()
            .relative()
            .bg(p.ground)
            .text_color(p.text)
            .font_features(tabular())
            .line_height(px(20.))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let m = &event.keystroke.modifiers;
                if m.control || m.platform || m.alt || this.query.focus_handle(cx).is_focused(window) || !this.keys_are_ours(window, cx) {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "escape" if this.inspected.is_some() => {
                        this.clear_selection(cx);
                        this.inspected = None;
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "/" => {
                        this.query.focus_handle(cx).focus(window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(self.rail(&rows, now, window.scale_factor(), cx))
            .child(
                v_flex().flex_1().min_w_0().h_full().child(self.top_bar(cx)).child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .child(div().flex_1().min_h_0().when(gutter, |d| d.px(px(PAD)).pt(px(PAD))).child(page))
                                .child(self.dock(cx))
                                .child(self.status_bar(cx)),
                        )
                        .children(inspector),
                ),
            )
    }
}

/// What a context-menu item does to the selection.
#[derive(Clone, Copy)]
enum MenuAct {
    Open,
    Reveal,
    Terminal,
    CopyPath,
    CopyNameSize,
    ZoomIn,
    ZoomOut,
    Sync,
    Export,
    Trash,
}
