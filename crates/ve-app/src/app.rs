//! The window: sidebar, the pages (Explorer with its Query bar, lists, panels and Treemap,
//! Settings) and the status bar.

use crate::categories::CategoryPanel;
use crate::cleaner::{CleanerPage, ShowInExplorer};
use crate::compare::{Compare, CompareEvent};
use crate::dashboard::{Act, DashboardPage};
use crate::duplicates::DuplicatesPanel;
use crate::engine::{Engine, Phase, last_synced_label};
use crate::explorer::FlatList;
use crate::filters::FiltersPanel;
use crate::format;
use crate::host::{Attached, Host};
use crate::selection_panel::{SelectionEvent, SelectionPanel};
use crate::settings::Settings;
#[cfg(feature = "updater")]
use crate::settings_page::CheckNow;
use crate::settings_page::{Changed, Forget, Refresh, SettingsPage};
use crate::theme::{SERIF_FAMILY, palette, tabular};
use crate::trash::{self, Trash};
use crate::tree::TreeList;
use crate::treemap::{Treemap, TreemapEvent};
use crate::ui::{caption, dot, pill};
use gpui_kit::component::WindowExt as _;
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
    Dashboard,
    Explorer,
    Cleaner,
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

/// How the Explorer looked when its window closed to the tray; a reopened window starts there.
/// The Query itself is the Engine's ([`crate::engine::Shared::query`]).
pub struct Remembered {
    page: Page,
    show_tree: bool,
    show_duplicates: bool,
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
    /// Whether the Explorer shows the Duplicates panel instead of a list.
    show_duplicates: bool,
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
    /// The Tasks list (opened from the sidebar) is showing.
    tasks_open: bool,
    /// When an outside click last closed the Tasks list.
    tasks_closed: Option<std::time::Instant>,
    /// The Locations list (opened from the sidebar) is showing.
    locations_open: bool,
    /// When an outside click last closed the Locations list.
    locations_closed: Option<std::time::Instant>,
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
    /// A repaint is already scheduled for the open Tasks list's elapsed times.
    tasks_ticking: bool,
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
        let duplicates = cx.new(|_| DuplicatesPanel::new(engine.shared.clone(), trash.clone(), volumes.clone()));
        let treemap_depth = settings.treemap_depth;
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
            this.page = Page::Explorer;
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
        let treemap = cx.new(|cx| Treemap::new(engine.shared.clone(), categories.clone(), treemap_depth, cx));
        let selection_panel = cx.new(|_| SelectionPanel::new(engine.shared.clone()));
        cx.subscribe_in(
            &selection_panel,
            window,
            |this, _, event: &SelectionEvent, window, cx| match event {
                SelectionEvent::Scope(folder) => this.scope(Some(*folder), window, cx),
                SelectionEvent::Trash(id) => this.trash.ask_trash(&[*id], &this.volumes, window, cx),
                SelectionEvent::Sync(folder) => this.engine.sync_folder(*folder),
            },
        )
        .detach();
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
                this.selection_panel.update(cx, |p, cx| p.set_selected(Some(*id), cx));
                this.reveal(*id, cx)
            }
            TreemapEvent::Scope(folder) => this.scope(*folder, window, cx),
            TreemapEvent::Depth(depth) => {
                let depth = *depth;
                this.settings_page
                    .update(cx, |page, cx| page.apply(|s| s.treemap_depth = depth, cx));
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
            Ok("explorer") => Page::Explorer,
            Ok("cleaner") => Page::Cleaner,
            Ok("dashboard") => Page::Dashboard,
            // The scroll measurement needs the Explorer's list on screen.
            _ if std::env::var_os("VE_BENCH").is_some() => Page::Explorer,
            _ => Page::Dashboard,
        };
        #[cfg(not(feature = "bench"))]
        let page = Page::Dashboard;
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
            show_duplicates: false,
            trash,
            treemap,
            selection_panel,
            categories: category_panel,
            category_map: categories.clone(),
            volumes,
            offered: HashSet::new(),
            startup_problems,
            tasks_open: false,
            tasks_closed: None,
            locations_open: false,
            locations_closed: None,
            #[cfg(feature = "updater")]
            update: None,
            #[cfg(feature = "updater")]
            self_update: SelfUpdate::Link,
            list_width: Rc::default(),
            selected_size: Cell::new(None),
            tasks_ticking: false,
            #[cfg(feature = "bench")]
            demoed: false,
        };
        if let Some(remembered) = cx.global_mut::<Host>().remembered.take() {
            shell.restore(remembered, cx);
        }
        cx.on_release(|this, cx| {
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
            show_duplicates: self.show_duplicates,
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
        self.show_duplicates = r.show_duplicates;
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

    /// How far the sidebar's Tasks and Locations rows sit higher while the update notice (which
    /// goes between them and Settings) shows: its margin, padding, border and text lines.
    #[cfg(feature = "updater")]
    fn update_row_height(&self) -> f32 {
        const NOTICE: f32 = 16. + 16. + 2. + 20. + 16.;
        const ACTION: f32 = 16.;
        match (&self.update, &self.self_update) {
            (None, _) => 0.,
            (Some(_), SelfUpdate::Link) => NOTICE,
            (Some(_), _) => NOTICE + ACTION,
        }
    }

    #[cfg(not(feature = "updater"))]
    fn update_row_height(&self) -> f32 {
        0.
    }

    /// The mounted Volumes changed: the Settings page re-applies them to the Engine, and each
    /// removable Volume never seen before is offered for indexing.
    fn mounts_changed(&mut self, now: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) {
        let before = std::mem::replace(&mut self.volumes, now.clone());
        self.cleaner.update(cx, |c, cx| c.set_volumes(now.clone(), cx));
        self.duplicates.update(cx, |d, cx| d.set_volumes(now.clone(), cx));
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
                    let button = pill(id, label).small();
                    if index_it { button.primary() } else { button }.build(cx, click)
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
    fn outline_selection(&self, cx: &mut Context<Self>) {
        let ids = self.selected_entries(cx);
        let Some(last) = self.last_clicked(cx).or_else(|| ids.first().copied()) else {
            return;
        };
        let rest: Vec<_> = ids.into_iter().filter(|&id| id != last).collect();
        self.treemap.update(cx, |t, cx| {
            t.set_selected(Some(last), cx);
            t.set_also_selected(rest, cx);
        });
        self.selection_panel.update(cx, |p, cx| p.set_selected(Some(last), cx));
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
                        pill(("saved-rename", i), "Rename")
                            .small()
                            .selected(renaming == Some(i))
                            .build(cx, rename),
                    )
                    .child(pill(("saved-delete", i), "Delete").small().build(cx, delete))
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
                .title(
                    div()
                        .font_family(SERIF_FAMILY)
                        .text_size(px(28.))
                        .line_height(px(32.))
                        .font_weight(FontWeight::NORMAL)
                        .child("Saved Queries"),
                )
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
                                        .h(px(38.))
                                        .px(px(6.))
                                        .items_center()
                                        .rounded_full()
                                        .border_1()
                                        .border_color(p.control)
                                        .bg(p.ground)
                                        .child(Input::new(&name_input).appearance(false).bordered(false)),
                                )
                                .child(
                                    pill("saved-save", if renaming.is_some() { "Rename" } else { "Save current" })
                                        .primary()
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
                        .child(pill("saved-close", "Close").build(cx, |_, window, cx| window.close_dialog(cx))),
                )
        });
    }

    /// What a click on the Dashboard asked for.
    fn dashboard_act(&mut self, act: &Act, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            Act::Explore(query) => {
                if let Some(text) = query {
                    self.set_query_text(text, window, cx);
                }
                self.page = Page::Explorer;
            }
            Act::OpenCleaner => {
                self.page = Page::Cleaner;
                cx.notify();
            }
            Act::Forget(identity, mount) => {
                self.settings_page
                    .update(cx, |page, cx| page.apply(|s| s.forget(identity, mount), cx));
                self.engine.forget(identity.clone());
            }
            Act::Sync(identities) => {
                for identity in identities {
                    self.engine.sync_location(identity.clone());
                }
            }
            Act::SyncAll => self.engine.sync_now(),
            Act::StartLive => {
                Host::start_live(cx);
                cx.notify();
            }
            Act::MoveToTrash(id) => self.trash.ask_trash(&[*id], &self.volumes, window, cx),
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
            .h(px(36.))
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
        self.selection_panel.update(cx, |p, cx| p.set_selected(None, cx));
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

    fn nav_row(&self, id: &'static str, label: &'static str, icon: impl IntoElement, page: Page, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = palette(cx);
        let (muted, text, active) = (p.muted, p.text, p.active);
        let current = self.page == page;
        h_flex()
            .id(id)
            .h(px(32.))
            .my(px(1.))
            .px(px(12.))
            .gap(px(10.))
            .items_center()
            .rounded(px(10.))
            .cursor_pointer()
            .text_color(if current { text } else { muted })
            .when(current, |d| d.bg(active))
            .hover(move |s| s.text_color(text))
            .child(icon)
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = page;
                cx.notify();
            }))
    }

    /// Brand, scope switcher, pages, Locations and Tasks; Settings and the user at the bottom.
    fn sidebar(&self, locations: &[crate::locations::Row], scale: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (border, ground) = (p.border, p.ground);
        let avatar = p.avatar;
        let count = locations.len();
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "user".into());
        let initial = user.chars().next().map_or('U', |c| c.to_ascii_uppercase());
        let display_name = format!("{initial}{}", user.chars().skip(1).collect::<String>());
        let entries = self.engine.shared.index.read().live_count() as u64;

        v_flex()
            .w(px(224.))
            .h_full()
            .flex_none()
            .pt(px(20.))
            .px(px(12.))
            .pb(px(16.))
            .bg(ground)
            .border_r_1()
            .border_color(border)
            .child(
                h_flex()
                    .h(px(32.))
                    .px(px(8.))
                    .gap(px(10.))
                    .items_center()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(img(app_icon(24., scale)).size(px(24.)).flex_none())
                    .child(APP_NAME),
            )
            .child(
                h_flex()
                    .h(px(36.))
                    .mt(px(12.))
                    .px(px(12.))
                    .justify_between()
                    .items_center()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(border)
                    .child("Index")
                    .child(caption(format!("{count} Location{}", if count == 1 { "" } else { "s" }), cx)),
            )
            .child(
                self.nav_row(
                    "nav-dashboard",
                    "Dashboard",
                    Icon::new(IconName::LayoutDashboard).size(px(16.)),
                    Page::Dashboard,
                    cx,
                )
                .mt(px(12.)),
            )
            .child(self.nav_row(
                "nav-explorer",
                "Explorer",
                Icon::new(IconName::Folder).size(px(16.)),
                Page::Explorer,
                cx,
            ))
            // The same trash can as Move to Trash.
            .child(self.nav_row(
                "nav-cleaner",
                "Cleaner",
                Icon::empty().path("icons/trash.svg").size(px(16.)),
                Page::Cleaner,
                cx,
            ))
            .child(div().flex_1().min_h_0())
            .child({
                let (health, tip) = crate::locations::summary(locations, chrono::Utc::now().timestamp());
                crate::locations::indicator_row(health, tip, self.locations_open, cx).on_click(cx.listener(|this, _, _, cx| {
                    // The press that closed the panel (an outside click) must not reopen it.
                    let just_closed = this
                        .locations_closed
                        .take()
                        .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300));
                    this.locations_open = !this.locations_open && !just_closed;
                    this.tasks_open = false;
                    cx.notify();
                }))
            })
            .child(
                crate::tasks::indicator_row(&self.engine.shared.tasks, self.tasks_open, cx).on_click(cx.listener(|this, _, _, cx| {
                    // The press that closed the panel (an outside click) must not reopen it.
                    let just_closed = this
                        .tasks_closed
                        .take()
                        .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300));
                    this.tasks_open = !this.tasks_open && !just_closed;
                    this.locations_open = false;
                    cx.notify();
                })),
            )
            .children(self.update_row(cx))
            .child(
                self.nav_row(
                    "nav-settings",
                    "Settings",
                    Icon::new(IconName::Settings).size(px(16.)),
                    Page::Settings,
                    cx,
                )
                .mt(px(16.)),
            )
            .child(
                h_flex()
                    .mt(px(8.))
                    .pt(px(12.))
                    .px(px(12.))
                    .gap(px(10.))
                    .items_center()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        div()
                            .size(px(28.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(avatar.0)
                            .text_color(avatar.1)
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(initial.to_string()),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(div().font_weight(FontWeight::MEDIUM).child(display_name))
                            .child(caption(format!("{} Entries", format::count(entries)), cx)),
                    ),
            )
    }

    /// The Query: wide enough to read whole, before the Filters that edit it. `/` focuses it.
    fn find_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        h_flex()
            .flex_1()
            .min_w_0()
            .h(px(36.))
            .pl(px(14.))
            .pr(px(14.))
            .gap(px(8.))
            .items_center()
            .rounded_full()
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
            .child(Icon::new(IconName::Search).small().text_color(p.muted))
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
            .child(div().text_xs().text_color(p.muted).child("/"))
    }

    /// `.tabs`: underlined Tree / Flat list with the summary line at the right.
    fn tabs(&self, summary: String, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (text, muted, border) = (p.text, p.muted, p.border);
        let tab = |id: &'static str, label: &'static str, on: bool, tree: Option<bool>, cx: &mut Context<Self>| {
            div()
                .id(id)
                .relative()
                .h(px(36.))
                .flex()
                .items_center()
                .cursor_pointer()
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { text } else { muted })
                .child(label)
                .when(on, |d| {
                    d.child(div().absolute().left_0().right_0().bottom(px(-1.)).h(px(2.)).bg(text))
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.show_duplicates = tree.is_none();
                    if let Some(tree) = tree {
                        this.show_tree = tree;
                    }
                    cx.notify();
                }))
        };
        h_flex()
            .gap(px(24.))
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(tab("tab-tree", "Tree", self.show_tree && !self.show_duplicates, Some(true), cx))
            .child(tab(
                "tab-flat",
                "Flat list",
                !self.show_tree && !self.show_duplicates,
                Some(false),
                cx,
            ))
            .child(tab("tab-dupes", "Duplicates", self.show_duplicates, None, cx))
            .child(div().ml_auto().text_xs().text_color(muted).child(summary))
    }

    fn explorer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.fit_lists(cx);
        let p = palette(cx);
        let (text, muted) = (p.text, p.muted);
        let surface = p.surface;
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
        let filters = pill("filters", label)
            .icon("icons/filters.svg")
            .small()
            .selected(self.show_filters)
            .build(
                cx,
                cx.listener(|this, _, _, cx| {
                    this.show_filters = !this.show_filters;
                    cx.notify();
                }),
            );
        let saved_count = self.settings_page.read(cx).settings().saved_queries.len();
        let saved = pill(
            "saved",
            if saved_count > 0 {
                format!("Saved · {saved_count}")
            } else {
                "Saved".into()
            },
        )
        .small()
        .build(
            cx,
            cx.listener(|this, _, window, cx| {
                this.renaming_saved = None;
                this.open_saved_dialog(window, cx);
            }),
        );
        let comparing = self.compare.read(cx).active();
        let compare = pill(
            "compare",
            comparing.map_or("Compare with".to_owned(), |d| format!("Compared with {d}")),
        )
        .small()
        .busy(self.compare.read(cx).working())
        .selected(comparing.is_some())
        .build(
            cx,
            cx.listener(|this, _, window, cx| Compare::open_dialog(&this.compare, window, cx)),
        );
        let export = pill("export", "Export CSV")
            .small()
            .build(cx, cx.listener(|this, _, window, cx| this.export_csv(window, cx)));
        let selection = (sel_len > 0).then(|| {
            h_flex()
                .gap(px(4.))
                .text_xs()
                .text_color(muted)
                .child(div().font_weight(FontWeight::MEDIUM).text_color(text).child(sel_len.to_string()))
                .child(format!("selected · {}", format::size(selected_size)))
        });
        let list = if self.show_duplicates {
            self.duplicates.clone().into_any_element()
        } else if self.show_tree {
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
        let natural = if self.show_tree || self.show_duplicates {
            self.tree.read(cx).delegate().natural_width()
        } else {
            FlatList::natural_width()
        } + 24.;
        let left = v_flex()
            .min_w(px(360.))
            .h_full()
            .gap(px(16.))
            .flex_shrink(1.)
            .flex_grow(0.)
            .flex_basis(px(natural));
        let left = left
            .child(
                div()
                    .id("explorer-list")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(surface)
                    .rounded(px(22.))
                    .pt(px(16.))
                    .px(px(12.))
                    .pb(px(8.))
                    .child(measure)
                    .child(list),
            )
            .child(div().h(px(301.)).flex_none().child(self.categories.clone()));
        let right = v_flex()
            .min_w(px(440.))
            .h_full()
            .gap(px(16.))
            .flex_1()
            .child(if self.show_filters {
                crate::motion::slide_in(div().flex_1().min_h_0().child(self.filters.clone()), "filters-slide").into_any_element()
            } else {
                div().flex_1().min_h_0().child(self.treemap.clone()).into_any_element()
            })
            .when(!self.show_filters, |d| d.child(self.selection_panel.clone()));
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .pt(px(24.))
                    .px(px(24.))
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
                            "/" => this.query.focus_handle(cx).focus(window, cx),
                            _ => {}
                        }
                    }))
                    .child(
                        h_flex()
                            .h(px(36.))
                            .mb(px(8.))
                            .gap(px(16.))
                            .items_center()
                            .child(div().flex_none().text_size(px(24.)).line_height(px(32.)).child("Explorer"))
                            .child(self.find_box(cx))
                            .child(
                                h_flex()
                                    .flex_none()
                                    .gap(px(12.))
                                    .items_center()
                                    .children(selection)
                                    .child(saved)
                                    .when(self.show_tree && !self.show_duplicates, |d| d.child(compare))
                                    .child(filters)
                                    .child(export),
                            ),
                    )
                    .child(self.tabs(summary, cx))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_h_0()
                            .gap(px(16.))
                            .pt(px(16.))
                            .pb(px(16.))
                            .child(left)
                            .child(right),
                    ),
            )
            .child(self.status_bar(cx))
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // One read lock for the whole frame: the status bar and the Locations rows read the Index
        // again (the lock is recursive), and no writer can slip in between them.
        let _frame_index = self.engine.shared.index.read();
        let now = chrono::Utc::now().timestamp();
        let above = self.update_row_height();
        // The Tasks list shows each running Task's elapsed time; nothing else repaints a Task that
        // reports no progress, so tick once a second while it is open and Tasks run.
        if self.tasks_open && !self.tasks_ticking && self.engine.shared.tasks.running_count() > 0 {
            self.tasks_ticking = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let _ = this.update(cx, |this, cx| {
                    this.tasks_ticking = false;
                    cx.notify();
                });
            })
            .detach();
        }
        let rows = crate::locations::rows(&self.engine.shared, &self.volumes, self.settings_page.read(cx).settings(), now);
        let page = match self.page {
            Page::Dashboard => self.dashboard.clone().into_any_element(),
            Page::Explorer => self.explorer(cx).into_any_element(),
            Page::Cleaner => self.cleaner.clone().into_any_element(),
            Page::Settings => self.settings_page.clone().into_any_element(),
        };
        let p = palette(cx);
        // Problems have no status bar to sit in outside the Explorer.
        let problems =
            self.page != Page::Explorer && (!self.startup_problems.is_empty() || !self.engine.shared.status.lock().problems.is_empty());
        h_flex()
            .size_full()
            .relative()
            .bg(p.ground)
            .text_color(p.text)
            .font_features(tabular())
            .line_height(px(20.))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if (this.tasks_open || this.locations_open) && event.keystroke.key == "escape" {
                    this.tasks_open = false;
                    this.locations_open = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(self.sidebar(&rows, window.scale_factor(), cx))
            .when(self.tasks_open, |d| {
                d.child(
                    deferred(
                        div()
                            .absolute()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.tasks_open = false;
                                this.tasks_closed = Some(std::time::Instant::now());
                                cx.notify();
                            }))
                            .left(px(232.))
                            .bottom(px(60. + above))
                            .child(crate::motion::slide_in(
                                div().child(crate::tasks::list_panel(&self.engine.shared.tasks, cx)),
                                "tasks-slide",
                            )),
                    )
                    .with_priority(1),
                )
            })
            .when(self.locations_open, |d| {
                d.child(
                    deferred(
                        div()
                            .absolute()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.locations_open = false;
                                this.locations_closed = Some(std::time::Instant::now());
                                cx.notify();
                            }))
                            .left(px(232.))
                            .bottom(px(94. + above))
                            .child(crate::motion::slide_in(
                                div().child(crate::locations::panel(rows, now, &self.engine, cx)),
                                "locations-slide",
                            )),
                    )
                    .with_priority(1),
                )
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(div().flex_1().min_h_0().child(page))
                    .when(problems, |c| c.child(self.status_bar(cx))),
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
