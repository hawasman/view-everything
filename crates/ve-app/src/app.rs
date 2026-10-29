//! The window: sidebar, the pages (Explorer with its Query bar, lists, panels and Treemap,
//! Settings) and the status bar.

use crate::engine::{Engine, Phase, Repaint, last_synced_label};
use crate::categories::CategoryPanel;
use crate::dashboard::{Act, DashboardPage};
use crate::explorer::FlatList;
use crate::filters::FiltersPanel;
use crate::format;
use crate::settings::Settings;
use crate::settings_page::{Changed, Forget, Refresh, SettingsPage};
use crate::theme::{palette, tabular};
use crate::ui::{caption, dot, pill};
use crate::tree::TreeList;
use crate::trash::{self, Trash};
use crate::treemap::{Treemap, TreemapEvent};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

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
    Settings,
}

pub struct Shell {
    engine: Engine,
    page: Page,
    query: Entity<InputState>,
    table: Entity<TableState<FlatList>>,
    tree: Entity<TableState<TreeList>>,
    /// Whether the Explorer shows the tree rather than the flat list.
    show_tree: bool,
    filters: Entity<FiltersPanel>,
    show_filters: bool,
    settings_page: Entity<SettingsPage>,
    dashboard: Entity<DashboardPage>,
    trash: Trash,
    treemap: Entity<Treemap>,
    categories: Entity<CategoryPanel>,
    volumes: Vec<VolumeInfo>,
    /// Identities of the Volumes already offered for indexing this session.
    offered: HashSet<String>,
    startup_problems: Vec<String>,
    /// The Explorer lists' width in pixels, as of the last paint.
    list_width: Rc<Cell<f32>>,
    #[cfg(feature = "bench")]
    demoed: bool,
}

