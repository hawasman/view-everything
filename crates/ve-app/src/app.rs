//! The window: Query bar, folder tree / flat list, Category breakdown, Treemap and status bar.

use crate::engine::{Engine, Phase};
use crate::filters::Filters;
use crate::format;
use crate::settings::{Settings, ThemeChoice};
use crate::theme;
use crate::trash;
use crate::treemap::{self, Tile};
use crate::view::View;
use egui::{Align, Id, Key, Layout, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use ve_core::{Category, CategoryMap, EntryId, Index};
use ve_scan::VolumeInfo;

const ROW_HEIGHT: f32 = 22.0;
const HEADER_HEIGHT: f32 = 32.0;
const INDENT: f32 = 16.0;
const NOTICE_TIME: Duration = Duration::from_secs(8);
/// Widths at which every column of the tree and of the flat list is shown at its initial size
/// (the Name / Folder minimum plus the fixed columns and the gaps between them). Narrower, the
/// table scrolls sideways.
const TREE_MIN_WIDTH: f32 = 240.0 + 170.0 + 90.0 + 60.0 + 90.0 + 130.0 + 5.0 * 8.0;
const FLAT_MIN_WIDTH: f32 = 240.0 + 160.0 + 170.0 + 90.0 + 60.0 + 90.0 + 130.0 + 6.0 * 8.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Tree,
    Flat,
}

/// The tree's sort column and direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sort {
    key: SortKey,
    descending: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SortKey {
    Name,
    OnDisk,
    Logical,
    Files,
    Modified,
}

/// A tree row: an Entry at a nesting depth.
#[derive(Clone, Copy)]
struct Row {
    id: EntryId,
    depth: usize,
}

/// Something a click asked for, applied after the frame's drawing.
enum Action {
    Select { id: EntryId, add: bool },
    Toggle(EntryId),
    ShowInTree(EntryId),
    ZoomTo(EntryId),
    Reveal(EntryId),
    CopyPath(EntryId),
    AskTrash(Vec<EntryId>),
    AddToQuery(String),
}

enum Dialog {
    ConfirmTrash {
        items: Vec<PathBuf>,
        total: u64,
    },
    ConfirmEmptyTrash {
        size: Option<u64>,
    },
    /// Empty Trash left items the user may not delete; offers to delete them as root.
    DeleteAsAdmin {
        failures: Vec<trash::PurgeFailure>,
    },
    Volumes {
        choices: Vec<(VolumeInfo, bool)>,
        refresh_minutes: u64,
    },
}

/// What the user confirmed in a dialog.
enum Confirmed {
    Trash(Vec<PathBuf>),
    EmptyTrash,
    DeleteAsAdmin(Vec<PathBuf>),
    Volumes(Vec<(VolumeInfo, bool)>, u64),
}

enum Job {
    Trashed {
        moved: Vec<PathBuf>,
        missing: Vec<PathBuf>,
        error: Option<String>,
    },
    Emptied(Result<trash::EmptyOutcome, String>),
    AdminDeleted(Result<(), String>),
    TrashSize(Option<u64>),
}

pub struct App {
    engine: Engine,
    ctx: egui::Context,
    categories: CategoryMap,
    settings: Settings,
    volumes: Vec<VolumeInfo>,
    query: String,
    tab: Tab,
    expanded: HashSet<EntryId>,
    expanded_version: u64,
    selection: Vec<EntryId>,
    sort: Sort,
    rows: Vec<Row>,
    /// The View id, expansion state and sort the tree rows were built for.
    rows_key: Option<(u64, u64, Sort)>,
    scroll_to: Option<EntryId>,
    zoom: Option<EntryId>,
    /// The newest finished Treemap tiles; may trail the current View by one layout.
    tiles: Vec<Tile>,
    layouter: treemap::Layouter,
    treemap_menu: Option<EntryId>,
    open_categories: HashSet<Category>,
    dialog: Option<Dialog>,
    /// The Filters window's form while it is open.
    filters: Option<Filters>,
    /// Fields other than Name the current Query sets, shown on the Filters button.
    filter_count: usize,
    notice: Option<(String, Instant)>,
    trash_size: Option<u64>,
    jobs: Receiver<Job>,
    job_sender: Sender<Job>,
}

impl App {
    pub fn new(
        ctx: &egui::Context,
        settings: Settings,
        categories: CategoryMap,
        volumes: Vec<VolumeInfo>,
        startup_problems: Vec<String>,
    ) -> App {
        let indexed: Vec<VolumeInfo> = volumes
            .iter()
            .filter(|v| settings.is_indexed(v))
            .cloned()
            .collect();
        let refresh = Duration::from_secs(settings.refresh_minutes.max(1) * 60);
        let engine = Engine::start(ctx.clone(), indexed, refresh, categories.clone());
        let layouter =
            treemap::Layouter::start(ctx.clone(), engine.shared.clone(), categories.clone());
        let (job_sender, jobs) = mpsc::channel();
        let app = App {
            engine,
            ctx: ctx.clone(),
            categories,
            settings,
            volumes,
            query: String::new(),
            tab: Tab::Tree,
            expanded: HashSet::new(),
            expanded_version: 0,
            selection: Vec::new(),
            sort: Sort {
                key: SortKey::OnDisk,
                descending: true,
            },
            rows: Vec::new(),
            rows_key: None,
            scroll_to: None,
            zoom: None,
            tiles: Vec::new(),
            layouter,
            treemap_menu: None,
            open_categories: HashSet::new(),
            dialog: None,
            filters: None,
            filter_count: 0,
            notice: startup_problems
                .into_iter()
                .reduce(|a, b| format!("{a}; {b}"))
                .map(|p| (p, Instant::now())),
            trash_size: None,
            jobs,
            job_sender,
        };
        app.measure_trash(ctx);
        app
    }

