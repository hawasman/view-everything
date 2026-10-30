//! The Cleaner: Cleanables found from the Index by Cleanup Rules, reviewed as a Cleanup Plan
//! and removed permanently by Clean (ADR-0008). The rules themselves are in `rules`.

mod rules;

use crate::categories::CategoryPanel;
use crate::engine::{Repaint, Shared};
use crate::format;
use crate::settings::{Settings, cleanup_path};
use crate::tasks::{Outcome, TaskKind};
use crate::theme::palette;
use crate::ui::{caption, panel, pill, task_progress};
use crate::view::{CategoryRow, EXTENSIONS_PER_CATEGORY};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Size, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rules::{CAN_ELEVATE, Cleanable, CommandReport, Group, Rule, Rules, admin_clean, admin_command, clean, foreign};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

/// Least time between two looks that only follow the Index moving on.
const RELOOK_THROTTLE: std::time::Duration = std::time::Duration::from_secs(5);
use ve_core::{Category, CategoryMap, Totals};
use ve_scan::VolumeInfo;

type Key = (String, PathBuf);

/// Items per page the pager offers; the first is the default.
const PAGE_SIZES: [usize; 3] = [25, 50, 100];

/// What the list shows: one group of Cleanables, or the admin-only ones.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Group(Group),
    Admin,
}

/// "Show in Explorer": the Query that selects one Cleanable (None: the Index has no folder to scope to).
pub struct ShowInExplorer(pub Option<String>);

impl EventEmitter<ShowInExplorer> for CleanerPage {}

pub struct CleanerPage {
    shared: Arc<Shared>,
    repaint: Repaint,
    volumes: Vec<VolumeInfo>,
    rules: Rules,
    /// When `cleanup.toml` was last read, as its modification time (None: no such file).
    rules_stamp: Option<SystemTime>,
    /// Age limits in days for pre-ticking temporary files and build artefacts.
    temp_days: u64,
    build_days: u64,
    /// Index generation `cleanables` was found in.
    seen: Option<u64>,
    /// When the last look started, for the throttle on looking again as the Index moves.
    last_look: Instant,
    relook_scheduled: bool,
    /// A look for Cleanables is running on a background thread. Nothing it has not told apart
    /// into yours and admin-only is offered, so a Cleanable never shows up as cleanable early.
    looking: bool,
    /// The user cancelled the look: none starts until they ask again or something changes.
    paused: bool,
    /// The running look is measuring what it found, not yet checking Entries.
    sizing: Arc<AtomicBool>,
    /// Cancel flag of the running look, so Cancel works for a look with no Task too.
    look_cancel: Arc<AtomicBool>,
    /// The Clean running (or last run) is the admin step, so its button is the one that spins.
    cleaning_admin: bool,
    /// Counts the changes (rules, Volumes, age limits) that make a running look's answer stale.
    epoch: u64,
    cleanables: Vec<Cleanable>,
    /// Cleanables already shown, so one that appears later is pre-ticked once and a choice of the
    /// user's stays.
    known: HashSet<Key>,
    ticked: HashSet<Key>,
    /// Cleanables the user does not own (or anything inside them): never in the normal Clean,
    /// only in the admin step, and never pre-ticked.
    admin: Vec<Cleanable>,
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
    page: usize,
    page_size: usize,
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

/// Whatever left the disk leaves the Index, skipped-because-vanished included.
fn drop_from_index(shared: &Shared, outcome: &rules::Outcome) {
    let mut index = shared.index.write();
    let vanished = outcome
        .skipped
        .iter()
        .filter(|(p, _)| std::fs::symlink_metadata(p).is_err())
        .map(|(p, _)| p);
    for path in outcome.removed.iter().chain(vanished) {
        if let Some(id) = index.find_path(path) {
            index.remove(id);
        }
    }
}

fn rules_stamp() -> Option<SystemTime> {
    std::fs::metadata(cleanup_path()?).ok()?.modified().ok()
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
        Self {
            shared,
            repaint,
            volumes,
            rules: rules::load(cleanup_path().as_deref()),
            rules_stamp: rules_stamp(),
            temp_days: settings.cleaner_temp_days,
            build_days: settings.cleaner_build_days,
            seen: None,
            last_look: Instant::now(),
            relook_scheduled: false,
            looking: false,
            paused: false,
            sizing: Arc::new(AtomicBool::new(false)),
            look_cancel: Arc::new(AtomicBool::new(false)),
            cleaning_admin: false,
            epoch: 0,
            cleanables: Vec::new(),
            known: HashSet::new(),
            ticked: HashSet::new(),
            admin: Vec::new(),
            admin_ticked: HashSet::new(),
            admin_report: Vec::new(),
            report: Vec::new(),
            advanced: false,
            categories,
            breakdown: cx.new(|_| CategoryPanel::new(None, "")),
            breakdown_plan: false,
            tab: None,
            page: 0,
            page_size: PAGE_SIZES[0],
        }
    }

