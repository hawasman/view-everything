//! The window: sidebar, the pages (Explorer with its Query bar, lists, panels and Treemap,
//! Settings) and the status bar.

use crate::engine::{Engine, Phase, Repaint, last_synced_label};
use crate::categories::CategoryPanel;
use crate::explorer::FlatList;
use crate::filters::FiltersPanel;
use crate::format;
use crate::settings::{Settings, ThemeChoice};
use crate::settings_page::{Changed, SettingsPage, apply_theme};
use crate::tree::TreeList;
use crate::trash::{self, Trash};
use crate::treemap::Treemap;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

const GEIST: &[u8] = include_bytes!("../assets/fonts/Geist-Variable.ttf");
const GEIST_MONO: &[u8] = include_bytes!("../assets/fonts/GeistMono-Variable.ttf");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Explorer,
    Settings,
}

/// Loads the Geist fonts and applies the theme choice; call once before opening the window.
pub fn init_theme(choice: ThemeChoice, cx: &mut App) {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(GEIST), Cow::Borrowed(GEIST_MONO)])
        .expect("embedded fonts are valid");
    let theme = Theme::global_mut(cx);
    theme.font_family = "Geist".into();
    theme.mono_font_family = "Geist Mono".into();
    apply_theme(choice, cx);
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
    trash: Trash,
    treemap: Entity<Treemap>,
    categories: Entity<CategoryPanel>,
    volumes: Vec<VolumeInfo>,
    startup_problems: Vec<String>,
    /// The Explorer lists' width in pixels, as of the last paint.
    list_width: Rc<Cell<f32>>,
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
        let engine = Engine::start(repaint.clone(), indexed, intervals, categories.clone());

        let trash = Trash::new(engine.shared.clone(), repaint);

        let settings_page = cx.new(|cx| SettingsPage::new(settings, volumes.clone(), window, cx));
        cx.subscribe(&settings_page, |this, _, changed: &Changed, cx| {
            this.engine.set_volumes(changed.indexed.clone());
            this.engine.set_intervals(changed.intervals.clone());
            // Saved fine, so the settings file read at startup has been replaced.
            if !this.settings_page.read(cx).has_problem() {
                this.startup_problems.retain(|p| !p.starts_with("Settings ignored"));
            }
            cx.notify();
        })
        .detach();

        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search names, or try size:>1gb type:video")
        });
        let filters = cx.new(|cx| FiltersPanel::new(window, cx));
        cx.subscribe_in(&query, window, |this, query, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Change) {
                let text = query.read(cx).value().to_string();
                this.filters.update(cx, |f, cx| f.reload(&text, window, cx));
                this.engine.set_query(text);
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
                .col_resizable(true)
                .select_on_modifier_click(false)
        });
        let treemap = cx.new(|cx| Treemap::new(engine.shared.clone(), categories.clone(), cx));
        let category_panel = cx.new(|_| CategoryPanel::new(query.clone()));

        let tree = cx.new(|cx| {
            TableState::new(TreeList::new(engine.shared.clone()), window, cx)
                .col_resizable(true)
                .select_on_modifier_click(false)
        });

        // Keyboard moves select a single row; clicks go through the lists' own handlers.
        cx.subscribe(&table, |_, table, event: &TableEvent, cx| {
            if let TableEvent::SelectRow(row) = event {
                table.update(cx, |t, _| {
                    if let Some(id) = t.delegate().entry_at(*row) {
                        t.delegate_mut().selection.only(*row, id);
                    }
                });
            }
            cx.notify();
        })
        .detach();
        cx.subscribe(&tree, |_, tree, event: &TableEvent, cx| {
            if let TableEvent::SelectRow(row) = event {
                tree.update(cx, |t, _| {
                    if let Some(id) = t.delegate().entry_at(*row) {
                        t.delegate_mut().selection.only(*row, id);
                    }
                });
            }
            cx.notify();
        })
        .detach();
        // A modifier-click changes the selection without the tables knowing.
        cx.observe(&table, |_, _, cx| cx.notify()).detach();
        cx.observe(&tree, |_, _, cx| cx.notify()).detach();

        #[cfg(feature = "bench")]
        if std::env::var_os("VE_BENCH").is_some() {
            crate::bench::start(table.clone(), window);
        }

        cx.spawn(async move |this, cx| {
            while woken.recv().await.is_ok() {
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        // Save the Index as the app quits.
        cx.on_app_quit(|this, _| {
            this.engine.shutdown();
            async {}
        })
        .detach();

        Self {
            engine,
            page: Page::Explorer,
            query,
            table,
            tree,
            show_tree: true,
            filters,
            show_filters: false,
            settings_page,
            trash,
            treemap,
            categories: category_panel,
            volumes,
            startup_problems,
            list_width: Rc::default(),
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
        });
        self.categories.update(cx, |panel, cx| panel.set_view(latest, cx));
        cx.notify();
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shared = &self.engine.shared;
        let status = shared.status.lock().clone();
        let progress = &shared.progress;
        let phase = match &status.phase {
            Phase::LoadingIndex => h_flex()
                .gap_2()
                .child(Spinner::new())
                .child("Loading the saved Index…")
                .into_any_element(),
            Phase::Syncing {
                volumes,
                catching_up,
                started,
            } => {
                let what = if *catching_up { "Catching up" } else { "Scanning" };
                let names: Vec<_> = volumes.iter().map(|v| v.display().to_string()).collect();
                h_flex()
                    .gap_2()
                    .child(Spinner::new())
                    .child(format!(
                        "{what} {} — {} entries read, {}s",
                        names.join(", "),
                        format::count(progress.entries()),
                        started.elapsed().as_secs()
                    ))
                    .into_any_element()
            }
            Phase::Idle => format!(
                "{} entries indexed",
                format::count(shared.index.read().live_count() as u64)
            )
            .into_any_element(),
        };
        let mut left = h_flex().gap_3().child(phase);
        let synced = {
            let oldest = shared.index.read().oldest_synced();
            last_synced_label(oldest, chrono::Utc::now().timestamp())
        };
        if let Some(label) = synced.filter(|_| !matches!(status.phase, Phase::LoadingIndex)) {
            left = left.child(format!("· {label}"));
        }
        if progress.unreadable() > 0 {
            left = left.child(format!(
                "· {} folders unreadable",
                format::count(progress.unreadable())
            ));
        }
        if let Some(view) = shared
            .view
            .lock()
            .as_ref()
            .filter(|v| !v.query.trim().is_empty())
        {
            left = left.child(format!(
                "· {} matched in {} ms",
                format::count(view.flat.len() as u64),
                view.took.as_millis()
            ));
        }
        let warn = cx.theme().warning;
        let problems = status
            .problems
            .into_iter()
            .map(|(_, message)| message)
            .chain(shared.query_error.lock().clone())
            .chain(self.startup_problems.iter().cloned());
        for problem in problems {
            left = left.child(div().text_color(warn).child(problem));
        }
        StatusBar::new()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_sm()
            .left(left)
            .right(self.trash_controls(cx))
    }

    fn trash_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let size = self.trash.size();
        let label = size.map_or_else(|| "Trash".to_owned(), |s| format!("{} in Trash", format::size(s)));
        let mut controls = h_flex().gap_2().child(
            Button::new("open-trash")
                .ghost()
                .xsmall()
                .label(label)
                .tooltip("Trashed files still take up space until the Trash is emptied. Click to open the Trash.")
                .on_click(cx.listener(|this, _, window, cx| this.trash.open(window, cx))),
        );
        if trash::CAN_EMPTY {
            controls = controls.child(
                Button::new("empty-trash")
                    .ghost()
                    .xsmall()
                    .label("Empty Trash")
                    .disabled(size == Some(0))
                    .on_click(cx.listener(|this, _, window, cx| this.trash.ask_empty(window, cx))),
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

    /// Lets the lists' name columns take the width left beside the Categories panel.
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

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = |label: &'static str, icon: IconName, page: Page, cx: &mut Context<Self>| {
            SidebarMenuItem::new(label)
                .icon(icon)
                .active(self.page == page)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
        };
        Sidebar::new("sidebar").child(
            SidebarGroup::new("view-everything").child(
                SidebarMenu::new()
                    .child(item("Dashboard", IconName::LayoutDashboard, Page::Dashboard, cx))
                    .child(item("Explorer", IconName::Folder, Page::Explorer, cx))
                    .child(item("Settings", IconName::Settings, Page::Settings, cx)),
            ),
        )
    }

    fn explorer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.fit_lists(cx);
        let list = if self.show_tree {
            DataTable::new(&self.tree).into_any_element()
        } else {
            DataTable::new(&self.table).into_any_element()
        };
        let selected = self.selected_entries(cx);
        let blocked = self.trash.blocker_of(&selected, &self.volumes);
        let width = self.list_width.clone();
        let measure = canvas(move |b, _, _| width.set(f32::from(b.size.width)), |_, _, _, _| {})
            .absolute()
            .size_full();
        let active = self.filters.read(cx).active();
        let label = if active > 0 { format!("Filters · {active}") } else { "Filters".into() };
        let toggle = Button::new("filters").label(label).on_click(cx.listener(|this, _, _, cx| {
            this.show_filters = !this.show_filters;
            cx.notify();
        }));
        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let focus = if this.show_tree {
                    this.tree.focus_handle(cx)
                } else {
                    this.table.focus_handle(cx)
                };
                if event.keystroke.key == "delete" && focus.is_focused(window) {
                    this.ask_trash_selected(window, cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.query).cleanable(true)))
                    .child(if self.show_filters { toggle.primary() } else { toggle })
                    .child(
                        Button::new("trash")
                            .icon(IconName::Delete)
                            .label("Move to Trash")
                            .danger()
                            .disabled(selected.is_empty() || blocked.is_some())
                            .when_some(blocked, |b, why| b.tooltip(why))
                            .on_click(cx.listener(|this, _, window, cx| this.ask_trash_selected(window, cx))),
                    ),
            )
            .when(self.show_filters, |c| c.child(self.filters.clone()))
            .child(
                TabBar::new("explorer-lists")
                    .segmented()
                    .selected_index(if self.show_tree { 0 } else { 1 })
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        this.show_tree = *ix == 0;
                        cx.notify();
                    }))
                    .child(Tab::new().label("Tree"))
                    .child(Tab::new().label("Flat list")),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h(px(150.))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(measure)
                            .child(list),
                    )
                    .child(div().w(px(280.)).h_full().child(self.categories.clone())),
            )
            // Open Filters need the room; the Treemap comes back when they close.
            .when(!self.show_filters, |c| c.child(div().h(px(340.)).child(self.treemap.clone())))
    }

    fn dashboard(&self) -> impl IntoElement {
        let index = self.engine.shared.index.read();
        let volumes = index.volumes().count();
        v_flex().p_4().gap_2().text_lg().child(format!(
            "{} entries on {} Volume(s)",
            format::count(index.live_count() as u64),
            volumes
        ))
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::Dashboard => self.dashboard().into_any_element(),
            Page::Explorer => self.explorer(cx).into_any_element(),
            Page::Settings => self.settings_page.clone().into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(page)),
            )
            .child(self.status_bar(cx))
    }
}