    fn measure_trash(&self, ctx: &egui::Context) {
        let sender = self.job_sender.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(Job::TrashSize(trash::trash_size()));
            ctx.request_repaint();
        });
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.receive_jobs(&ctx);
        let view = self.engine.shared.view.lock().clone();
        let mut actions = Vec::new();
        {
            let shared = self.engine.shared.clone();
            let index = shared.index.read();
            self.query_bar(ui);
            self.status_bar(ui, &index, view.as_deref());
            if let Some(view) = &view {
                self.treemap_panel(ui, &index, view, &mut actions);
                self.breakdown_panel(ui, view, &mut actions);
            }
            egui::CentralPanel::default().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Tree, "Tree");
                    ui.selectable_value(&mut self.tab, Tab::Flat, "Flat list");
                });
                ui.separator();
                match &view {
                    None => {
                        ui.centered_and_justified(|ui| ui.spinner());
                    }
                    // Too narrow for every column: the table scrolls sideways instead of clipping.
                    Some(view) => {
                        egui::ScrollArea::horizontal().id_salt("table-x").show(ui, |ui| {
                            let min = match self.tab {
                                Tab::Tree => TREE_MIN_WIDTH,
                                Tab::Flat => FLAT_MIN_WIDTH,
                            };
                            ui.set_width(ui.available_width().max(min));
                            match self.tab {
                                Tab::Tree => self.tree_table(ui, &index, view, &mut actions),
                                Tab::Flat => self.flat_table(ui, &index, view, &mut actions),
                            }
                        });
                    }
                }
            });
            self.keyboard(ui, &mut actions);
            self.apply(&ctx, &index, actions);
        }
        self.filters_window(&ctx);
        self.dialogs(&ctx);
        if self.engine.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn on_exit(&mut self) {
        self.engine.shutdown();
    }
}

impl App {
    fn receive_jobs(&mut self, ctx: &egui::Context) {
        while let Ok(job) = self.jobs.try_recv() {
            match job {
                Job::TrashSize(size) => self.trash_size = size,
                Job::Trashed {
                    moved,
                    missing,
                    error,
                } => {
                    {
                        let mut index = self.engine.shared.index.write();
                        for path in moved.iter().chain(&missing) {
                            if let Some(id) = index.find_path(path) {
                                index.remove(id);
                            }
                        }
                    }
                    self.selection.clear();
                    let mut text = match moved.len() {
                        0 => String::new(),
                        1 => format!("Moved {} to the Trash.", moved[0].display()),
                        n => format!("Moved {n} items to the Trash."),
                    };
                    if !missing.is_empty() {
                        text.push_str(&format!(
                            " {} item(s) were already gone and left the Index.",
                            missing.len()
                        ));
                    }
                    if let Some(error) = error {
                        text.push_str(&format!(" Nothing was moved: {error}"));
                    }
                    self.notify(text.trim().to_owned());
                    self.measure_trash(ctx);
                }
                Job::Emptied(Err(e)) => {
                    self.notify(format!("Could not empty the Trash: {e}"));
                    self.measure_trash(ctx);
                }
                Job::Emptied(Ok(outcome)) => {
                    let (needs_admin, other): (Vec<_>, Vec<_>) = outcome
                        .failed
                        .into_iter()
                        .partition(|f| f.admin_paths.is_some());
                    let mut text = match outcome.purged {
                        0 => String::new(),
                        n => format!("Permanently deleted {n} item(s) from the Trash."),
                    };
                    if let Some(first) = other.first() {
                        text.push_str(&format!(
                            " {} item(s) could not be deleted: {} ({})",
                            other.len(),
                            first.original.display(),
                            first.error
                        ));
                    }
                    if !text.is_empty() {
                        self.notify(text.trim_start().to_owned());
                    }
                    if !needs_admin.is_empty() {
                        self.dialog = Some(Dialog::DeleteAsAdmin {
                            failures: needs_admin,
                        });
                    }
                    self.measure_trash(ctx);
                }
                Job::AdminDeleted(result) => {
                    match result {
                        Ok(()) => {
                            self.notify("Deleted the remaining Trash items as administrator.")
                        }
                        Err(e) => self.notify(format!("Left in the Trash: {e}")),
                    }
                    self.measure_trash(ctx);
                }
            }
        }
    }

