//! The Cleaner: Cleanables found from the Index by Cleanup Rules, reviewed as a Cleanup Plan
//! and removed permanently by Clean (ADR-0008). The rules themselves are in `rules`.

mod rules;

use crate::engine::{Repaint, Shared};
use crate::format;
use crate::settings::{Settings, cleanup_path};
use crate::theme::palette;
use crate::ui::{caption, panel, pill};
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rules::{Cleanable, CommandReport, Rule, Rules, clean};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use ve_scan::VolumeInfo;

type Key = (String, PathBuf);

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
    cleanables: Vec<Cleanable>,
    /// Cleanables already shown, so one that appears later is pre-ticked once and a choice of the
    /// user's stays.
    known: HashSet<Key>,
    ticked: HashSet<Key>,
    /// What the commands of the last Clean said.
    report: Vec<CommandReport>,
    advanced: bool,
}

fn rules_stamp() -> Option<SystemTime> {
    std::fs::metadata(cleanup_path()?).ok()?.modified().ok()
}

impl CleanerPage {
    pub fn new(shared: Arc<Shared>, repaint: Repaint, volumes: Vec<VolumeInfo>, settings: &Settings) -> Self {
        Self {
            shared,
            repaint,
            volumes,
            rules: rules::load(cleanup_path().as_deref()),
            rules_stamp: rules_stamp(),
            temp_days: settings.cleaner_temp_days,
            build_days: settings.cleaner_build_days,
            seen: None,
            cleanables: Vec::new(),
            known: HashSet::new(),
            ticked: HashSet::new(),
            report: Vec::new(),
            advanced: false,
        }
    }

    pub fn set_volumes(&mut self, volumes: Vec<VolumeInfo>, cx: &mut Context<Self>) {
        self.volumes = volumes;
        self.seen = None;
        cx.notify();
    }

    /// Takes the age limits; new ones start the Cleanup Plan over with the new pre-ticking.
    pub fn set_settings(&mut self, settings: &Settings, cx: &mut Context<Self>) {
        if (self.temp_days, self.build_days) != (settings.cleaner_temp_days, settings.cleaner_build_days) {
            self.temp_days = settings.cleaner_temp_days;
            self.build_days = settings.cleaner_build_days;
            self.known.clear();
            self.ticked.clear();
            self.seen = None;
            cx.notify();
        }
    }

    /// Looks for Cleanables again when the Index or `cleanup.toml` changed since the last look.
    // ponytail: scans on the render thread; move to a background thread if a 10M-Entry Index makes it visible.
    fn refresh(&mut self) {
        let stamp = rules_stamp();
        if stamp != self.rules_stamp {
            self.rules_stamp = stamp;
            self.rules = rules::load(cleanup_path().as_deref());
            self.seen = None;
        }
        let index = self.shared.index.read();
        if self.seen == Some(index.generation()) {
            return;
        }
        self.cleanables = rules::find(&index, &self.volumes, &self.rules.rules);
        self.seen = Some(index.generation());
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
        for c in &self.cleanables {
            if self.known.insert(c.key()) && c.pre_ticked(now, self.temp_days, self.build_days) {
                self.ticked.insert(c.key());
            }
        }
        let present: HashSet<Key> = self.cleanables.iter().map(Cleanable::key).collect();
        self.known.retain(|k| present.contains(k));
        self.ticked.retain(|k| present.contains(k));
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
        let shared = self.shared.clone();
        window
            .spawn(cx, {
                let this = cx.entity();
                async move |cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            let outcome = clean(&plan, &volumes);
                            // Whatever a command changed is read back into the Index: only its working
                            // folder, since a command rule's effect is confined to it (the Volume would
                            // take minutes to re-walk).
                            for command in outcome.commands.iter().filter(|c| c.result.is_ok()) {
                                let folder = shared.index.read().find_path(&command.cwd);
                                if let Some(folder) = folder {
                                    let _ = ve_scan::sync_folder(&shared.index, folder, &command.cwd, &shared.progress);
                                }
                            }
                            outcome
                        })
                        .await;
                    cx.update(|window, cx| {
                        let (shared, repaint) = {
                            let page = this.read(cx);
                            (page.shared.clone(), page.repaint.clone())
                        };
                        {
                            let mut index = shared.index.write();
                            // Whatever left the disk leaves the Index, skipped-because-vanished included.
                            for path in outcome.removed.iter().chain(
                                outcome
                                    .skipped
                                    .iter()
                                    .filter(|(p, _)| std::fs::symlink_metadata(p).is_err())
                                    .map(|(p, _)| p),
                            ) {
                                if let Some(id) = index.find_path(path) {
                                    index.remove(id);
                                }
                            }
                        }
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
        self.refresh();
        let p = palette(cx);
        let (muted, rule, warning) = (p.muted, p.rule, p.warning);
        let plan = self.plan();
        let (count, total) = (plan.len(), plan.iter().map(|c| c.on_disk).sum::<u64>());
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut group = None;
        for (ix, c) in self.cleanables.iter().enumerate() {
            if group != Some(c.rule.group) {
                group = Some(c.rule.group);
                rows.push(
                    div()
                        .mt(px(if rows.is_empty() { 0. } else { 12. }))
                        .pb(px(4.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(c.rule.group.title())
                        .into_any_element(),
                );
            }
            let key = c.key();
            let by = if c.rule.is_command() {
                format!("{}: runs {}", c.rule.name, c.rule.argv_text())
            } else {
                c.rule.name.clone()
            };
            rows.push(
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
                    .into_any_element(),
            );
        }
        let advanced = self.advanced.then(|| {
            let paths: Vec<_> = plan.iter().filter(|c| !c.rule.is_command()).collect();
            let commands: Vec<_> = plan.iter().filter(|c| c.rule.is_command()).collect();
            v_flex()
                .gap_1()
                .mt(px(12.))
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
                .when(count == 0, |d| d.child(caption("Nothing ticked.", cx)))
        });
        let problems = (!self.rules.errors.is_empty()).then(|| {
            v_flex()
                .gap_1()
                .text_color(warning)
                .child("Some Cleanup Rules were not read; the others still work:")
                .children(self.rules.errors.iter().map(|e| div().child(e.clone())))
        });
        div().id("cleaner").size_full().overflow_y_scroll().child(
            v_flex()
                .gap(px(16.))
                .p(px(16.))
                .child(div().text_size(px(24.)).line_height(px(32.)).child("Cleaner"))
                .child(div().text_color(muted).child(format!(
                    "Temporary files, caches and build artefacts, found in the Index and confirmed by Cleanup Rules (add your own in cleanup.toml). Pre-ticked: temporary files untouched for {} days and build artefacts whose project is untouched for {} days; the limits are in Settings.",
                    self.temp_days, self.build_days
                )))
                .children(problems)
                .child(
                    panel(cx)
                        .child(v_flex().children(rows))
                        .when(self.cleanables.is_empty(), |d| d.child("Nothing to clean."))
                        .child(
                            h_flex()
                                .mt(px(12.))
                                .gap(px(8.))
                                .items_center()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(format!("Cleanup Plan: {count} selected, {}", format::size(total))),
                                )
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
                                        .disabled((count == 0).then_some("Tick what to clean first"))
                                        .build(cx, cx.listener(|this, _, window, cx| this.ask_clean(window, cx))),
                                ),
                        )
                        .children(advanced),
                )
                .children(self.report_panel(cx)),
        )
    }
}
