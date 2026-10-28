//! The Settings page: which Volumes are indexed, how often they are re-read, and the theme.

use crate::settings::{Settings, ThemeChoice};
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::Selectable as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, InputState, NumberInput};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, h_flex, v_flex};
use gpui_kit::*;
use crate::engine::Intervals;
use std::path::PathBuf;
use ve_scan::VolumeInfo;

const REFRESH_RANGE: std::ops::RangeInclusive<u64> = 1..=1440;

/// What the Engine must be told after the user changes a setting.
pub struct Changed {
    pub mounted: Vec<VolumeInfo>,
    pub indexed: Vec<VolumeInfo>,
    pub intervals: Intervals,
}

/// The user asked to Forget the Offline Volume with this identity.
pub struct Forget(pub String);

pub struct SettingsPage {
    settings: Settings,
    /// Offline Volumes as (identity, last mount path).
    offline: Vec<(String, PathBuf)>,
    volumes: Vec<VolumeInfo>,
    refresh: Entity<InputState>,
    /// One refresh-interval field per Volume, in `volumes` order.
    volume_refresh: Vec<Entity<InputState>>,
    /// Set when the settings file could not be written.
    problem: Option<String>,
}

impl EventEmitter<Changed> for SettingsPage {}
impl EventEmitter<Forget> for SettingsPage {}

/// Applies the theme choice; the Geist fonts set at startup are kept.
pub fn apply_theme(choice: ThemeChoice, cx: &mut App) {
    match choice {
        ThemeChoice::System => Theme::sync_system_appearance(None, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

impl SettingsPage {
    pub fn new(
        settings: Settings,
        volumes: Vec<VolumeInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let minutes_input = |minutes: u64, window: &mut Window, cx: &mut Context<Self>| {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .step(1.0)
                    .min(*REFRESH_RANGE.start() as f64)
                    .max(*REFRESH_RANGE.end() as f64)
            });
            input.update(cx, |input, cx| input.set_value(minutes.to_string(), window, cx));
            input
        };
        let refresh = minutes_input(settings.refresh_minutes, window, cx);
        let volume_refresh: Vec<_> = volumes
            .iter()
            .map(|v| minutes_input(settings.refresh_minutes_for(v), window, cx))
            .collect();
        for (ix, input) in volume_refresh.iter().enumerate() {
            // Same rule as the default field: taken on Enter or when the field is left.
            cx.subscribe_in(input, window, move |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } | InputEvent::Blur = event {
                    let volume = this.volumes[ix].clone();
                    let minutes = input.read(cx).value().trim().parse::<u64>().ok();
                    if let Some(m) = minutes.filter(|m| REFRESH_RANGE.contains(m))
                        && m != this.settings.refresh_minutes_for(&volume)
                    {
                        this.settings.volume_refresh_minutes.insert(volume.identity.clone(), m);
                        this.commit(cx);
                    }
                    let text = this.settings.refresh_minutes_for(&volume).to_string();
                    input.update(cx, |input, cx| input.set_value(text, window, cx));
                }
            })
            .detach();
        }
        cx.subscribe_in(&refresh, window, |this, input, event: &InputEvent, window, cx| {
            match event {
                // Typing "15" must not apply 1 on the way, so the value is taken on Enter or when
                // the field is left; junk puts the applied value back.
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    let minutes = input.read(cx).value().trim().parse::<u64>().ok();
                    if let Some(m) = minutes.filter(|m| REFRESH_RANGE.contains(m))
                        && m != this.settings.refresh_minutes
                    {
                        this.settings.refresh_minutes = m;
                        this.commit(cx);
                    }
                    let text = this.settings.refresh_minutes.to_string();
                    input.update(cx, |input, cx| input.set_value(text, window, cx));
                }
                _ => {}
            }
        })
        .detach();
        Self {
            settings,
            offline: Vec::new(),
            volumes,
            refresh,
            volume_refresh,
            problem: None,
        }
    }

    pub fn has_problem(&self) -> bool {
        self.problem.is_some()
    }

    /// Saves the settings and tells the window what to apply.
    fn commit(&mut self, cx: &mut Context<Self>) {
        self.problem = self
            .settings
            .save()
            .err()
            .map(|e| format!("Could not save settings: {e}"));
        self.emit_changed(cx);
    }

    fn emit_changed(&mut self, cx: &mut Context<Self>) {
        let indexed: Vec<VolumeInfo> = self
            .volumes
            .iter()
            .filter(|v| self.settings.is_indexed(v))
            .cloned()
            .collect();
        cx.emit(Changed {
            mounted: self.volumes.clone(),
            intervals: self.settings.intervals(&indexed),
            indexed,
        });
        cx.notify();
    }

    /// Replaces the list of Offline Volumes the Engine reports.
    pub fn set_offline(&mut self, offline: Vec<(String, PathBuf)>, cx: &mut Context<Self>) {
        if self.offline != offline {
            self.offline = offline;
            cx.notify();
        }
    }

    fn offline_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let (identity, mount) = self.offline[ix].clone();
        h_flex()
            .gap_3()
            .items_center()
            .child(div().child(mount.display().to_string()))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("offline · searchable, not changeable"),
            )
            .child(Button::new(("forget", ix)).label("Forget").on_click(cx.listener(
                move |this, _, _, cx| {
                    this.settings.forget(&identity);
                    this.commit(cx);
                    cx.emit(Forget(identity.clone()));
                },
            )))
            .into_any_element()
    }

    fn volume_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let volume = &self.volumes[ix];
        let mut notes = vec![volume.fs_type.clone()];
        if !volume.label.is_empty() {
            notes.push(volume.label.clone());
        }
        if volume.removable {
            notes.push("removable".into());
        }
        if volume.network {
            notes.push("Network share".into());
        }
        if volume.read_only {
            notes.push("read-only".into());
        }
        h_flex()
            .gap_3()
            .items_center()
            .child(
                Checkbox::new(("volume", ix))
                    .label(volume.mount.display().to_string())
                    .checked(self.settings.is_indexed(volume))
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let volume = this.volumes[ix].clone();
                        this.settings.set_indexed(&volume, *checked);
                        this.commit(cx);
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(notes.join(" · ")),
            )
            .child(div().text_sm().child("re-read every"))
            .child(div().w(px(120.)).child(NumberInput::new(&self.volume_refresh[ix])))
            .child(div().text_sm().child("min"))
            .into_any_element()
    }

    fn theme_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        const CHOICES: [(&str, ThemeChoice); 3] = [
            ("System", ThemeChoice::System),
            ("Light", ThemeChoice::Light),
            ("Dark", ThemeChoice::Dark),
        ];
        ButtonGroup::new("theme")
            .outline()
            .children(CHOICES.map(|(label, choice)| {
                Button::new(label)
                    .label(label)
                    .selected(self.settings.theme == choice)
            }))
            .on_click(cx.listener(|this, clicks: &Vec<usize>, _, cx| {
                if let Some(&(_, choice)) = clicks.first().and_then(|&i| CHOICES.get(i)) {
                    this.settings.theme = choice;
                    apply_theme(choice, cx);
                    this.commit(cx);
                }
            }))
            .into_any_element()
    }
}