    fn query_bar(&mut self, ui: &mut Ui) {
        egui::Panel::top("query").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let busy = self.engine.is_busy();
                if ui.add_enabled(!busy, egui::Button::new("⟳ Refresh")).on_hover_text("Re-read every Volume (F5)").clicked() {
                    self.engine.refresh();
                }
                if ui.button("Volumes…").clicked() {
                    self.open_volumes_dialog();
                }
                let label = match self.filter_count {
                    0 => "Filters…".to_owned(),
                    n => format!("Filters · {n}"),
                };
                if ui
                    .add(egui::Button::new(label).selected(self.filters.is_some()))
                    .on_hover_text("Build the Query from fields")
                    .clicked()
                {
                    self.filters = match self.filters {
                        Some(_) => None,
                        None => Some(Filters::read(&self.query)),
                    };
                }
                ui.label("?").on_hover_text(QUERY_HELP);
                // The theme switch and the clear button are laid out first, from the right, so
                // the search field takes the remaining width.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.theme_switch(ui);
                    if !self.query.is_empty()
                        && ui
                            .add(egui::Button::new("✕").frame(false))
                            .on_hover_text("Clear the Query and every filter")
                            .clicked()
                    {
                        self.query.clear();
                        self.query_edited();
                    }
                    // A fixed id keeps focus while the clear button comes and goes beside it.
                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .id_salt("query")
                        .hint_text("Search — e.g.  node_modules   ext:mp4;mkv   size:>1gb   dm:2025   type:video   !*.tmp")
                        .desired_width(f32::INFINITY);
                    if ui.add(edit).changed() {
                        self.query_edited();
                    }
                });
            });
            if let Some(error) = self.engine.shared.query_error.lock().clone() {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(2.0);
        });
    }

    /// After the Query text changed anywhere but the Filters window: recount it, rebuild an
    /// open form from it, and send it to the engine.
    fn query_edited(&mut self) {
        let form = Filters::read(&self.query);
        self.filter_count = form.active();
        if self.filters.is_some() {
            self.filters = Some(form);
        }
        self.engine.set_query(self.query.clone());
    }

    /// The Filters window: edits apply as they are made, and the window stays open beside the
    /// results.
    fn filters_window(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.filters else {
            return;
        };
        let mut open = true;
        let mut changed = false;
        egui::Window::new("Filters")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            // Below the Query bar on the right, so the search text stays in view.
            .pivot(egui::Align2::RIGHT_TOP)
            .default_pos(ctx.content_rect().right_top() + egui::vec2(-16.0, 56.0))
            .show(ctx, |ui| {
                changed = form.ui(ui);
                ui.add_space(12.0);
                if ui.button("Clear").on_hover_text("Untick and empty every field").clicked() {
                    *form = Filters::read("");
                    changed = true;
                }
            });
        if changed {
            self.query = form.write();
            self.filter_count = form.active();
            self.engine.set_query(self.query.clone());
        }
        if !open {
            self.filters = None;
        }
    }

    /// A sun or moon button for the mode on screen; its menu picks Light, Dark or System.
    fn theme_switch(&mut self, ui: &mut Ui) {
        let icon = if ui.visuals().dark_mode { "🌙" } else { "☀" };
        let menu = egui::containers::menu::MenuButton::new(icon);
        let response = menu.ui(ui, |ui| {
            for (choice, label) in [
                (ThemeChoice::Light, "☀  Light"),
                (ThemeChoice::Dark, "🌙  Dark"),
                (ThemeChoice::System, "💻  System"),
            ] {
                if ui.selectable_label(self.settings.theme == choice, label).clicked() {
                    self.settings.theme = choice;
                    ui.ctx().set_theme(choice);
                    if let Err(e) = self.settings.save() {
                        self.notify(format!("Could not save settings: {e}"));
                    }
                }
            }
        });
        response.0.on_hover_text("Light or dark");
    }

    fn status_bar(&mut self, ui: &mut Ui, index: &Index, view: Option<&View>) {
        egui::Panel::bottom("status").show(ui, |ui| {
            // The Trash controls are laid out first so the status text truncates before them.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if trash::CAN_EMPTY
                    && ui.add_enabled(self.trash_size != Some(0), egui::Button::new("Empty Trash")).clicked()
                {
                    self.dialog = Some(Dialog::ConfirmEmptyTrash { size: self.trash_size });
                }
                let label = match self.trash_size {
                    Some(size) => format!("{} in Trash", format::size(size)),
                    None => "Trash".to_owned(),
                };
                let link = ui.link(label).on_hover_text(
                    "Trashed files still take up space until the Trash is emptied. Click to open the Trash.",
                );
                if link.clicked()
                    && let Err(e) = trash::open_trash()
                {
                    self.notify(format!("Could not open the Trash: {e}"));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    let status = self.engine.shared.status.lock().clone();
                    let progress = &self.engine.shared.progress;
                    match &status.phase {
                        Phase::LoadingIndex => {
                            ui.add(egui::Spinner::new().size(14.0));
                            ui.label("Loading the saved Index…");
                        }
                        Phase::Syncing { volumes, catching_up, started } => {
                            ui.add(egui::Spinner::new().size(14.0));
                            let what = if *catching_up { "Catching up" } else { "Scanning" };
                            let names: Vec<String> = volumes.iter().map(|v| v.display().to_string()).collect();
                            ui.label(format!(
                                "{what} {} — {} entries read, {}s",
                                names.join(", "),
                                format::count(progress.entries()),
                                started.elapsed().as_secs()
                            ));
                        }
                        Phase::Idle => {
                            ui.label(format!("{} entries indexed", format::count(index.live_count() as u64)));
                        }
                    }
                    if progress.unreadable() > 0 {
                        ui.label(format!("· {} folders unreadable", format::count(progress.unreadable())))
                            .on_hover_text("Folders the current user may not open; their contents are not counted.");
                    }
                    if let Some(view) = view.filter(|v| !v.query.trim().is_empty()) {
                        ui.label(format!(
                            "· {} matched in {} ms",
                            format::count(view.flat.len() as u64),
                            view.took.as_millis()
                        ));
                    }
                    if let Some(problem) = status.problem {
                        ui.colored_label(ui.visuals().warn_fg_color, problem);
                    }
                    if let Some((text, at)) = &self.notice {
                        if at.elapsed() < NOTICE_TIME {
                            ui.label(RichText::new(text).strong());
                        } else {
                            self.notice = None;
                        }
                    }
                });
            });
        });
    }

    fn breakdown_panel(&mut self, ui: &mut Ui, view: &View, actions: &mut Vec<Action>) {
        egui::Panel::right("breakdown")
            .resizable(true)
            .default_size(270.0)
            .show(ui, |ui| {
                ui.heading("Categories");
                ui.label(format!(
                    "{} on disk in {} files",
                    format::size(view.total.on_disk),
                    format::count(view.total.files)
                ));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for row in &view.categories {
                        let open = self.open_categories.contains(&row.category);
                        ui.horizontal(|ui| {
                            let chevron =
                                RichText::new(if open { "▾" } else { "▸" }).color(theme::palette(ui).muted);
                            if ui.add(egui::Button::new(chevron).frame(false).small()).clicked() {
                                if open {
                                    self.open_categories.remove(&row.category);
                                } else {
                                    self.open_categories.insert(row.category);
                                }
                            }
                            let (swatch, _) =
                                ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
                            ui.painter().rect_filled(
                                swatch,
                                2.0,
                                treemap::category_color(row.category),
                            );
                            if ui
                                .link(row.category.name())
                                .on_hover_text("Show only this Category")
                                .clicked()
                            {
                                actions.push(Action::AddToQuery(format!(
                                    "type:{}",
                                    row.category.name().to_lowercase()
                                )));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.monospace(format::size(row.totals.on_disk))
                                    .on_hover_text(format!(
                                        "{} files",
                                        format::count(row.totals.files)
                                    ));
                            });
                        });
                        if open {
                            for (ext, totals) in &row.extensions {
                                ui.horizontal(|ui| {
                                    ui.add_space(INDENT * 2.0);
                                    if ext == "(none)" {
                                        ui.label(ext);
                                    } else if ui.link(format!(".{ext}")).clicked() {
                                        actions.push(Action::AddToQuery(format!("ext:{ext}")));
                                    }
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.monospace(format::size(totals.on_disk));
                                    });
                                });
                            }
                        }
                    }
                });
            });
    }

    fn treemap_panel(
        &mut self,
        ui: &mut Ui,
        index: &Index,
        view: &Arc<View>,
        actions: &mut Vec<Action>,
    ) {
        egui::Panel::bottom("treemap")
            .resizable(true)
            .default_size(360.0)
            .min_size(120.0)
            .show(ui, |ui| {
                let Some(root) = self.treemap_root(index, view) else {
                    ui.label("No Volume indexed yet.");
                    // Claim the whole panel, or it remembers this one line as its height.
                    ui.allocate_space(ui.available_size());
                    return;
                };
                ui.horizontal(|ui| {
                    let mut chain = vec![root];
                    while let Some(parent) = index.parent(*chain.last().unwrap()) {
                        chain.push(parent);
                    }
                    for (i, &id) in chain.iter().rev().enumerate() {
                        if i > 0 {
                            ui.label("›");
                        }
                        if ui.link(index.name(id)).clicked() {
                            actions.push(Action::ZoomTo(id));
                        }
                    }
                    ui.label(
                        RichText::new(format::size(view.filter.totals(index, root).on_disk)).weak(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("double-click a folder to zoom in").weak());
                    });
                });
                let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
                let rect = response.rect;
                self.layouter.request(view, root, rect);
                if let Some(tiles) = self.layouter.finished() {
                    self.tiles = tiles;
                }
                treemap::paint(&painter, theme::palette(ui), &self.tiles, self.selection.first().copied());

                let hovered = response
                    .hover_pos()
                    .and_then(|pos| treemap::hit(&self.tiles, pos))
                    .map(|t| t.id);
                if let Some(id) = hovered {
                    let response = response.clone();
                    response.on_hover_ui_at_pointer(|ui| {
                        ui.label(RichText::new(index.path(id).display().to_string()).strong());
                        let totals = view.filter.totals(index, id);
                        ui.label(format!(
                            "{} on disk · {} size",
                            format::size(totals.on_disk),
                            format::size(totals.logical)
                        ));
                        if index.is_dir(id) {
                            ui.label(format!("{} files", format::count(totals.files)));
                        } else {
                            ui.label(self.categories.of_name(index.name(id)).name());
                        }
                    });
                }
                if response.double_clicked()
                    && let Some(id) = hovered
                {
                    if let Some(folder) = folder_of(index, id) {
                        actions.push(Action::ZoomTo(folder));
                    }
                } else if response.clicked()
                    && let Some(id) = hovered
                {
                    actions.push(Action::Select { id, add: false });
                    actions.push(Action::ShowInTree(id));
                }
                if response.secondary_clicked() {
                    self.treemap_menu = hovered;
                }
                if let Some(id) = self.treemap_menu {
                    response.context_menu(|ui| self.entry_menu(ui, index, id, actions));
                }
            });
    }

    /// The folder the Treemap shows: the zoom target, else the Volume of the selection, else
    /// the first Volume.
    fn treemap_root(&self, index: &Index, view: &View) -> Option<EntryId> {
        let usable =
            |id: EntryId| index.is_live(id) && index.is_dir(id) && view.filter.is_shown(id);
        self.zoom
            .filter(|&id| usable(id))
            .or_else(|| {
                self.selection
                    .first()
                    .map(|&id| index.root_of(id))
                    .filter(|&id| usable(id))
            })
            .or_else(|| {
                index
                    .volumes()
                    .map(|(_, v)| v.root)
                    .filter(|&id| usable(id))
                    .max_by_key(|&id| view.filter.totals(index, id).on_disk)
            })
            .or_else(|| index.volumes().map(|(_, v)| v.root).next())
    }

    fn rebuild_rows(&mut self, index: &Index, view: &Arc<View>) {
        let key = (view.id, self.expanded_version, self.sort);
        if self.rows_key == Some(key) {
            return;
        }
        self.rows.clear();
        let mut roots: Vec<EntryId> = index
            .volumes()
            .map(|(_, v)| v.root)
            .filter(|&root| view.filter.is_shown(root))
            .collect();
        self.sort_entries(index, view, &mut roots);
        let mut stack: Vec<Row> = roots
            .into_iter()
            .rev()
            .map(|id| Row { id, depth: 0 })
            .collect();
        while let Some(row) = stack.pop() {
            self.rows.push(row);
            if self.expanded.contains(&row.id) {
                let mut children: Vec<EntryId> = index
                    .children(row.id)
                    .iter()
                    .copied()
                    .filter(|&c| view.filter.is_shown(c))
                    .collect();
                self.sort_entries(index, view, &mut children);
                stack.extend(children.into_iter().rev().map(|id| Row {
                    id,
                    depth: row.depth + 1,
                }));
            }
        }
        self.rows_key = Some(key);
    }

    fn sort_entries(&self, index: &Index, view: &View, ids: &mut [EntryId]) {
        let Sort { key, descending } = self.sort;
        let totals = |id: EntryId| view.filter.totals(index, id);
        match key {
            SortKey::Name => ids.sort_by_cached_key(|&id| index.name(id).to_lowercase()),
            SortKey::OnDisk => ids.sort_by_key(|&id| totals(id).on_disk),
            SortKey::Logical => ids.sort_by_key(|&id| totals(id).logical),
            SortKey::Files => ids.sort_by_key(|&id| totals(id).files),
            SortKey::Modified => ids.sort_by_key(|&id| index.modified(id)),
        }
        if descending {
            ids.reverse();
        }
    }

    fn sort_header(&mut self, ui: &mut Ui, key: SortKey, label: &str, numeric: bool) {
        let active = self.sort.key == key;
        let marker = match (active, self.sort.descending) {
            (true, true) => " ⏷",
            (true, false) => " ⏶",
            (false, _) => "",
        };
        let p = theme::palette(ui);
        let color = if active { p.text } else { p.muted };
        let button = egui::Button::new(RichText::new(format!("{label}{marker}")).small().color(color))
            .frame(false)
            .small();
        let response = if numeric {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(button)).inner
        } else {
            ui.add(button)
        };
        if response.clicked()
        {
            self.sort = if self.sort.key == key {
                Sort {
                    key,
                    descending: !self.sort.descending,
                }
            } else {
                // Sizes and counts read best largest first; names alphabetically.
                Sort {
                    key,
                    descending: key != SortKey::Name,
                }
            };
        }
    }

    fn tree_table(
        &mut self,
        ui: &mut Ui,
        index: &Index,
        view: &Arc<View>,
        actions: &mut Vec<Action>,
    ) {
        self.rebuild_rows(index, view);
        let scroll_row = self
            .scroll_to
            .take()
            .and_then(|id| self.rows.iter().position(|r| r.id == id));
        let mut table = TableBuilder::new(ui)
            .id_salt("tree")
            .striped(false)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(240.0).clip(true))
            .column(Column::initial(170.0).at_least(90.0))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(60.0).at_least(40.0))
            .column(Column::initial(90.0).at_least(50.0))
            .column(Column::initial(130.0).at_least(80.0));
        if let Some(row) = scroll_row {
            table = table.scroll_to_row(row, Some(Align::Center));
        }
        let rows = std::mem::take(&mut self.rows);
        table
            .header(HEADER_HEIGHT, |mut header| {
                header.col(|ui| self.sort_header(ui, SortKey::Name, "Name", false));
                header.col(|ui| self.sort_header(ui, SortKey::OnDisk, "On disk", true));
                header.col(|ui| self.sort_header(ui, SortKey::Logical, "Size", true));
                header.col(|ui| column_header(ui, "%", true));
                header.col(|ui| self.sort_header(ui, SortKey::Files, "Files", true));
                header.col(|ui| self.sort_header(ui, SortKey::Modified, "Modified", true));
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, rows.len(), |mut table_row| {
                    let Row { id, depth } = rows[table_row.index()];
                    if !index.is_live(id) {
                        return;
                    }
                    let totals = view.filter.totals(index, id);
                    let share = share_of_parent(index, view, id);
                    table_row.set_selected(self.selection.contains(&id));
                    table_row.col(|ui| {
                        ui.add_space(depth as f32 * INDENT);
                        let has_children =
                            index.children(id).iter().any(|&c| view.filter.is_shown(c));
                        if has_children {
                            let open = self.expanded.contains(&id);
                            let chevron =
                                RichText::new(if open { "▾" } else { "▸" }).color(theme::palette(ui).muted);
                            if ui
                                .add_sized([18.0, ROW_HEIGHT], egui::Button::new(chevron).frame(false).small())
                                .clicked()
                            {
                                actions.push(Action::Toggle(id));
                            }
                        } else {
                            ui.add_space(18.0);
                        }
                        let icon = if index.is_dir(id) { "🗀 " } else { "  " };
                        let mut name = RichText::new(format!("{icon}{}", index.name(id)));
                        // With a Query, folders shown only for what they contain stay grey.
                        if !view.query.trim().is_empty() && !view.filter.is_matched(id) {
                            name = name.color(theme::palette(ui).muted);
                        }
                        ui.add(egui::Label::new(name).truncate().selectable(false));
                    });
                    table_row.col(|ui| size_bar(ui, totals.on_disk, share));
                    table_row.col(|ui| figure(ui, format::size(totals.logical)));
                    table_row.col(|ui| figure(ui, format!("{:.1}%", share * 100.0)));
                    table_row.col(|ui| {
                        figure(ui, if index.is_dir(id) { format::count(totals.files) } else { String::new() })
                    });
                    table_row.col(|ui| figure(ui, format::time(index.modified(id))));
                    let response = table_row.response();
                    self.row_interaction(&response, index, id, true, actions);
                });
            });
        self.rows = rows;
    }

    fn flat_table(
        &mut self,
        ui: &mut Ui,
        index: &Index,
        view: &Arc<View>,
        actions: &mut Vec<Action>,
    ) {
        TableBuilder::new(ui)
            .id_salt("flat")
            .striped(false)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::initial(240.0).at_least(120.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::initial(170.0).at_least(90.0))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(60.0).at_least(40.0))
            .column(Column::initial(90.0).at_least(50.0))
            .column(Column::initial(130.0).at_least(80.0))
            .header(HEADER_HEIGHT, |mut header| {
                for (title, numeric) in [
                    ("Name", false),
                    ("Folder", false),
                    ("On disk", true),
                    ("Size", true),
                    ("%", true),
                    ("Files", true),
                    ("Modified", true),
                ] {
                    header.col(|ui| column_header(ui, title, numeric));
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, view.flat.len(), |mut table_row| {
                    let id = view.flat[table_row.index()];
                    if !index.is_live(id) {
                        return;
                    }
                    let totals = view.filter.totals(index, id);
                    let share = share_of_parent(index, view, id);
                    table_row.set_selected(self.selection.contains(&id));
                    table_row.col(|ui| {
                        let icon = if index.is_dir(id) { "🗀 " } else { "" };
                        ui.add(
                            egui::Label::new(format!("{icon}{}", index.name(id)))
                                .truncate()
                                .selectable(false),
                        );
                    });
                    table_row.col(|ui| {
                        let folder = index
                            .parent(id)
                            .map(|p| index.path(p).display().to_string())
                            .unwrap_or_default();
                        ui.add(
                            egui::Label::new(RichText::new(folder).weak())
                                .truncate()
                                .selectable(false),
                        );
                    });
                    table_row.col(|ui| size_bar(ui, totals.on_disk, share));
                    table_row.col(|ui| figure(ui, format::size(totals.logical)));
                    table_row.col(|ui| figure(ui, format!("{:.1}%", share * 100.0)));
                    table_row.col(|ui| {
                        figure(ui, if index.is_dir(id) { format::count(totals.files) } else { String::new() })
                    });
                    table_row.col(|ui| figure(ui, format::time(index.modified(id))));
                    let response = table_row.response();
                    self.row_interaction(&response, index, id, false, actions);
                });
            });
    }

    fn row_interaction(
        &self,
        response: &egui::Response,
        index: &Index,
        id: EntryId,
        in_tree: bool,
        actions: &mut Vec<Action>,
    ) {
        if response.double_clicked() {
            if in_tree && index.is_dir(id) {
                actions.push(Action::Toggle(id));
            } else {
                actions.push(Action::ShowInTree(id));
            }
        } else if response.clicked() {
            let add = response.ctx.input(|i| i.modifiers.command);
            actions.push(Action::Select { id, add });
        }
        if response.secondary_clicked() && !self.selection.contains(&id) {
            actions.push(Action::Select { id, add: false });
        }
        response.context_menu(|ui| self.entry_menu(ui, index, id, actions));
    }

    /// The right-click menu for an Entry (or the selection containing it).
    fn entry_menu(&self, ui: &mut Ui, index: &Index, id: EntryId, actions: &mut Vec<Action>) {
        let targets: Vec<EntryId> = if self.selection.contains(&id) {
            self.selection.clone()
        } else {
            vec![id]
        };
        let blocked = targets.iter().find_map(|&t| self.trash_blocker(index, t));
        let label = if targets.len() > 1 {
            format!("Move {} items to Trash", targets.len())
        } else {
            "Move to Trash".into()
        };
        let trash = ui.add_enabled(blocked.is_none(), egui::Button::new(label));
        if let Some(reason) = &blocked {
            trash.on_disabled_hover_text(reason);
        } else if trash.clicked() {
            actions.push(Action::AskTrash(targets));
            ui.close();
        }
        if ui.button("Reveal in file manager").clicked() {
            actions.push(Action::Reveal(id));
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            actions.push(Action::CopyPath(id));
            ui.close();
        }
        if let Some(folder) = folder_of(index, id)
            && ui.button("Zoom treemap here").clicked()
        {
            actions.push(Action::ZoomTo(folder));
            ui.close();
        }
    }

    /// Why `id` can't go to the Trash, if it can't.
    fn trash_blocker(&self, index: &Index, id: EntryId) -> Option<String> {
        if index.parent(id).is_none() {
            return Some("A whole Volume can't be moved to the Trash.".into());
        }
        let volume = index.volume(index.volume_of(id)?)?;
        let info = self.volumes.iter().find(|v| v.mount == volume.mount)?;
        info.read_only.then(|| {
            format!(
                "{} is mounted read-only, so nothing on it can go to the Trash.",
                info.mount.display()
            )
        })
    }

    fn keyboard(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let (delete, refresh) = ui.input(|i| (i.key_pressed(Key::Delete), i.key_pressed(Key::F5)));
        if delete
            && !self.selection.is_empty()
            && self.dialog.is_none()
            && !ui.ctx().egui_wants_keyboard_input()
        {
            actions.push(Action::AskTrash(self.selection.clone()));
        }
        if refresh && !self.engine.is_busy() {
            self.engine.refresh();
        }
    }

    fn apply(&mut self, ctx: &egui::Context, index: &Index, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Select { id, add } => {
                    if !add {
                        self.selection.clear();
                    }
                    if let Some(pos) = self.selection.iter().position(|&s| s == id) {
                        self.selection.remove(pos);
                    } else {
                        self.selection.push(id);
                    }
                }
                Action::Toggle(id) => {
                    if !self.expanded.remove(&id) {
                        self.expanded.insert(id);
                    }
                    self.expanded_version += 1;
                }
                Action::ShowInTree(id) => {
                    let mut current = id;
                    while let Some(parent) = index.parent(current) {
                        self.expanded.insert(parent);
                        current = parent;
                    }
                    self.expanded_version += 1;
                    self.tab = Tab::Tree;
                    self.scroll_to = Some(id);
                    self.selection = vec![id];
                }
                Action::ZoomTo(id) => self.zoom = Some(id),
                Action::Reveal(id) => {
                    if let Err(e) = opener::reveal(index.path(id)) {
                        self.notify(format!("Could not open the file manager: {e}"));
                    }
                }
                Action::CopyPath(id) => ctx.copy_text(index.path(id).display().to_string()),
                Action::AskTrash(ids) => {
                    // A selected folder already takes its selected descendants with it.
                    let ids: Vec<EntryId> = ids
                        .iter()
                        .copied()
                        .filter(|&id| {
                            let mut ancestor = index.parent(id);
                            while let Some(a) = ancestor {
                                if ids.contains(&a) {
                                    return false;
                                }
                                ancestor = index.parent(a);
                            }
                            true
                        })
                        .collect();
                    if let Some(reason) = ids.iter().find_map(|&id| self.trash_blocker(index, id)) {
                        self.notify(reason);
                        continue;
                    }
                    let total = ids.iter().map(|&id| subtree_on_disk(index, id)).sum();
                    let items = ids.iter().map(|&id| index.path(id)).collect();
                    self.dialog = Some(Dialog::ConfirmTrash { items, total });
                }
                Action::AddToQuery(term) => {
                    if !self
                        .query
                        .split_whitespace()
                        .any(|t| t.eq_ignore_ascii_case(&term))
                    {
                        if !self.query.trim().is_empty() {
                            self.query.push(' ');
                        }
                        self.query.push_str(&term);
                        self.query_edited();
                    }
                }
            }
        }
    }

    fn open_volumes_dialog(&mut self) {
        self.volumes = ve_scan::discover_volumes();
        let choices = self
            .volumes
            .iter()
            .map(|v| (v.clone(), self.settings.is_indexed(v)))
            .collect();
        self.dialog = Some(Dialog::Volumes {
            choices,
            refresh_minutes: self.settings.refresh_minutes,
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let mut close = false;
        let mut confirmed = None;
        let modal = egui::Modal::new(Id::unique("dialog")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            match dialog {
                Dialog::ConfirmTrash { items, total } => {
                    ui.heading(if items.len() == 1 { "Move to Trash?" } else { "Move these items to Trash?" });
                    ui.label(format!(
                        "{} item(s), {} on disk. They can be restored from the Trash, and keep using space until the Trash is emptied.",
                        items.len(),
                        format::size(*total)
                    ));
                    for path in items.iter().take(8) {
                        ui.label(RichText::new(path.display().to_string()).monospace());
                    }
                    if items.len() > 8 {
                        ui.label(format!("…and {} more", items.len() - 8));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Move to Trash").clicked() {
                            let items = std::mem::take(items);
                            confirmed = Some(Confirmed::Trash(items));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::ConfirmEmptyTrash { size } => {
                    ui.heading("Empty the Trash?");
                    let amount = size.map(format::size).unwrap_or_else(|| "everything".into());
                    ui.label(format!("This permanently deletes {amount} in the Trash. It cannot be undone."));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let label = theme::palette(ui).ground;
                        let danger = egui::Button::new(RichText::new("Empty Trash").color(label))
                            .fill(ui.visuals().error_fg_color);
                        if ui.add(danger).clicked() {
                            confirmed = Some(Confirmed::EmptyTrash);
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::DeleteAsAdmin { failures } => {
                    ui.heading("Administrator rights needed");
                    ui.label(
                        "These Trash items contain files your user may not delete, usually because \
                         something created them as root. Delete them as administrator?",
                    );
                    for failure in failures.iter() {
                        ui.label(RichText::new(failure.original.display().to_string()).strong())
                            .on_hover_text(&failure.error);
                    }
                    let paths: Vec<PathBuf> =
                        failures.iter().flat_map(|f| f.admin_paths.iter().flatten().cloned()).collect();
                    ui.add_space(6.0);
                    ui.label("You will be asked for your password once. Only this command runs as root:");
                    egui::Frame::new()
                        .fill(ui.visuals().extreme_bg_color)
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(
                                RichText::new(trash::admin_command(&paths).join(" ")).monospace(),
                            ).wrap());
                        });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let label = theme::palette(ui).ground;
                        let danger = egui::Button::new(RichText::new("Delete as administrator").color(label))
                            .fill(ui.visuals().error_fg_color);
                        if ui.add(danger).clicked() {
                            confirmed = Some(Confirmed::DeleteAsAdmin(paths));
                            close = true;
                        }
                        if ui.button("Leave in Trash").clicked() {
                            close = true;
                        }
                    });
                }
                Dialog::Volumes { choices, refresh_minutes } => {
                    ui.heading("Indexed Volumes");
                    ui.label("Local fixed Volumes are indexed by default; removable ones only when ticked.");
                    ui.add_space(4.0);
                    for (volume, indexed) in choices.iter_mut() {
                        let mut text = format!("{}   {}  {}", volume.mount.display(), volume.fs_type, volume.label);
                        if volume.removable {
                            text.push_str("  · removable");
                        }
                        if volume.read_only {
                            text.push_str("  · read-only");
                        }
                        ui.checkbox(indexed, text);
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Re-read walked Volumes every");
                        ui.add(egui::DragValue::new(refresh_minutes).range(1..=1440));
                        ui.label("minutes while running");
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            let choices = std::mem::take(choices);
                            let minutes = *refresh_minutes;
                            confirmed = Some(Confirmed::Volumes(choices, minutes));
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                }
            }
        });
        if modal.should_close() {
            close = true;
        }
        if close {
            self.dialog = None;
        }
        match confirmed {
            Some(Confirmed::Trash(items)) => self.start_trash(items),
            Some(Confirmed::EmptyTrash) => self.start_empty_trash(),
            Some(Confirmed::DeleteAsAdmin(paths)) => self.start_admin_delete(paths),
            Some(Confirmed::Volumes(choices, minutes)) => self.save_volumes(choices, minutes),
            None => {}
        }
    }

    fn start_trash(&mut self, items: Vec<PathBuf>) {
        let ctx = self.ctx.clone();
        let sender = self.job_sender.clone();
        std::thread::spawn(move || {
            // Check the disk first: anything already gone just leaves the Index.
            let (present, missing): (Vec<PathBuf>, Vec<PathBuf>) = items
                .into_iter()
                .partition(|p| std::fs::symlink_metadata(p).is_ok());
            let result = if present.is_empty() {
                Ok(())
            } else {
                trash::move_to_trash(&present)
            };
            let job = match result {
                Ok(()) => Job::Trashed {
                    moved: present,
                    missing,
                    error: None,
                },
                Err(e) => Job::Trashed {
                    moved: Vec::new(),
                    missing,
                    error: Some(e),
                },
            };
            let _ = sender.send(job);
            ctx.request_repaint();
        });
    }

    fn start_admin_delete(&mut self, paths: Vec<PathBuf>) {
        let ctx = self.ctx.clone();
        let sender = self.job_sender.clone();
        std::thread::spawn(move || {
            let _ = sender.send(Job::AdminDeleted(trash::delete_as_admin(&paths)));
            ctx.request_repaint();
        });
    }

    fn start_empty_trash(&mut self) {
        let ctx = self.ctx.clone();
        let sender = self.job_sender.clone();
        std::thread::spawn(move || {
            let _ = sender.send(Job::Emptied(trash::empty_trash()));
            ctx.request_repaint();
        });
    }

    fn save_volumes(&mut self, choices: Vec<(VolumeInfo, bool)>, refresh_minutes: u64) {
        for (volume, indexed) in &choices {
            self.settings.set_indexed(volume, *indexed);
        }
        self.settings.refresh_minutes = refresh_minutes;
        if let Err(e) = self.settings.save() {
            self.notify(format!("Could not save settings: {e}"));
        }
        let indexed = choices
            .into_iter()
            .filter(|(_, i)| *i)
            .map(|(v, _)| v)
            .collect();
        self.engine.set_volumes(indexed);
    }
}

