//! The Cleaner page: the Cleanables the engine's Cleaner found (`ve_engine::cleaner`), reviewed
//! as a Cleanup Plan and removed permanently by Clean (ADR-0008).

use crate::categories::CategoryPanel;
use crate::format;
use crate::settings::{Settings, cleanup_path};
use crate::theme::{FS_SMALL, palette};
use crate::ui::{ButtonKind, button, caption, panel, task_progress};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Size, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use ve_core::{Category, CategoryMap, Totals};
use ve_engine::cleaner::rules::{CAN_ELEVATE, Cleanable, CommandReport, Group, admin_command};
use ve_engine::cleaner::{self, Ages, Planned};
use ve_engine::tasks::TaskKind;
use ve_engine::view::{CategoryRow, EXTENSIONS_PER_CATEGORY};
use ve_engine::{Repaint, Shared};
use ve_scan::VolumeInfo;

type Key = (String, PathBuf);

/// Height of a Cleanable row (path and rule, 20px lines) and of an admin-only one (with its
/// command too), padding and rule included: the list is virtualised, so every row of a tab must
/// have the height the list measures.
const ROW_HEIGHT: f32 = 53.;
const ADMIN_ROW_HEIGHT: f32 = 73.;

/// What the plan bar and the tabs show, counted when the Cleanables or the ticks change rather
/// than every frame, so a list of hundreds of thousands scrolls without recounting it.
#[derive(Default)]
struct Counts {
    /// Cleanables per group, in `Group::ALL` order.
    per_group: [usize; 3],
    /// The Cleanup Plan and the admin step: how many ticked, and their On-disk Size.
    plan: (usize, u64),
    admin_plan: (usize, u64),
}

/// What the list shows: one group of Cleanables, or the admin-only ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Group(Group),
    Admin,
}

/// "Show in Browse": the Query that selects one Cleanable (None: the Index has no folder to scope to).
pub struct ShowInBrowse(pub Option<String>);

impl EventEmitter<ShowInBrowse> for CleanerPage {}

pub struct CleanerPage {
    shared: Arc<Shared>,
    repaint: Repaint,
    /// The engine's Cleanables as last taken (`Found::version`).
    version: u64,
    /// The Clean running (or last run) is the admin step, so its button is the one that spins.
    cleaning_admin: bool,
    cleanables: Arc<Vec<Cleanable>>,
    /// Cleanables already shown, so one that appears later is pre-ticked once and a choice of the
    /// user's stays.
    known: HashSet<Key>,
    ticked: HashSet<Key>,
    /// Cleanables the user does not own (or anything inside them): never in the normal Clean,
    /// only in the admin step, and never pre-ticked.
    admin: Arc<Vec<Cleanable>>,
    admin_ticked: HashSet<Key>,
    /// Why each Cleanable of the last admin step was not removed.
    admin_report: Vec<(PathBuf, String)>,
    /// What the commands of the last Clean said.
    report: Vec<CommandReport>,
    advanced: bool,
    categories: CategoryMap,
    breakdown: Entity<CategoryPanel>,
    /// The Category breakdown covers the ticked Cleanup Plan instead of every Cleanable.
    breakdown_plan: bool,
    /// The tab shown; none yet means the first one with anything in it.
    tab: Option<Tab>,
    counts: Counts,
    /// The tab the list shows, and the indexes of its Cleanables (into `cleanables` or `admin`).
    shown_tab: Tab,
    shown: Vec<usize>,
    list_scroll: UniformListScrollHandle,
}

