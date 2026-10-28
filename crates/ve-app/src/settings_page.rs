//! The Settings page: which Volumes are indexed, how often they are re-read, and the theme.

use crate::settings::{Settings, ThemeChoice};
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::Selectable as _;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{InputEvent, InputState, NumberInput};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, h_flex, v_flex};
use gpui_kit::*;
use std::time::Duration;
use ve_scan::VolumeInfo;

const REFRESH_RANGE: std::ops::RangeInclusive<u64> = 1..=1440;

/// What the Engine must be told after the user changes a setting.
pub struct Changed {
    pub indexed: Vec<VolumeInfo>,
    pub refresh: Duration,
}

pub struct SettingsPage {
    settings: Settings,
    volumes: Vec<VolumeInfo>,
    refresh: Entity<InputState>,
    /// Set when the settings file could not be written.
    problem: Option<String>,
}

impl EventEmitter<Changed> for SettingsPage {}

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
        let refresh = cx.new(|cx| {
            InputState::new(window, cx)
                .step(1.0)
                .min(*REFRESH_RANGE.start() as f64)
                .max(*REFRESH_RANGE.end() as f64)
        });
        refresh.update(cx, |input, cx| {
            input.set_value(settings.refresh_minutes.to_string(), window, cx)
        });
        cx.subscribe_in(&refresh, window, |this, input, event: &InputEvent, window, cx| {
            match event {
                InputEvent::Change => {
                    let minutes = input.read(cx).value().trim().parse::<u64>().ok();
                    if let Some(m) = minutes.filter(|m| REFRESH_RANGE.contains(m))
                        && m != this.settings.refresh_minutes
                    {
                        this.settings.refresh_minutes = m;
                        this.commit(cx);
                    }
                }
                // Leaving the field with junk in it puts the applied value back.
                InputEvent::Blur => {
                    let text = this.settings.refresh_minutes.to_string();
                    input.update(cx, |input, cx| input.set_value(text, window, cx));
                }
                _ => {}
            }
        })
        .detach();
        Self {
            settings,
            volumes,
            refresh,
            problem: None,
        }
    }

    /// Saves the settings and tells the window what to apply.
    fn commit(&mut self, cx: &mut Context<Self>) {
        self.problem = self
            .settings
            .save()
            .err()
            .map(|e| format!("Could not save settings: {e}"));
        cx.emit(Changed {
            indexed: self
                .volumes
                .iter()
                .filter(|v| self.settings.is_indexed(v))
                .cloned()
                .collect(),
            refresh: Duration::from_secs(self.settings.refresh_minutes * 60),
        });
        cx.notify();
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
        if volume.read_only {
            notes.push("read-only".into());
        }
        h_flex()
            .gap_3()
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
        let volumes = v_flex().gap_2().children(rows);
        let muted = cx.theme().muted_foreground;
        let refresh = h_flex()
            .gap_2()
            .items_center()
            .child("Re-read walked Volumes every")
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
                "Local fixed Volumes are indexed by default; removable ones only when ticked.",
                volumes,
                muted,
            ))
            .child(section("Refresh interval", "How often walked Volumes are re-read.", refresh, muted))
            .child(section("Theme", "Light, Dark, or following the system.", theme, muted));
        if let Some(problem) = &self.problem {
            page = page.child(div().text_color(cx.theme().warning).child(problem.clone()));
        }
        page
    }
}