/// On-disk Size of an Entry and everything beneath it, regardless of the Query.
fn subtree_on_disk(index: &Index, id: EntryId) -> u64 {
    let mut total = 0;
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        total += index.on_disk(current);
        stack.extend_from_slice(index.children(current));
    }
    total
}

/// An Entry's share of its parent folder's Filtered On-disk Size. A Volume's share is of every
/// Indexed Volume together.
fn share_of_parent(index: &Index, view: &View, id: EntryId) -> f64 {
    let own = view.filter.totals(index, id).on_disk;
    let parent = match index.parent(id) {
        Some(parent) => view.filter.totals(index, parent).on_disk,
        None => view.total.on_disk,
    };
    if parent > 0 {
        own as f64 / parent as f64
    } else {
        1.0
    }
}

/// The Entry itself if it is a folder, else the folder holding it.
fn folder_of(index: &Index, id: EntryId) -> Option<EntryId> {
    if index.is_dir(id) {
        Some(id)
    } else {
        index.parent(id)
    }
}

/// A figure in its column: Geist Mono, right-aligned; a missing value is a grey dash.
fn figure(ui: &mut Ui, text: String) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if text.is_empty() {
            ui.label(RichText::new("—").color(theme::palette(ui).muted));
        } else {
            ui.monospace(text);
        }
    });
}