/// Category rows (largest first) and their total On-disk Size over what `items` hold.
fn breakdown<'a>(items: impl Iterator<Item = &'a Cleanable>, map: &CategoryMap) -> (Vec<CategoryRow>, u64) {
    let mut by_ext: HashMap<&str, Totals> = HashMap::new();
    for (ext, totals) in items.flat_map(|c| &c.exts) {
        by_ext.entry(ext).or_default().add(*totals);
    }
    let mut rows: HashMap<Category, CategoryRow> = HashMap::new();
    for (ext, totals) in by_ext {
        let category = map.of_extension(ext);
        let row = rows.entry(category).or_insert_with(|| CategoryRow {
            category,
            totals: Totals::default(),
            extensions: Vec::new(),
        });
        row.totals.add(totals);
        row.extensions
            .push((if ext.is_empty() { "(none)".to_owned() } else { ext.to_owned() }, totals));
    }
    let mut rows: Vec<CategoryRow> = rows.into_values().collect();
    for row in &mut rows {
        row.extensions.sort_unstable_by_key(|(_, t)| std::cmp::Reverse(t.on_disk));
        row.extensions.truncate(EXTENSIONS_PER_CATEGORY);
    }
    rows.sort_unstable_by_key(|r| std::cmp::Reverse(r.totals.on_disk));
    let whole = rows.iter().map(|r| r.totals.on_disk).sum();
    (rows, whole)
}

fn ages(settings: &Settings) -> Ages {
    Ages {
        temp_days: settings.cleaner_temp_days,
        build_days: settings.cleaner_build_days,
    }
}

/// Where `group` sits in `Group::ALL`.
fn group_ix(group: Group) -> usize {
    Group::ALL.iter().position(|&g| g == group).unwrap_or(0)
}

impl CleanerPage {
    pub fn new(
        shared: Arc<Shared>,
        repaint: Repaint,
        volumes: Vec<VolumeInfo>,
        categories: CategoryMap,
        settings: &Settings,
        cx: &mut Context<Self>,
    ) -> Self {
        shared.cleaner.configure(cleanup_path(), volumes, ages(settings));
        Self {
            shared,
            repaint,
            version: 0,
            cleaning_admin: false,
            cleanables: Arc::default(),
            known: HashSet::new(),
            ticked: HashSet::new(),
            admin: Arc::default(),
            admin_ticked: HashSet::new(),
            admin_report: Vec::new(),
            report: Vec::new(),
            advanced: false,
            categories,
            breakdown: cx.new(|_| CategoryPanel::new(None, "")),
            breakdown_plan: false,
            tab: None,
            counts: Counts::default(),
            shown_tab: Tab::Group(Group::Temp),
            shown: Vec::new(),
            list_scroll: UniformListScrollHandle::new(),
        }
    }