impl Shell {
    pub fn new(
        settings: Settings,
        categories: CategoryMap,
        volumes: Vec<VolumeInfo>,
        startup_problems: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let indexed: Vec<VolumeInfo> = volumes
            .iter()
            .filter(|v| settings.is_indexed(v))
            .cloned()
            .collect();
        let intervals = settings.intervals(&indexed);
        // One pending wake-up is enough: the window then reads everything that changed.
        let (wake, woken) = async_channel::bounded::<()>(1);
        let repaint: Repaint = Arc::new(move || {
            let _ = wake.try_send(());
        });
        let engine =
            Engine::start(repaint.clone(), volumes.clone(), indexed, settings.remotes.clone(), intervals, categories.clone());

        let trash = Trash::new(engine.shared.clone(), repaint.clone());

        let dashboard = cx.new(|cx| {
            let mut page =
                DashboardPage::new(engine.shared.clone(), trash.clone(), categories.clone(), &settings, repaint, window, cx);
            page.set_context(&settings, volumes.clone(), cx);
            page
        });
        let settings_page = cx.new(|cx| SettingsPage::new(settings, volumes.clone(), window, cx));
        cx.subscribe_in(&dashboard, window, |this, _, act: &Act, window, cx| this.dashboard_act(act, window, cx))
            .detach();
        cx.subscribe(&settings_page, |this, _, changed: &Changed, cx| {
            this.engine.set_volumes(changed.mounted.clone(), changed.indexed.clone(), changed.remotes.clone());
            this.engine.set_intervals(changed.intervals.clone());
            let settings = this.settings_page.read(cx).settings().clone();
            this.dashboard.update(cx, |d, cx| d.set_context(&settings, changed.mounted.clone(), cx));
            // Saved fine, so the settings file read at startup has been replaced.
            if !this.settings_page.read(cx).has_problem() {
                this.startup_problems.retain(|p| !p.starts_with("Settings ignored"));
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&settings_page, |this, _, forget: &Forget, _| this.engine.forget(forget.0.clone()))
            .detach();
        cx.subscribe(&settings_page, |this, _, refresh: &Refresh, _| this.engine.refresh_location(refresh.0.clone()))
            .detach();

        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Find a file by name")
        });
        let filters = cx.new(|cx| FiltersPanel::new(window, cx));
        cx.subscribe_in(&query, window, |this, query, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                let text = query.read(cx).value().to_string();
                this.filters.update(cx, |f, cx| f.reload(&text, window, cx));
                this.engine.set_query(text);
                // Searching from anywhere shows the results.
                this.page = Page::Explorer;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe_in(&filters, window, |this, _, text: &String, window, cx| {
            this.query.update(cx, |q, cx| q.set_value(text.as_str(), window, cx));
            this.engine.set_query(text.clone());
        })
        .detach();
        let list = FlatList::new(engine.shared.clone());
        let table = cx.new(|cx| {
            TableState::new(list, window, cx)
                .col_resizable(false)
                .select_on_modifier_click(false)
        });
        let treemap = cx.new(|cx| Treemap::new(engine.shared.clone(), categories.clone(), cx));
        let category_panel = cx.new(|_| CategoryPanel::new(query.clone()));

        let tree = cx.new(|cx| {
            TableState::new(TreeList::new(engine.shared.clone(), categories.clone()), window, cx)
                .col_resizable(false)
                .select_on_modifier_click(false)
        });

        // Keyboard moves select a single row; clicks go through the lists' own handlers. A
        // double-click on a folder zooms the Query into it, as on the Treemap.
        cx.subscribe_in(&table, window, |this, table, event: &TableEvent, window, cx| {
            match event {
                TableEvent::SelectRow(row) => table.update(cx, |t, _| {
                    if let Some(id) = t.delegate().entry_at(*row) {
                        t.delegate_mut().selection.only(*row, id);
                    }
                }),
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
                        t.delegate_mut().selection.only(*row, id);
                    }
                }),
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
            TreemapEvent::Select(id) => this.reveal(*id, cx),
            TreemapEvent::Scope(folder) => this.scope(*folder, window, cx),
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
                this.settings_page.clone().update(cx, |page, cx| page.open_add_remote(window, cx))
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
        // removable one is offered for indexing.
        #[cfg(feature = "bench")]
        let watching = std::env::var_os("VE_BENCH_MOUNT").is_none() && std::env::var_os("VE_FAKE_SHARE").is_none();
        #[cfg(not(feature = "bench"))]
        let watching = true;
        if watching {
            let (mounts, mounted) = async_channel::unbounded::<Vec<VolumeInfo>>();
            ve_scan::watch::watch(volumes.clone(), move |now| {
                let _ = mounts.send_blocking(now);
            });
            cx.spawn_in(window, async move |this, cx| {
                while let Ok(now) = mounted.recv().await {
                    if this.update_in(cx, |this, window, cx| this.mounts_changed(now, window, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        // Save the Index as the app quits.
        cx.on_app_quit(|this, _| {
            this.engine.shutdown();
            async {}
        })
        .detach();

        Self {
            engine,
            page,
            query,
            table,
            tree,
            // The 10M-Entry measurement scrolls the flat list, so it must be the one on screen.
            show_tree: !cfg!(feature = "bench") || std::env::var_os("VE_BENCH").is_none(),
            filters,
            show_filters: false,
            settings_page,
            dashboard,
            trash,
            treemap,
            categories: category_panel,
            volumes,
            offered: HashSet::new(),
            startup_problems,
            list_width: Rc::default(),
            #[cfg(feature = "bench")]
            demoed: false,
        }
    }

    /// The mounted Volumes changed: the Settings page re-applies them to the Engine, and each
    /// removable Volume never seen before is offered for indexing.
    fn mounts_changed(&mut self, now: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) {
        let before = std::mem::replace(&mut self.volumes, now.clone());
        // An unmounted Volume's 'Index this?' no longer applies; a replug asks again.
        let gone: Vec<String> = self.offered.iter().filter(|id| !now.iter().any(|v| &v.identity == *id)).cloned().collect();
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
        let name = if volume.label.is_empty() { volume.mount.display().to_string() } else { volume.label.clone() };
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
            page.apply(
                |s| if index_it { s.set_indexed(volume, true) } else { s.decline(volume) },
                cx,
            )
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
            cx.emit(InputEvent::Change);
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

    /// Outlines on the Treemap the one Entry the shown list selects. A list that cannot show
    /// what the Treemap selected leaves its outline alone.
    fn outline_selection(&self, cx: &mut Context<Self>) {
        if let [id] = self.selected_entries(cx).as_slice() {
            let id = *id;
            self.treemap.update(cx, |t, cx| t.set_selected(Some(id), cx));
        }
    }

    /// Picks up what the Engine's threads changed since the last look.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let latest = self.engine.shared.view.lock().clone();
        self.table.update(cx, |table, cx| {
            if table.delegate_mut().set_view(latest.clone()) {
                table.refresh(cx);
                // Row numbers mean something else in a new View.
                table.clear_selection(cx);
            }
        });
        self.tree.update(cx, |table, cx| {
            if table.delegate_mut().set_view(latest.clone()) {
                table.refresh(cx);
                table.clear_selection(cx);
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
        self.settings_page.update(cx, |page, cx| {
            page.set_offline(offline, cx);
            page.set_remote_synced(synced, cx);
        });
        self.dashboard.update(cx, |d, cx| d.poll(cx));
        cx.notify();
    }

    /// What a click on the Dashboard asked for.
    fn dashboard_act(&mut self, act: &Act, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            Act::Explore(query) => {
                if let Some(text) = query {
                    self.query.update(cx, |q, cx| {
                        q.set_value(text.as_str(), window, cx);
                        // set_value is silent; Change recomputes the View.
                        cx.emit(InputEvent::Change);
                    });
                }
                self.page = Page::Explorer;
            }
            Act::Forget(identity, mount) => {
                self.settings_page.update(cx, |page, cx| page.apply(|s| s.forget(identity, mount), cx));
                self.engine.forget(identity.clone());
            }
            Act::Refresh(identities) => {
                for identity in identities {
                    self.engine.refresh_location(identity.clone());
                }
            }
            Act::MoveToTrash(id) => self.trash.ask_trash(&[*id], &self.volumes, window, cx),
            Act::SaveQuery(text) => {
                self.settings_page.update(cx, |page, cx| page.apply(|s| s.recent_query = text.clone(), cx));
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
            Phase::Syncing { volumes, catching_up, started } => {
                let what = if *catching_up { "Catching up" } else { "Scanning" };
                let names: Vec<_> = volumes.iter().map(|v| v.display().to_string()).collect();
                bar = bar.child(item().child(dot(accent)).child(format!(
                    "{what} {} — {} Entries read, {}s",
                    names.join(", "),
                    format::count(progress.entries()),
                    started.elapsed().as_secs()
                )));
            }
            Phase::Idle => {}
        }
        let synced = {
            let oldest = shared.index.read().oldest_synced();
            last_synced_label(oldest, chrono::Utc::now().timestamp())
        };
        if let Some(label) = synced.filter(|_| !matches!(status.phase, Phase::LoadingIndex)) {
            bar = bar.child(sep()).child(item().child(label));
        }
        let offline = shared.index.read().volumes().filter(|(_, v)| !v.online).count();
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
            bar = bar.child(sep()).child(div().text_color(warn).child(problem));
        }
        bar.child(div().flex_1())
            .child(item().child(
                h_flex()
                    .gap(px(4.))
                    .child(div().text_color(text).child(format::count(shared.index.read().live_count() as u64)))
                    .child("Entries"),
            ))
            .child(sep())
            .child(self.trash_controls(cx))
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
                    Tooltip::new(
                        "Trashed files still take up space until the Trash is emptied. Click to open the Trash.",
                    )
                    .build(window, cx)
                })
                .on_click(cx.listener(|this, _, window, cx| this.trash.open(window, cx))),
        );
        if trash::CAN_EMPTY {
            let empty = size == Some(0);
            controls = controls.child(
                div()
                    .id("empty-trash")
                    .rounded(px(6.))
                    .px(px(4.))
                    .when(empty, |d| d.opacity(0.5))
                    .when(!empty, |d| {
                        d.cursor_pointer()
                            .hover(move |s| s.bg(hover).text_color(text))
                            .on_click(cx.listener(|this, _, window, cx| this.trash.ask_empty(window, cx)))
                    })
                    .child("Empty Trash"),
            );
        }
        controls
    }

    /// The Entries selected in whichever list the Explorer shows.
    fn selected_entries(&self, cx: &App) -> Vec<ve_core::EntryId> {
        if self.show_tree {
            self.tree.read(cx).delegate().selection.ids()
        } else {
            self.table.read(cx).delegate().selection.ids()
        }
    }

    fn ask_trash_selected(&self, window: &mut Window, cx: &mut App) {
        let ids = self.selected_entries(cx);
        if !ids.is_empty() {
            self.trash.ask_trash(&ids, &self.volumes, window, cx);
        }
    }

    /// Lets the lists' name columns take the width the panel gives them.
    fn fit_lists(&self, cx: &mut Context<Self>) {
        let width = self.list_width.get();
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

    fn nav_row(&self, id: &'static str, label: &'static str, icon: IconName, page: Page, cx: &mut Context<Self>) -> Stateful<Div> {
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
            .child(Icon::new(icon).size(px(16.)))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = page;
                cx.notify();
            }))
    }

    /// Brand, scope switcher, Find-a-file, pages, Locations; Settings and the user at the bottom.
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (text, muted, border, ground, surface, accent) = (p.text, p.muted, p.border, p.ground, p.surface, p.accent);
        let control = p.control;
        let (positive, avatar) = (p.positive, p.avatar);
        let status = self.engine.shared.status.lock().clone();
        let syncing: Vec<_> = match &status.phase {
            Phase::Syncing { volumes, .. } => volumes.clone(),
            _ => Vec::new(),
        };
        // Every Location in the Index: Offline Volumes too, with a grey dot.
        let rows: Vec<(String, Hsla)> = self
            .engine
            .shared
            .index
            .read()
            .volumes()
            .map(|(_, v)| {
                let name = match (v.remote(), self.volumes.iter().find(|m| m.identity == v.identity)) {
                    (Some(remote), _) => remote.label(),
                    (None, Some(m)) => volume_name(m),
                    (None, None) => v.mount.display().to_string(),
                };
                let state = if !v.online {
                    muted
                } else if syncing.contains(&v.mount) {
                    accent
                } else {
                    positive
                };
                (name, state)
            })
            .collect();
        let count = rows.len();
        let locations = rows.into_iter().enumerate().map(|(ix, (name, state))| {
            h_flex()
                .id(("location", ix))
                .h(px(32.))
                .my(px(1.))
                .px(px(12.))
                .gap(px(10.))
                .items_center()
                .text_color(muted)
                .child(dot(state))
                .child(div().min_w_0().truncate().child(name))
        });
        let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "user".into());
        let initial = user.chars().next().map_or('U', |c| c.to_ascii_uppercase());
        let display_name = format!("{initial}{}", user.chars().skip(1).collect::<String>());
        let entries = self.engine.shared.index.read().live_count() as u64;

        let mark = div()
            .relative()
            .size(px(22.))
            .flex_none()
            .child(div().absolute().left(px(1.)).top(px(1.)).w(px(12.)).h(px(20.)).rounded(px(2.)).bg(text))
            .child(div().absolute().left(px(15.)).top(px(1.)).w(px(6.)).h(px(9.)).rounded(px(2.)).bg(accent))
            .child(div().absolute().left(px(15.)).top(px(12.)).w(px(6.)).h(px(9.)).rounded(px(2.)).bg(control));
        let find = h_flex()
            .h(px(32.))
            .mt(px(12.))
            .mb(px(8.))
            .pl(px(4.))
            .pr(px(10.))
            .gap(px(4.))
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(border)
            .bg(surface)
            .child(div().flex_1().min_w_0().child(Input::new(&self.query).appearance(false).bordered(false).small()))
            .child(div().text_xs().text_color(muted).child("/"));

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
                    .child(mark)
                    .child("view-everything"),
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
            .child(find)
            .child(self.nav_row("nav-dashboard", "Dashboard", IconName::LayoutDashboard, Page::Dashboard, cx))
            .child(self.nav_row("nav-explorer", "Explorer", IconName::Folder, Page::Explorer, cx))
            .child(div().mt(px(30.)).mb(px(8.)).px(px(12.)).child(caption("Locations", cx)))
            .child(v_flex().id("locations").flex_1().min_h_0().overflow_y_scroll().children(locations))
            .child(self.nav_row("nav-settings", "Settings", IconName::Settings, Page::Settings, cx).mt(px(16.)))
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

    /// `.tabs`: underlined Tree / Flat list with the summary line at the right.
    fn tabs(&self, summary: String, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (text, muted, border) = (p.text, p.muted, p.border);
        let tab = |id: &'static str, label: &'static str, on: bool, tree: bool, cx: &mut Context<Self>| {
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
                    this.show_tree = tree;
                    cx.notify();
                }))
        };
        h_flex()
            .gap(px(24.))
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(tab("tab-tree", "Tree", self.show_tree, true, cx))
            .child(tab("tab-flat", "Flat list", !self.show_tree, false, cx))
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
        let selected = self.selected_entries(cx);
        let blocked = self.trash.blocker_of(&selected, &self.volumes);
        let selected_size = {
            let view = self.engine.shared.view.lock().clone();
            let index = self.engine.shared.index.read();
            view.map_or(0, |v| {
                selected
                    .iter()
                    .filter(|&&id| index.is_live(id))
                    .map(|&id| v.filter.totals(&index, id).on_disk)
                    .sum()
            })
        };
        let width = self.list_width.clone();
        let measure = canvas(move |b, _, _| width.set(f32::from(b.size.width)), |_, _, _, _| {})
            .absolute()
            .size_full();
        let active = self.filters.read(cx).active();
        let label = if active > 0 { format!("Filters · {active}") } else { "Filters".into() };
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
        let why = if selected.is_empty() {
            Some(SharedString::from("Select Entries to move to the Trash"))
        } else {
            blocked.map(SharedString::from)
        };
        let trash = pill("trash", "Move to Trash")
            .icon("icons/trash.svg")
            .kbd("Del")
            .small()
            .disabled(why)
            .build(cx, cx.listener(|this, _, window, cx| this.ask_trash_selected(window, cx)));
        let selection = (!selected.is_empty()).then(|| {
            h_flex()
                .gap(px(4.))
                .text_xs()
                .text_color(muted)
                .child(div().font_weight(FontWeight::MEDIUM).text_color(text).child(selected.len().to_string()))
                .child(format!("selected · {}", format::size(selected_size)))
        });
        let list = if self.show_tree {
            DataTable::new(&self.tree).small().bordered(false).stripe(false).header_row_height(px(30.)).into_any_element()
        } else {
            DataTable::new(&self.table).small().bordered(false).stripe(false).header_row_height(px(30.)).into_any_element()
        };
        let grow = |weight: f32| v_flex().min_w_0().h_full().gap(px(16.)).flex_grow(weight).flex_basis(relative(0.));
        let left = grow(7.)
            .child(
                div()
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
        let right = grow(5.).child(if self.show_filters {
            self.filters.clone().into_any_element()
        } else {
            self.treemap.clone().into_any_element()
        });
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
                            "/" => this.query.focus_handle(cx).focus(window, cx),
                            _ => {}
                        }
                    }))
                    .child(
                        h_flex()
                            .h(px(32.))
                            .mb(px(8.))
                            .items_center()
                            .justify_between()
                            .child(div().text_size(px(24.)).line_height(px(32.)).child("Explorer"))
                            .child(h_flex().gap(px(12.)).items_center().children(selection).child(filters).child(trash)),
                    )
                    .child(self.tabs(summary, cx))
                    .child(h_flex().flex_1().min_h_0().gap(px(16.)).pt(px(16.)).pb(px(16.)).child(left).child(right)),
            )
            .child(self.status_bar(cx))
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::Dashboard => self.dashboard.clone().into_any_element(),
            Page::Explorer => self.explorer(cx).into_any_element(),
            Page::Settings => self.settings_page.clone().into_any_element(),
        };
        let p = palette(cx);
        // Problems have no status bar to sit in outside the Explorer.
        let problems = self.page != Page::Explorer
            && (!self.startup_problems.is_empty() || !self.engine.shared.status.lock().problems.is_empty());
        h_flex()
            .size_full()
            .bg(p.ground)
            .text_color(p.text)
            .font_features(tabular())
            .line_height(px(20.))
            .child(self.sidebar(cx))
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