/// A column header that does not sort: 12px grey sentence case, flush with its figures.
fn column_header(ui: &mut Ui, label: &str, numeric: bool) {
    let text = RichText::new(label).small().color(theme::palette(ui).muted);
    if numeric {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(text));
    } else {
        ui.label(text);
    }
}

/// An On-disk Size cell: the size, right-aligned, after a bar showing the share of the parent
/// folder in the one data hue.
fn size_bar(ui: &mut Ui, bytes: u64, share: f64) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let text = painter.text(
        rect.right_center(),
        egui::Align2::RIGHT_CENTER,
        format::size(bytes),
        egui::TextStyle::Monospace.resolve(ui.style()),
        ui.visuals().text_color(),
    );
    let track = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.center().y - 3.0),
        egui::pos2(text.left() - 10.0, rect.center().y + 3.0),
    );
    if track.width() > 8.0 {
        let p = theme::palette(ui);
        painter.rect_filled(track, 3.0, p.raised);
        let mut bar = track;
        bar.set_width(track.width() * share.clamp(0.0, 1.0) as f32);
        painter.rect_filled(bar, 3.0, p.data);
    }
}

const QUERY_HELP: &str = "\
Plain text matches part of a name, ignoring case.
Space = AND,  a|b = OR (binds tighter than space),  !term = NOT,  \"quotes\" keep spaces.
*  and  ?  wildcards match the whole name.
A term with / or \\ matches the full path, e.g.  node_modules/
ext:mp4;mkv     type:video     size:>1gb  size:10mb..1gb
dm:2025  dm:>=2024-06  dm:today     regex:\"^img_\\d+\"

The tree, Treemap and Categories count only matching files, plus everything inside matching folders
(except files excluded by a stand-alone !term).";
