//! The window: sidebar, Query bar, Explorer flat list and status bar.

use crate::engine::{Engine, Phase, Repaint};
use crate::explorer::FlatList;
use crate::format;
use crate::settings::{Settings, ThemeChoice};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, Theme, ThemeMode, h_flex, v_flex};
use gpui_kit::*;
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

/// How often the window looks for changes the Engine's threads have reported.
const POLL: Duration = Duration::from_millis(50);

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
    match choice {
        ThemeChoice::System => Theme::sync_system_appearance(None, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

pub struct Shell {
    engine: Engine,
    page: Page,
    query: Entity<InputState>,
    table: Entity<TableState<FlatList>>,
    /// Set by the Engine's threads; cleared when the window has caught up.
    changed: Arc<AtomicBool>,
    settings: Settings,
    volumes: Vec<VolumeInfo>,
    startup_problems: Vec<String>,
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
        let refresh = Duration::from_secs(settings.refresh_minutes.max(1) * 60);
        let changed = Arc::new(AtomicBool::new(true));
        let repaint: Repaint = {
            let changed = changed.clone();
            Arc::new(move || changed.store(true, Ordering::Relaxed))
        };
        let engine = Engine::start(repaint, indexed, refresh, categories);

        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search names, or try size:>1gb type:video")
        });
        cx.subscribe(&query, |this, query, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.engine.set_query(query.read(cx).value().to_string());
            }
        })
        .detach();
        let list = FlatList::new(engine.shared.clone());
        let table = cx.new(|cx| TableState::new(list, window, cx).col_resizable(true));

        if std::env::var_os("VE_BENCH").is_some() {
            crate::bench::start(table.clone(), window);
        }

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
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
            changed,
            settings,
            volumes,
            startup_problems,
        }
    }

    /// Picks up what the Engine's threads changed since the last look.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let busy = !matches!(self.engine.shared.status.lock().phase, Phase::Idle);
        if !self.changed.swap(false, Ordering::Relaxed) && !busy {
            return;
        }
        let latest = self.engine.shared.view.lock().clone();
        self.table.update(cx, |table, cx| {
            let current = &table.delegate().view;
            let same = match (current, &latest) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                table.delegate_mut().view = latest;
                table.refresh(cx);
            }
        });
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
            .problem
            .into_iter()
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

    fn explorer(&self) -> impl IntoElement {
        v_flex()
            .size_full()
            .gap_2()
            .p_3()
            .child(Input::new(&self.query).cleanable(true))
            .child(div().flex_1().min_h_0().child(DataTable::new(&self.table)))
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

    fn settings_page(&self) -> impl IntoElement {
        v_flex().p_4().gap_2().children(self.volumes.iter().map(|v| {
            let state = if self.settings.is_indexed(v) { "indexed" } else { "not indexed" };
            format!("{} — {state}", v.mount.display())
        }))
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match self.page {
            Page::Dashboard => self.dashboard().into_any_element(),
            Page::Explorer => self.explorer().into_any_element(),
            Page::Settings => self.settings_page().into_any_element(),
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