fn section(title: &'static str, note: &'static str, body: impl IntoElement, muted: Hsla) -> Div {
    v_flex()
        .gap_2()
        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
        .child(div().text_sm().text_color(muted).child(note))
        .child(body)
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = (0..self.volumes.len()).map(|ix| self.volume_row(ix, cx)).collect();
        let offline: Vec<_> = (0..self.offline.len()).map(|ix| self.offline_row(ix, cx)).collect();
        let volumes = v_flex().gap_2().children(rows).children(offline);
        let muted = cx.theme().muted_foreground;
        let refresh = h_flex()
            .gap_2()
            .items_center()
            .child("Volumes without their own interval are re-read every")
            .child(div().w(px(140.)).child(NumberInput::new(&self.refresh)))
            .child("minutes while running");
        let theme = self.theme_buttons(cx);
        let mut page = v_flex()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_6()
            .child(section(
                "Indexed Volumes",
                "Local fixed Volumes are indexed by default; removable ones and network shares only when ticked.",
                volumes,
                muted,
            ))
            .child(section(
                "Default refresh interval",
                "A walked Volume is re-read this often while the app runs, and at launch only if its Last Synced is older. Set a different interval beside any Volume above.",
                refresh,
                muted,
            ))
            .child(section("Theme", "Light, Dark, or following the system.", theme, muted));
        if let Some(problem) = &self.problem {
            page = page.child(div().text_color(cx.theme().warning).child(problem.clone()));
        }
        page
    }
}

/// Hot-plug: the window hands the Settings page the Volumes mounted now and changes settings
/// through it, so the page stays the one owner (and writer) of the settings.
impl SettingsPage {
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Takes the Volumes mounted now, with a refresh field for each, and tells the window to
    /// apply the result.
    pub fn set_mounted(&mut self, volumes: Vec<VolumeInfo>, window: &mut Window, cx: &mut Context<Self>) {
        self.volume_refresh = volumes
            .iter()
            .map(|v| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .step(1.0)
                        .min(*REFRESH_RANGE.start() as f64)
                        .max(*REFRESH_RANGE.end() as f64)
                });
                let text = self.settings.refresh_minutes_for(v).to_string();
                input.update(cx, |input, cx| input.set_value(text, window, cx));
                let identity = v.identity.clone();
                cx.subscribe_in(&input, window, move |this, input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } | InputEvent::Blur = event
                        && let Some(volume) = this.volumes.iter().find(|v| v.identity == identity).cloned()
                    {
                        let minutes = input.read(cx).value().trim().parse::<u64>().ok();
                        if let Some(m) = minutes.filter(|m| REFRESH_RANGE.contains(m))
                            && m != this.settings.refresh_minutes_for(&volume)
                        {
                            this.settings.volume_refresh_minutes.insert(identity.clone(), m);
                            this.commit(cx);
                        }
                        let text = this.settings.refresh_minutes_for(&volume).to_string();
                        input.update(cx, |input, cx| input.set_value(text, window, cx));
                    }
                })
                .detach();
                input
            })
            .collect();
        self.volumes = volumes;
        // Nothing to save: only what is mounted changed (and a settings file that could not be
        // read at startup must not be overwritten by a hot-plug).
        self.emit_changed(cx);
    }

    /// Changes the settings with `change`, saves them and tells the window.
    pub fn apply(&mut self, change: impl FnOnce(&mut Settings), cx: &mut Context<Self>) {
        change(&mut self.settings);
        self.commit(cx);
    }
}