    /// The Cleanables or the ticks changed: count again and sum the breakdown again.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.recount();
        self.rebuild_breakdown(cx);
    }

    /// Counts the groups and the plans, and picks the tab the list shows with its Cleanables.
    fn recount(&mut self) {
        let mut per_group = [0; 3];
        for c in self.cleanables.iter() {
            per_group[group_ix(c.rule.group)] += 1;
        }
        let sum = |plan: Vec<&Cleanable>| (plan.len(), plan.iter().map(|c| c.on_disk).sum());
        self.counts = Counts {
            per_group,
            plan: sum(self.plan()),
            admin_plan: sum(self.admin_plan()),
        };
        let tabs = self.tabs();
        let tab = self
            .tab
            .filter(|t| tabs.iter().any(|(x, _)| x == t))
            .or_else(|| tabs.iter().find(|(_, n)| *n > 0).map(|&(t, _)| t))
            .unwrap_or(Tab::Group(Group::Temp));
        self.shown_tab = tab;
        self.shown = match tab {
            Tab::Group(g) => (0..self.cleanables.len()).filter(|&i| self.cleanables[i].rule.group == g).collect(),
            Tab::Admin => (0..self.admin.len()).collect(),
        };
    }

    /// A list per group, so each one reads on its own. Admin only is the last, and only when
    /// there is something in it.
    fn tabs(&self) -> Vec<(Tab, usize)> {
        let mut tabs: Vec<(Tab, usize)> = Group::ALL
            .iter()
            .map(|&g| (Tab::Group(g), self.counts.per_group[group_ix(g)]))
            .collect();
        if !self.admin.is_empty() || !self.admin_report.is_empty() {
            tabs.push((Tab::Admin, self.admin.len()));
        }
        tabs
    }

    /// Sums the Category breakdown again for what it covers. The Entries inside each Cleanable
    /// were read off the UI thread with the look; this only adds up their per-extension totals.
    fn rebuild_breakdown(&self, cx: &mut Context<Self>) {
        let all = self.cleanables.iter().chain(self.admin.iter());
        let plan = self.plan().into_iter().chain(self.admin_plan());
        let (rows, whole) = if self.breakdown_plan {
            breakdown(plan, &self.categories)
        } else {
            breakdown(all, &self.categories)
        };
        let what = if self.breakdown_plan {
            "Cleanup Plan (ticked)"
        } else {
            "all Cleanables"
        };
        self.breakdown.update(cx, |panel, cx| panel.set_rows(rows, whole, what, cx));
    }

    pub fn set_volumes(&mut self, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.shared.cleaner.set_volumes(volumes);
        cx.notify();
    }

    /// Takes the age limits; new ones start the Cleanup Plan over with the new pre-ticking.
    pub fn set_settings(&mut self, settings: &Settings, cx: &mut Context<Self>) {
        if self.shared.cleaner.set_ages(ages(settings)) {
            self.known.clear();
            self.ticked.clear();
            self.changed(cx);
            cx.notify();
        }
    }

    /// Whether the first look for Cleanables is still running.
    pub fn looking(&self) -> bool {
        self.shared.cleaner.looking()
    }

    /// Has the engine look for Cleanables again when the Index or `cleanup.toml` changed, and
    /// takes the answer of a look that finished since.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cleaner::refresh(&self.shared);
        let found = self.shared.cleaner.found();
        if found.version != self.version {
            self.version = found.version;
            self.apply(found.own, found.admin);
            self.changed(cx);
            cx.notify();
        }
    }

    fn apply(&mut self, own: Arc<Vec<Cleanable>>, admin: Arc<Vec<Cleanable>>) {
        self.cleanables = own;
        self.admin = admin;
        let ages = self.shared.cleaner.ages();
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        for c in self.cleanables.iter() {
            if self.known.insert(c.key()) && c.pre_ticked(now, ages.temp_days, ages.build_days) {
                self.ticked.insert(c.key());
            }
        }
        let present: HashSet<Key> = self.cleanables.iter().map(Cleanable::key).collect();
        self.known.retain(|k| present.contains(k));
        self.ticked.retain(|k| present.contains(k));
        let admin: HashSet<Key> = self.admin.iter().map(Cleanable::key).collect();
        self.admin_ticked.retain(|k| admin.contains(k));
    }

    /// What the Cleaner could reclaim, with admin-only Cleanables too: size and count.
    pub fn reclaimable(&self) -> (u64, usize) {
        let all = self.cleanables.iter().chain(self.admin.iter());
        (all.clone().map(|c| c.on_disk).sum(), all.count())
    }

    /// The part of [`CleanerPage::reclaimable`] only a Clean with administrator rights frees.
    pub fn admin_reclaimable(&self) -> u64 {
        self.admin.iter().map(|c| c.on_disk).sum()
    }

    fn show_in_browse(&mut self, path: &std::path::Path, is_file: bool, cx: &mut Context<Self>) {
        let index = self.shared.index.read();
        // A file is shown by the folder it sits in.
        let at = if is_file { path.parent() } else { Some(path) };
        let term = at.and_then(|p| index.find_path(p)).and_then(|id| crate::scope::term(&index, id));
        drop(index);
        cx.emit(ShowInBrowse(term));
    }

    /// The Cleanables of the admin step that are ticked and can run (command rules cannot).
    fn admin_plan(&self) -> Vec<&Cleanable> {
        self.admin
            .iter()
            .filter(|c| !c.rule.is_command() && self.admin_ticked.contains(&c.key()))
            .collect()
    }

    fn ask_admin_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let plan: Vec<Cleanable> = self.admin_plan().into_iter().cloned().collect();
        if plan.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = plan.iter().map(|c| c.path.clone()).collect();
        let total: u64 = plan.iter().map(|c| c.on_disk).sum();
        let command = admin_command(&paths);
        let items: Vec<Planned> = plan.iter().map(|c| (c.rule.clone(), c.path.clone(), None)).collect();
        let this = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
            let items = items.clone();
            dialog
                .confirm()
                .title("Clean with admin rights?")
                .description(
                    div()
                        .child(format!(
                            "This permanently removes {} item(s), freeing about {}, and cannot be undone. Anything that no longer matches is skipped and reported. It runs:",
                            plan.len(),
                            format::size(total)
                        ))
                        .child(
                            div()
                                .mt(px(8.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(command.clone()),
                        ),
                )
                .ok_text("Clean as admin")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |page, cx| page.start_admin_clean(items.clone(), window, cx));
                    true
                })
        });
    }

    fn start_admin_clean(&mut self, plan: Vec<Planned>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clean) = cleaner::start_admin_clean(&self.shared, plan) else {
            return;
        };
        self.cleaning_admin = true;
        cx.notify();
        window
            .spawn(cx, {
                let this = cx.entity();
                async move |cx| {
                    // Off the UI thread: dropping a large Clean from the Index takes a while.
                    let outcome = cx.background_executor().spawn(async move { clean.run() }).await;
                    cx.update(|window, cx| {
                        // Wakes the Dashboard and status bar, which read the changed Index.
                        (this.read(cx).repaint)();
                        let text = format!(
                            "Permanently removed {} item(s) with admin rights; {} not removed (listed under Admin only).",
                            outcome.removed.len(),
                            outcome.skipped.len()
                        );
                        let ok = outcome.skipped.is_empty();
                        this.update(cx, |page, cx| {
                            page.admin_ticked.clear();
                            page.admin_report = outcome.skipped;
                            page.changed(cx);
                            cx.notify();
                        });
                        window.push_notification(
                            if ok {
                                Notification::info(text)
                            } else {
                                Notification::warning(text)
                            },
                            cx,
                        );
                    })
                    .ok();
                }
            })
            .detach();
    }

    fn plan(&self) -> Vec<&Cleanable> {
        self.cleanables.iter().filter(|c| self.ticked.contains(&c.key())).collect()
    }

    fn ask_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let plan: Vec<Cleanable> = self.plan().into_iter().cloned().collect();
        if plan.is_empty() {
            return;
        }
        let total: u64 = plan.iter().map(|c| c.on_disk).sum();
        let commands = plan.iter().filter(|c| c.rule.is_command()).count();
        let this = cx.entity();
        let ages = self.shared.cleaner.ages();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            // Ticked for being old: the age is tested again on disk just before removal.
            let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let items = cleaner::plan(&plan, ages, now);
            let mut what = format!("This permanently removes {} item(s)", plan.len() - commands);
            if commands > 0 {
                what.push_str(&format!(" and runs {commands} command(s)"));
            }
            dialog
                .confirm()
                .title("Clean these now?")
                .description(format!(
                    "{what}, freeing about {}. It bypasses the Trash and cannot be undone.",
                    format::size(total)
                ))
                .ok_text("Clean")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    this.update(cx, |page, cx| page.start_clean(items.clone(), window, cx));
                    true
                })
        });
    }

    fn start_clean(&mut self, plan: Vec<Planned>, window: &mut Window, cx: &mut Context<Self>) {
        // A Clean cannot run twice, whichever button or dialog started it.
        let Some(clean) = cleaner::start_clean(&self.shared, plan) else {
            return;
        };
        self.cleaning_admin = false;
        cx.notify();
        window
            .spawn(cx, {
                let this = cx.entity();
                async move |cx| {
                    // Off the UI thread: dropping a large Clean from the Index takes a while.
                    let outcome = cx.background_executor().spawn(async move { clean.run() }).await;
                    cx.update(|window, cx| {
                        // Wakes the Dashboard and status bar, which read the changed Index.
                        (this.read(cx).repaint)();
                        let failed = outcome.commands.iter().filter(|c| !c.succeeded()).count();
                        let mut text = format!("Permanently removed {} item(s).", outcome.removed.len());
                        if !outcome.commands.is_empty() {
                            text.push_str(&format!(" Ran {} command(s), {failed} failed.", outcome.commands.len()));
                        }
                        if !outcome.skipped.is_empty() {
                            text.push_str(&format!(" Skipped {}: ", outcome.skipped.len()));
                            let why: Vec<String> = outcome.skipped.iter().map(|(p, w)| format!("{} ({w})", p.display())).collect();
                            text.push_str(&why.join("; "));
                        }
                        let ok = outcome.skipped.is_empty() && failed == 0;
                        this.update(cx, |page, cx| {
                            page.ticked.clear();
                            page.changed(cx);
                            page.report = outcome.commands;
                            cx.notify();
                        });
                        window.push_notification(
                            if ok {
                                Notification::info(text)
                            } else {
                                Notification::warning(text)
                            },
                            cx,
                        );
                    })
                    .ok();
                }
            })
            .detach();
    }

    fn show_button(&self, id: (&'static str, usize), c: &Cleanable, cx: &mut Context<Self>) -> impl IntoElement {
        let (path, command) = (c.path.clone(), c.rule.is_command());
        button(id, "Show in Browse").small().build(
            cx,
            cx.listener(move |this, _, _, cx| this.show_in_browse(&path, !command && !path.is_dir(), cx)),
        )
    }

    /// Rows of the admin-only Cleanables at `ixs` of `admin`, each with the command that would
    /// remove it. Every line truncates, so each row fits `ADMIN_ROW_HEIGHT`.
    fn admin_rows(&self, ixs: &[usize], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = palette(cx);
        let mono = cx.theme().mono_font_family.clone();
        ixs.iter()
            .map(|&ix| {
                let c = &self.admin[ix];
                let key = c.key();
                let command = if c.rule.is_command() {
                    format!(
                        "{} (in {}): a command rule is not run with admin rights; run it yourself",
                        c.rule.argv_text(),
                        c.path.display()
                    )
                } else {
                    admin_command(std::slice::from_ref(&c.path))
                };
                h_flex()
                    .w_full()
                    .gap(px(12.))
                    .h(px(ADMIN_ROW_HEIGHT))
                    .border_b_1()
                    .border_color(p.rule)
                    .items_center()
                    .child(if c.rule.is_command() {
                        div().w(px(16.)).into_any_element()
                    } else {
                        Checkbox::new(("admin-cleanable", ix))
                            .checked(self.admin_ticked.contains(&key))
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                if *on {
                                    this.admin_ticked.insert(key.clone());
                                } else {
                                    this.admin_ticked.remove(&key);
                                }
                                this.changed(cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(c.path.display().to_string()))
                            .child(caption(format!("{}: {}", c.rule.name, c.rule.group.title()), cx).truncate())
                            .child(div().font_family(mono.clone()).text_size(px(FS_SMALL)).truncate().child(command)),
                    )
                    .child(div().flex_none().child(format::size(c.on_disk)))
                    .child(self.show_button(("admin-show", ix), c, cx))
                    .into_any_element()
            })
            .collect()
    }

    /// Rows of the Cleanables at `ixs` of `cleanables`.
    fn rows(&self, ixs: &[usize], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let rule = palette(cx).rule;
        ixs.iter()
            .map(|&ix| {
                let c = &self.cleanables[ix];
                let key = c.key();
                let by = if c.rule.is_command() {
                    format!("{}: runs {}", c.rule.name, c.rule.argv_text())
                } else {
                    c.rule.name.clone()
                };
                h_flex()
                    .w_full()
                    .gap(px(12.))
                    .h(px(ROW_HEIGHT))
                    .border_b_1()
                    .border_color(rule)
                    .items_center()
                    .child(
                        Checkbox::new(("cleanable", ix))
                            .checked(self.ticked.contains(&key))
                            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                                if *on {
                                    this.ticked.insert(key.clone());
                                } else {
                                    this.ticked.remove(&key);
                                }
                                this.changed(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(c.path.display().to_string()))
                            .child(caption(by, cx).truncate()),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .items_end()
                            .child(format::size(c.on_disk))
                            .when(c.rule.is_command(), |d| d.child(caption("Estimated", cx))),
                    )
                    .child(self.show_button(("show", ix), c, cx))
                    .into_any_element()
            })
            .collect()
    }

    fn report_panel(&self, cx: &App) -> Option<Div> {
        if self.report.is_empty() {
            return None;
        }
        let p = palette(cx);
        let mono = cx.theme().mono_font_family.clone();
        let entries = self.report.iter().map(|r| {
            let (status, color, output) = match &r.result {
                Ok((Some(0), out)) => ("exit status 0".to_owned(), p.muted, out.clone()),
                Ok((Some(code), out)) => (format!("failed: exit status {code}"), p.warning, out.clone()),
                Ok((None, out)) => ("failed: stopped by a signal".to_owned(), p.warning, out.clone()),
                Err(why) => (format!("not run: {why}"), p.warning, String::new()),
            };
            v_flex()
                .gap_1()
                .child(
                    div()
                        .font_family(mono.clone())
                        .child(format!("{}  (in {})", r.argv, r.cwd.display())),
                )
                .child(div().text_color(color).child(status))
                .when(!output.is_empty(), |d| {
                    d.child(div().font_family(mono.clone()).text_xs().child(output))
                })
        });
        Some(
            panel(cx).child(
                v_flex()
                    .gap_2()
                    .child(div().font_weight(FontWeight::MEDIUM).child("Output of the last Clean"))
                    .children(entries),
            ),
        )
    }
}

impl Render for CleanerPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _span = crate::trace::span("CleanerPage::render");
        self.refresh(cx);
        let p = palette(cx);
        let (muted, warning) = (p.muted, p.warning);
        let ((count, total), (admin_count, admin_total)) = (self.counts.plan, self.counts.admin_plan);
        let tabs = self.tabs();
        let tab = self.shown_tab;

        let advanced = self.advanced.then(|| {
            let (plan, admin_plan) = (self.plan(), self.admin_plan());
            let paths: Vec<_> = plan.iter().chain(&admin_plan).filter(|c| !c.rule.is_command()).collect();
            let commands: Vec<_> = plan.iter().filter(|c| c.rule.is_command()).collect();
            v_flex()
                .id("advanced")
                .max_h(px(200.))
                .overflow_y_scrollbar()
                .gap_1()
                .p_2()
                .rounded_md()
                .bg(cx.theme().muted)
                .font_family(cx.theme().mono_font_family.clone())
                .when(!paths.is_empty(), |d| {
                    d.child(caption("Removed permanently, exactly these paths:", cx))
                        .children(paths.iter().map(|c| div().child(c.path.display().to_string())))
                })
                .when(!commands.is_empty(), |d| {
                    d.child(caption(
                        "Run without a shell, exactly this argv, in this working folder (the size is Estimated):",
                        cx,
                    ))
                    .children(commands.iter().map(|c| {
                        v_flex()
                            .child(div().child(c.rule.argv_text()))
                            .child(caption(format!("in {}", c.path.display()), cx))
                    }))
                })
                .when(count + admin_count == 0, |d| d.child(caption("Nothing ticked.", cx)))
        });
        let (rules, ages) = (self.shared.cleaner.rules(), self.shared.cleaner.ages());
        let problems = (!rules.errors.is_empty()).then(|| {
            v_flex()
                .gap_1()
                .text_color(warning)
                .child("Some Cleanup Rules were not read; the others still work:")
                .children(rules.errors.iter().map(|e| div().child(e.clone())))
        });
        let admin_why = if !CAN_ELEVATE {
            Some("This system cannot run the command elevated from here; run it yourself")
        } else if admin_count == 0 {
            Some(if self.admin.is_empty() {
                "Nothing here needs admin rights"
            } else {
                "Tick something under Admin only first"
            })
        } else {
            None
        };
        let mut summary = format!("Cleanup Plan: {count} selected, {}", format::size(total));
        if !self.admin.is_empty() {
            summary.push_str(&format!(" · Admin only: {admin_count} selected, {}", format::size(admin_total)));
        }
        let clean_task = self.shared.tasks.running().into_iter().find(|t| t.kind == TaskKind::Clean);
        let clean_busy = clean_task.is_some() && !self.cleaning_admin;
        let admin_busy = clean_task.is_some() && self.cleaning_admin;
        let clean_blocked = clean_task.is_some().then_some("A Clean is already running");
        // The plan and its buttons stay above the list, so they are never a scroll away.
        let bar = panel(cx)
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(div().flex_1().child(summary))
                    .child(button("plan-advanced", "Advanced").small().pressed(self.advanced).build(
                        cx,
                        cx.listener(|this, _, _, cx| {
                            this.advanced = !this.advanced;
                            cx.notify();
                        }),
                    ))
                    .child(
                        button("clean", "Clean")
                            .kind(ButtonKind::Primary)
                            .busy(clean_busy)
                            .disabled(if clean_busy {
                                None
                            } else {
                                clean_blocked.or((count == 0).then_some("Tick what to clean first"))
                            })
                            .build(cx, cx.listener(|this, _, window, cx| this.ask_clean(window, cx))),
                    )
                    .child(
                        button("admin-clean", "Clean as admin…")
                            .busy(admin_busy)
                            .disabled(if admin_busy { None } else { clean_blocked.or(admin_why) })
                            .build(cx, cx.listener(|this, _, window, cx| this.ask_admin_clean(window, cx))),
                    ),
            )
            .children(clean_task.map(|t| {
                div()
                    .mt(px(10.))
                    .child(task_progress("clean-progress", t.done, t.total, "Cleanables", cx))
            }))
            .children(advanced.map(|a| div().mt(px(12.)).child(a)));
        let looking = self.looking();
        let search_task = self.shared.tasks.running().into_iter().find(|t| t.kind == TaskKind::CleanerSearch);
        // While looking, the empty list says so with a spinner; this row adds only the progress.
        let status = if looking {
            search_task.filter(|t| t.total.is_some_and(|n| n > 0)).map(|t| {
                let what = if self.shared.cleaner.sizing() {
                    "Cleanables measured"
                } else {
                    "Entries checked"
                };
                task_progress("cleaner-progress", t.done, t.total, what, cx).into_any_element()
            })
        } else if self.shared.cleaner.paused() {
            Some("The search was cancelled.".into_any_element())
        } else {
            Some(
                format!(
                    "{} Cleanables found",
                    format::count((self.cleanables.len() + self.admin.len()) as u64)
                )
                .into_any_element(),
            )
        };
        let search_row = h_flex()
            .flex_none()
            .gap(px(10.))
            .items_center()
            .child(div().flex_1().text_color(muted).children(status))
            .when(looking, |row| {
                row.child(button("cancel-search", "Cancel").small().build(
                    cx,
                    cx.listener(|this, _, _, cx| {
                        cleaner::cancel_look(&this.shared);
                        cx.notify();
                    }),
                ))
            })
            .child(
                button("cleaner-rescan", "Look again")
                    .small()
                    .busy(looking)
                    .disabled(looking.then_some("A search is already running"))
                    .build(
                        cx,
                        cx.listener(|this, _, _, cx| {
                            this.shared.cleaner.look_again();
                            cx.notify();
                        }),
                    ),
            );

        let tab_titles: Vec<String> = tabs
            .iter()
            .map(|&(t, n)| {
                let title = match t {
                    Tab::Group(g) => g.title(),
                    Tab::Admin => "Admin only",
                };
                format!("{title} ({n})")
            })
            .collect();
        let tab_titles: Vec<&str> = tab_titles.iter().map(String::as_str).collect();
        let tab_ids: Vec<Tab> = tabs.iter().map(|&(t, _)| t).collect();
        let tab_ix = tab_ids.iter().position(|&t| t == tab).unwrap_or(0);
        let me = cx.entity();
        let tab_bar = crate::ui::tabs("tab", &tab_titles, tab_ix, cx, move |i, _, cx| {
            me.update(cx, |this, cx| {
                this.tab = Some(tab_ids[i]);
                this.recount();
                this.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
        });
        let admin_intro = (tab == Tab::Admin).then(|| {
            v_flex()
                .gap_1()
                .child(caption(
                    "You do not own these (or something inside them), so the normal Clean never touches them. Each is removed only by Clean as admin, which shows the exact command first.",
                    cx,
                ))
                .when(!self.admin_report.is_empty(), |d| {
                    d.child(div().text_color(warning).child("Not removed by the last admin step:"))
                        .children(
                            self.admin_report
                                .iter()
                                .map(|(path, why)| div().text_color(warning).child(format!("{} ({why})", path.display()))),
                        )
                })
        });
        // Every Cleanable of the tab in one list; only the rows in view are built.
        let body = if self.shown.is_empty() {
            if self.looking() {
                h_flex()
                    .gap(px(10.))
                    .items_center()
                    .child(Spinner::new().with_size(Size::XSmall).color(muted))
                    .child("Looking for Cleanables…")
                    .into_any_element()
            } else {
                div().child("Nothing to clean here.").into_any_element()
            }
        } else {
            let rows = uniform_list(
                "cleanables",
                self.shown.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| match this.shown_tab {
                    Tab::Group(_) => this.rows(&this.shown[range], cx),
                    Tab::Admin => this.admin_rows(&this.shown[range], cx),
                }),
            );
            crate::ui::scrolling_list(rows, &self.list_scroll).into_any_element()
        };

        let list = panel(cx)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(search_row)
            .child(tab_bar)
            .children(admin_intro)
            .child(body);

        let me = cx.entity();
        let scope = crate::ui::segmented(
            "breakdown-scope",
            &["All Cleanables", "Cleanup Plan"],
            usize::from(self.breakdown_plan),
            cx,
            move |i, _, cx| {
                me.update(cx, |this, cx| {
                    this.breakdown_plan = i == 1;
                    this.rebuild_breakdown(cx);
                    cx.notify();
                })
            },
        );
        let side = v_flex()
            .flex_grow(2.)
            .flex_basis(relative(0.))
            .min_w(px(320.))
            .gap(px(8.))
            .child(h_flex().child(scope))
            .child(div().h(px(301.)).child(self.breakdown.clone()));

        v_flex()
            .id("cleaner")
            .size_full()
            .gap(px(12.))
            .child(crate::ui::title_row(
                "Cleaner",
                Some(format!(
                    "Temporary files, caches and build artefacts, found in the Index and confirmed by Cleanup Rules (add your own in cleanup.toml). Pre-ticked: temporary files untouched for {} days and build artefacts whose project is untouched for {} days; the limits are in Settings.",
                    ages.temp_days, ages.build_days
                )),
                cx,
            ))
            .children(problems)
            .child(bar)
            .children(self.report_panel(cx).map(|r| div().id("report").max_h(px(160.)).overflow_y_scrollbar().child(r)))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(12.))
                    .items_start()
                    .child(list.h_full().flex_grow(3.).flex_basis(relative(0.)).min_w_0())
                    .child(side),
            )
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob would bring gpui's own `test` attribute into scope.
    use super::{Category, CategoryMap, Cleanable, Totals, breakdown};
    use ve_engine::cleaner::rules;

    #[test]
    fn breakdown_sums_extensions_into_categories_largest_first() {
        let t = |on_disk, files| Totals {
            logical: on_disk,
            on_disk,
            files,
        };
        let rule = rules::load(None).rules[0].clone();
        let cleanable = |exts| Cleanable {
            rule: rule.clone(),
            path: "/x".into(),
            entry: ve_core::EntryId(0),
            on_disk: 0,
            exts,
            newest: 0,
        };
        let a = cleanable(vec![("rs".into(), t(10, 2)), ("mp4".into(), t(50, 1))]);
        let b = cleanable(vec![("rs".into(), t(5, 1)), (String::new(), t(1, 1))]);
        let (rows, whole) = breakdown([&a, &b].into_iter(), &CategoryMap::default());
        assert_eq!(whole, 66);
        assert_eq!(rows[0].category, Category::Video);
        let code = rows.iter().find(|r| r.category == Category::Code).unwrap();
        assert_eq!((code.totals.on_disk, code.totals.files), (15, 3));
    }
}