    /// Sums the Category breakdown again for what it covers. The Entries inside each Cleanable
    /// were read off the UI thread with the look; this only adds up their per-extension totals.
    fn rebuild_breakdown(&self, cx: &mut Context<Self>) {
        let all = self.cleanables.iter().chain(&self.admin);
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
        self.volumes = volumes;
        self.invalidate();
        cx.notify();
    }

    /// Takes the age limits; new ones start the Cleanup Plan over with the new pre-ticking.
    pub fn set_settings(&mut self, settings: &Settings, cx: &mut Context<Self>) {
        if (self.temp_days, self.build_days) != (settings.cleaner_temp_days, settings.cleaner_build_days) {
            self.temp_days = settings.cleaner_temp_days;
            self.build_days = settings.cleaner_build_days;
            self.known.clear();
            self.ticked.clear();
            self.invalidate();
            cx.notify();
        }
    }

    /// The next `refresh` looks again, and a look already running is forgotten.
    fn invalidate(&mut self) {
        self.seen = None;
        self.paused = false;
        self.epoch += 1;
    }

    /// Whether the first look for Cleanables is still running.
    pub fn looking(&self) -> bool {
        self.looking && self.seen.is_none()
    }

    /// Starts looking for Cleanables again when the Index or `cleanup.toml` changed since the
    /// last look. The look, with the ownership walk over every Cleanable, runs on a background
    /// thread; what the page shows meanwhile is the last look's answer.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let stamp = rules_stamp();
        if stamp != self.rules_stamp {
            self.rules_stamp = stamp;
            self.rules = rules::load(cleanup_path().as_deref());
            self.invalidate();
        }
        if self.looking || self.paused {
            return;
        }
        let generation = self.shared.index.read().generation();
        if self.seen == Some(generation) {
            return;
        }
        // Looking again only because the Index moved (Live updates) waits out the throttle, so a
        // busy volume does not re-walk every Cleanable on every generation.
        if self.seen.is_some() {
            let since = self.last_look.elapsed();
            if since < RELOOK_THROTTLE {
                if !self.relook_scheduled {
                    self.relook_scheduled = true;
                    let wait = RELOOK_THROTTLE - since;
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        this.update(cx, |this, cx| {
                            this.relook_scheduled = false;
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                }
                return;
            }
        }
        self.last_look = Instant::now();
        self.looking = true;
        let epoch = self.epoch;
        let (shared, volumes, rules) = (self.shared.clone(), self.volumes.clone(), self.rules.rules.clone());
        let sizing = self.sizing.clone();
        // Only a look the page waits on (the first, or after a change of rules or Volumes) is a
        // Task; looking again as the Index moves on is quiet, or every sync would fill the list.
        let task = self
            .seen
            .is_none()
            .then(|| shared.tasks.begin(TaskKind::CleanerSearch, "Look for Cleanables".into(), true));
        let cancel = task.as_ref().map_or_else(|| Arc::new(AtomicBool::new(false)), |t| t.cancel_flag());
        self.look_cancel = cancel.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let index = shared.index.read();
                    let generation = index.generation();
                    let progress = |phase: rules::Phase, done: u64, total: u64| {
                        sizing.store(phase == rules::Phase::Sizing, Ordering::Relaxed);
                        if let Some(task) = &task {
                            task.set_progress(done, Some(total));
                        }
                    };
                    let found = rules::find(&index, &volumes, &rules, &cancel, &progress);
                    drop(index);
                    // Fail closed: a Cleanable is the user's only if the whole walk says so.
                    let (admin, own): (Vec<_>, Vec<_>) = found
                        .into_iter()
                        .partition(|c| !cancel.load(Ordering::Relaxed) && foreign(&c.path, !c.rule.is_command()).is_some());
                    let cancelled = cancel.load(Ordering::Relaxed);
                    if let Some(task) = task {
                        task.finish(if cancelled {
                            Outcome::Cancelled
                        } else {
                            Outcome::Done(format!(
                                "{} Cleanables found",
                                crate::format::count((own.len() + admin.len()) as u64)
                            ))
                        });
                    }
                    (generation, own, admin, cancelled)
                })
                .await;
            this.update(cx, |this, cx| {
                this.looking = false;
                match look_verdict(this.epoch, epoch, found.3) {
                    LookVerdict::Apply => {
                        this.apply(found.0, found.1, found.2);
                        this.rebuild_breakdown(cx);
                    }
                    // Cancelled: no new look until the user asks again or the rules change.
                    LookVerdict::Pause => this.paused = true,
                    // Rules or Volumes changed meanwhile: this answer is for something else, so the
                    // render that follows looks again.
                    LookVerdict::Stale => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply(&mut self, generation: u64, own: Vec<Cleanable>, admin: Vec<Cleanable>) {
        self.cleanables = own;
        self.admin = admin;
        self.seen = Some(generation);
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        for c in &self.cleanables {
            if self.known.insert(c.key()) && c.pre_ticked(now, self.temp_days, self.build_days) {
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
        let all = self.cleanables.iter().chain(&self.admin);
        (all.clone().map(|c| c.on_disk).sum(), all.count())
    }

    fn show_in_explorer(&mut self, path: &std::path::Path, is_file: bool, cx: &mut Context<Self>) {
        let index = self.shared.index.read();
        // A file is shown by the folder it sits in.
        let at = if is_file { path.parent() } else { Some(path) };
        let term = at.and_then(|p| index.find_path(p)).and_then(|id| crate::scope::term(&index, id));
        drop(index);
        cx.emit(ShowInExplorer(term));
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
        let items: Vec<(Arc<Rule>, PathBuf, Option<u64>)> = plan.iter().map(|c| (c.rule.clone(), c.path.clone(), None)).collect();
        let (this, volumes) = (cx.entity(), self.volumes.clone());
        window.open_alert_dialog(cx, move |dialog, _, cx| {
            let this = this.clone();
            let (items, volumes) = (items.clone(), volumes.clone());
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
                    this.update(cx, |page, cx| page.start_admin_clean(items.clone(), volumes.clone(), window, cx));
                    true
                })
        });
    }

    fn start_admin_clean(
        &mut self,
        plan: Vec<(Arc<Rule>, PathBuf, Option<u64>)>,
        volumes: Vec<VolumeInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shared.tasks.is_running(TaskKind::Clean) {
            return;
        }
        self.cleaning_admin = true;
        let total = plan.len() as u64;
        let task = self
            .shared
            .tasks
            .begin(TaskKind::Clean, format!("Clean {total} Cleanables as admin"), false);
        task.set_progress(0, Some(total));
        cx.notify();
        window
            .spawn(cx, {
                let this = cx.entity();
                async move |cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            let outcome = admin_clean(&plan, &volumes, &|k| task.set_progress(k as u64, Some(total)), &|| {
                                // No countable progress while pkexec asks and removes: spin.
                                task.set_label("Removing as administrator…".into());
                                task.set_progress(0, None);
                            });
                            // No set_retry on purpose: a failed Clean is not offered as Retry, since
                            // re-running a destructive Clean unseen is not what the user asked for.
                            task.finish(if outcome.skipped.is_empty() {
                                Outcome::Done(format!("Removed {} item(s)", crate::format::count(outcome.removed.len() as u64)))
                            } else {
                                Outcome::Failed(format!("{} item(s) not removed", outcome.skipped.len()))
                            });
                            outcome
                        })
                        .await;
                    cx.update(|window, cx| {
                        let (shared, repaint) = {
                            let page = this.read(cx);
                            (page.shared.clone(), page.repaint.clone())
                        };
                        drop_from_index(&shared, &outcome);
                        repaint();
                        let text = format!(
                            "Permanently removed {} item(s) with admin rights; {} not removed (listed under Admin only).",
                            outcome.removed.len(),
                            outcome.skipped.len()
                        );
                        let ok = outcome.skipped.is_empty();
                        this.update(cx, |page, cx| {
                            page.admin_ticked.clear();
                            page.rebuild_breakdown(cx);
                            page.admin_report = outcome.skipped;
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
        let volumes = self.volumes.clone();
        let (temp_days, build_days) = (self.temp_days, self.build_days);
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let this = this.clone();
            // Ticked for being old: the age is tested again on disk just before removal.
            let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
            let items: Vec<(Arc<Rule>, PathBuf, Option<u64>)> = plan
                .iter()
                .map(|c| {
                    let aged = c
                        .pre_ticked(now, temp_days, build_days)
                        .then(|| c.age_limit(temp_days, build_days))
                        .flatten();
                    (c.rule.clone(), c.path.clone(), aged)
                })
                .collect();
            let volumes = volumes.clone();
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
                    this.update(cx, |page, cx| page.start_clean(items.clone(), volumes.clone(), window, cx));
                    true
                })
        });
    }

    fn start_clean(
        &mut self,
        plan: Vec<(Arc<Rule>, PathBuf, Option<u64>)>,
        volumes: Vec<VolumeInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A Clean cannot run twice, whichever button or dialog started it.
        if self.shared.tasks.is_running(TaskKind::Clean) {
            return;
        }
        self.cleaning_admin = false;
        let shared = self.shared.clone();
        let total = plan.len() as u64;
        let task = shared.tasks.begin(TaskKind::Clean, format!("Clean {total} Cleanables"), false);
        task.set_progress(0, Some(total));
        cx.notify();
        window
            .spawn(cx, {
                let this = cx.entity();
                async move |cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            let outcome = clean(&plan, &volumes, &|k| task.set_progress(k as u64, Some(total)));
                            // Whatever a command changed is read back into the Index: only its working
                            // folder, since a command rule's effect is confined to it (the Volume would
                            // take minutes to re-walk).
                            for command in outcome.commands.iter().filter(|c| c.result.is_ok()) {
                                let folder = shared.index.read().find_path(&command.cwd);
                                if let Some(folder) = folder {
                                    let _ = ve_scan::sync_folder(&shared.index, folder, &command.cwd, &shared.progress);
                                }
                            }
                            let failed = outcome.commands.iter().filter(|c| !c.succeeded()).count() + outcome.skipped.len();
                            task.finish(if failed == 0 {
                                Outcome::Done(format!("Removed {} item(s)", crate::format::count(outcome.removed.len() as u64)))
                            } else {
                                Outcome::Failed(format!("{failed} item(s) skipped or failed"))
                            });
                            outcome
                        })
                        .await;
                    cx.update(|window, cx| {
                        let (shared, repaint) = {
                            let page = this.read(cx);
                            (page.shared.clone(), page.repaint.clone())
                        };
                        drop_from_index(&shared, &outcome);
                        // Wakes the Dashboard and status bar, which read the changed Index.
                        repaint();
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
                            page.rebuild_breakdown(cx);
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
        pill(id, "Show in Explorer").small().build(
            cx,
            cx.listener(move |this, _, _, cx| this.show_in_explorer(&path, !command && !path.is_dir(), cx)),
        )
    }

    /// Rows of the admin-only Cleanables `range` of them, each with the command that would remove it.
    fn admin_rows(&self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = palette(cx);
        let mono = cx.theme().mono_font_family.clone();
        range
            .map(|ix| {
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
                    .gap(px(12.))
                    .py(px(6.))
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
                                this.rebuild_breakdown(cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(c.path.display().to_string()))
                            .child(caption(format!("{}: {}", c.rule.name, c.rule.group.title()), cx))
                            .child(div().font_family(mono.clone()).text_xs().child(command)),
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
                    .gap(px(12.))
                    .py(px(6.))
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
                                this.rebuild_breakdown(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().child(c.path.display().to_string()))
                            .child(caption(by, cx)),
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
        self.refresh(cx);
        let p = palette(cx);
        let (muted, warning) = (p.muted, p.warning);
        let plan = self.plan();
        let (count, total) = (plan.len(), plan.iter().map(|c| c.on_disk).sum::<u64>());
        let admin_plan = self.admin_plan();
        let (admin_count, admin_total) = (admin_plan.len(), admin_plan.iter().map(|c| c.on_disk).sum::<u64>());

        // Tabs: a list per group, so each one reads on its own and pages within itself. Admin only
        // is the last, and only when there is something in it.
        let in_group = |g: Group| self.cleanables.iter().filter(|c| c.rule.group == g).count();
        let mut tabs: Vec<(Tab, usize)> = Group::ALL.iter().map(|&g| (Tab::Group(g), in_group(g))).collect();
        if !self.admin.is_empty() || !self.admin_report.is_empty() {
            tabs.push((Tab::Admin, self.admin.len()));
        }
        let tab = self
            .tab
            .filter(|t| tabs.iter().any(|(x, _)| x == t))
            .or_else(|| tabs.iter().find(|(_, n)| *n > 0).map(|&(t, _)| t))
            .unwrap_or(Tab::Group(Group::Temp));
        let ixs: Vec<usize> = match tab {
            Tab::Group(g) => (0..self.cleanables.len()).filter(|&i| self.cleanables[i].rule.group == g).collect(),
            Tab::Admin => (0..self.admin.len()).collect(),
        };
        let pages = ixs.len().div_ceil(self.page_size).max(1);
        let page = self.page.min(pages - 1);
        let (from, to) = (page * self.page_size, ((page + 1) * self.page_size).min(ixs.len()));
        let rows = match tab {
            Tab::Group(_) => self.rows(&ixs[from..to], cx),
            Tab::Admin => self.admin_rows(from..to, cx),
        };

        let advanced = self.advanced.then(|| {
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
        let problems = (!self.rules.errors.is_empty()).then(|| {
            v_flex()
                .gap_1()
                .text_color(warning)
                .child("Some Cleanup Rules were not read; the others still work:")
                .children(self.rules.errors.iter().map(|e| div().child(e.clone())))
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
                    .child(pill("plan-advanced", "Advanced").small().selected(self.advanced).build(
                        cx,
                        cx.listener(|this, _, _, cx| {
                            this.advanced = !this.advanced;
                            cx.notify();
                        }),
                    ))
                    .child(
                        pill("clean", "Clean")
                            .primary()
                            .busy(clean_busy)
                            .disabled(if clean_busy {
                                None
                            } else {
                                clean_blocked.or((count == 0).then_some("Tick what to clean first"))
                            })
                            .build(cx, cx.listener(|this, _, window, cx| this.ask_clean(window, cx))),
                    )
                    .child(
                        pill("admin-clean", "Clean as admin…")
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
                let what = if self.sizing.load(Ordering::Relaxed) {
                    "Cleanables measured"
                } else {
                    "Entries checked"
                };
                task_progress("cleaner-progress", t.done, t.total, what, cx).into_any_element()
            })
        } else if self.paused {
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
                row.child(pill("cancel-search", "Cancel").small().build(
                    cx,
                    cx.listener(|this, _, _, cx| {
                        this.look_cancel.store(true, Ordering::Relaxed);
                        this.shared.tasks.cancel_kind(TaskKind::CleanerSearch);
                        cx.notify();
                    }),
                ))
            })
            .child(
                pill("cleaner-rescan", "Look again")
                    .small()
                    .busy(looking)
                    .disabled(looking.then_some("A search is already running"))
                    .build(
                        cx,
                        cx.listener(|this, _, _, cx| {
                            this.invalidate();
                            cx.notify();
                        }),
                    ),
            );

        let tab_bar = h_flex().gap(px(8.)).children(tabs.iter().enumerate().map(|(i, &(t, n))| {
            let title = match t {
                Tab::Group(g) => g.title(),
                Tab::Admin => "Admin only",
            };
            pill(("tab", i), format!("{title} ({n})")).small().selected(t == tab).build(
                cx,
                cx.listener(move |this, _, _, cx| {
                    this.tab = Some(t);
                    this.page = 0;
                    cx.notify();
                }),
            )
        }));
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
        let empty = ixs.is_empty().then(|| {
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
        });
        let size_pills = PAGE_SIZES.iter().map(|&n| {
            pill(("page-size", n), n.to_string()).small().selected(n == self.page_size).build(
                cx,
                cx.listener(move |this, _, _, cx| {
                    // Keep the first item on screen across a change of size.
                    this.page = this.page * this.page_size / n;
                    this.page_size = n;
                    cx.notify();
                }),
            )
        });
        let pager = h_flex()
            .gap(px(8.))
            .items_center()
            .child(
                pill("page-prev", "‹ Previous")
                    .small()
                    .disabled((page == 0).then_some("This is the first page"))
                    .build(
                        cx,
                        cx.listener(move |this, _, _, cx| {
                            this.page = page - 1;
                            cx.notify();
                        }),
                    ),
            )
            .child(div().child(format!(
                "Page {} of {pages} · {}–{} of {}",
                page + 1,
                if ixs.is_empty() { 0 } else { from + 1 },
                to,
                ixs.len()
            )))
            .child(
                pill("page-next", "Next ›")
                    .small()
                    .disabled((page + 1 >= pages).then_some("This is the last page"))
                    .build(
                        cx,
                        cx.listener(move |this, _, _, cx| {
                            this.page = page + 1;
                            cx.notify();
                        }),
                    ),
            )
            .child(div().flex_1())
            .child(caption("Per page", cx))
            .children(size_pills);

        let list = panel(cx)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(search_row)
            .child(tab_bar)
            .children(admin_intro)
            // The list scrolls inside the page, with a scrollbar, under the bar and above the pager.
            .child(
                v_flex()
                    .id("cleanables")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    // Clear of the scrollbar, which overlays the right edge.
                    .pr(px(22.))
                    .children(rows)
                    .children(empty),
            )
            .child(pager);

        let scope = |plan_only: bool, label: &'static str| {
            pill(if plan_only { "scope-plan" } else { "scope-all" }, label)
                .small()
                .selected(self.breakdown_plan == plan_only)
                .build(
                    cx,
                    cx.listener(move |this, _, _, cx| {
                        this.breakdown_plan = plan_only;
                        this.rebuild_breakdown(cx);
                        cx.notify();
                    }),
                )
        };
        let side = v_flex()
            .w(px(540.))
            .flex_none()
            .gap(px(8.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(scope(false, "All Cleanables"))
                    .child(scope(true, "Cleanup Plan")),
            )
            .child(div().h(px(301.)).child(self.breakdown.clone()));

        v_flex()
            .id("cleaner")
            .size_full()
            .gap(px(12.))
            .p(px(16.))
            .child(div().text_size(px(24.)).line_height(px(32.)).child("Cleaner"))
            .child(div().text_color(muted).child(format!(
                "Temporary files, caches and build artefacts, found in the Index and confirmed by Cleanup Rules (add your own in cleanup.toml). Pre-ticked: temporary files untouched for {} days and build artefacts whose project is untouched for {} days; the limits are in Settings.",
                self.temp_days, self.build_days
            )))
            .children(problems)
            .child(bar)
            .children(self.report_panel(cx).map(|r| div().id("report").max_h(px(160.)).overflow_y_scrollbar().child(r)))
            .child(h_flex().flex_1().min_h_0().gap(px(12.)).items_start().child(list.h_full().flex_1().min_w_0()).child(side))
    }
}

/// What a finished look does to the page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LookVerdict {
    /// Its answer is current: show it.
    Apply,
    /// The user cancelled it and nothing changed since: no new look until asked.
    Pause,
    /// Rules or Volumes changed meanwhile: its answer is for something else.
    Stale,
}

fn look_verdict(page_epoch: u64, look_epoch: u64, cancelled: bool) -> LookVerdict {
    match (page_epoch == look_epoch, cancelled) {
        (false, _) => LookVerdict::Stale,
        (true, true) => LookVerdict::Pause,
        (true, false) => LookVerdict::Apply,
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: the glob would bring gpui's own `test` attribute into scope.
    use super::{Category, CategoryMap, Cleanable, Totals, breakdown, rules};

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

    #[test]
    fn a_cancelled_look_pauses_only_while_its_epoch_is_current() {
        use super::{LookVerdict, look_verdict};
        assert!(look_verdict(3, 3, false) == LookVerdict::Apply);
        assert!(look_verdict(3, 3, true) == LookVerdict::Pause);
        // Rules or Volumes changed meanwhile: neither applied nor latched.
        assert!(look_verdict(4, 3, true) == LookVerdict::Stale);
        assert!(look_verdict(4, 3, false) == LookVerdict::Stale);
    }
}
