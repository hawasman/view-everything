//! The Settings page: which Volumes are indexed, how often they are re-read, and the theme.

use crate::settings::{Settings, ThemeChoice};
use crate::theme::{self, SERIF_FAMILY, palette};
use crate::ui::{caption, panel, pill, tag, Tone};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::IndexPath;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use crate::engine::Intervals;
use std::path::PathBuf;
use ve_scan::VolumeInfo;

/// The refresh choices, in minutes; 0 is Manual (never re-read while running).
const REFRESH_CHOICES: [u64; 6] = [15, 30, 60, 120, 360, 0];

#[derive(Clone)]
struct Every(u64);

impl SelectItem for Every {
    type Value = u64;
    fn value(&self) -> &u64 {
        &self.0
    }
    fn title(&self) -> SharedString {
        match self.0 {
            0 => "Manual".into(),
            m if m % 60 == 0 => format!("{} h", m / 60).into(),
            m => format!("{m} min").into(),
        }
    }
}

type RefreshSelect = Entity<SelectState<Vec<Every>>>;

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
    refresh: RefreshSelect,
    /// One refresh-interval select per Volume, in `volumes` order.
    volume_refresh: Vec<RefreshSelect>,
    /// Set when the settings file could not be written.
    problem: Option<String>,
    /// Fields of the Add Remote dialog.
    remote_host: Entity<InputState>,
    remote_path: Entity<InputState>,
}

/// Whether Remotes can be added yet; until SSH Remotes land the button stays off.
const REMOTES_AVAILABLE: bool = false;

impl EventEmitter<Changed> for SettingsPage {}
impl EventEmitter<Forget> for SettingsPage {}

impl SettingsPage {
    pub fn new(
        settings: Settings,
        volumes: Vec<VolumeInfo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let refresh = Self::refresh_select(settings.refresh_minutes, None, window, cx);
        let volume_refresh = volumes
            .iter()
            .map(|v| Self::refresh_select(settings.refresh_minutes_for(v), Some(v.identity.clone()), window, cx))
            .collect();
        let remote_host = cx.new(|cx| InputState::new(window, cx).placeholder("user@host"));
        let remote_path = cx.new(|cx| InputState::new(window, cx).placeholder("/srv/backup"));
        Self {
            settings,
            offline: Vec::new(),
            volumes,
            refresh,
            volume_refresh,
            problem: None,
            remote_host,
            remote_path,
        }
    }

    /// A select of refresh choices (plus the stored value if it is not one of them) that stores
    /// the pick for the Volume `identity`, or as the default when there is none.
    fn refresh_select(minutes: u64, identity: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> RefreshSelect {
        let mut items: Vec<Every> = REFRESH_CHOICES.map(Every).into();
        if !REFRESH_CHOICES.contains(&minutes) {
            items.insert(items.len() - 1, Every(minutes));
        }
        let at = items.iter().position(|e| e.0 == minutes);
        let select = cx.new(|cx| SelectState::new(items, at.map(IndexPath::new), window, cx));
        cx.subscribe(&select, move |this, _, event: &SelectEvent<Vec<Every>>, cx| {
            let SelectEvent::Confirm(Some(m)) = event else { return };
            match &identity {
                Some(id) => this.settings.volume_refresh_minutes.insert(id.clone(), *m),
                None => std::mem::replace(&mut this.settings.refresh_minutes, *m).into(),
            };
            this.commit(cx);
        })
        .detach();
        select
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

    /// An Offline Volume: its mount, the Offline tag and the Forget link in the reserved slot.
    fn offline_row(&self, ix: usize, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let (identity, mount) = self.offline[ix].clone();
        let forget_mount = mount.clone();
        h_flex()
            .min_h(px(62.))
            .py(px(8.))
            .gap(px(16.))
            .items_center()
            .when(!first, |d| d.border_t_1().border_color(p.rule))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().font_weight(FontWeight::MEDIUM).truncate().child(mount.display().to_string()))
                            .child(tag("Offline", Tone::Warning, cx)),
                    )
                    .child(div().text_xs().text_color(p.muted).child("Searchable, not changeable")),
            )
            .child(self.static_field("On plug-in", cx))
            .child(div().w(px(36.)).flex_none())
            .child(
                div()
                    .id(("forget", ix))
                    .w(px(64.))
                    .flex_none()
                    .text_right()
                    .cursor_pointer()
                    .text_color(p.accent)
                    .child("Forget")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.forget(&identity, &forget_mount);
                        this.commit(cx);
                        cx.emit(Forget(identity.clone()));
                    })),
            )
            .into_any_element()
    }

    fn refresh_field(&self, select: &RefreshSelect) -> Div {
        div().w(px(150.)).flex_none().child(Select::new(select).w_full().small())
    }

    /// The read-only stand-in for a select, as an Offline Volume's refresh interval.
    fn static_field(&self, text: &'static str, cx: &App) -> Div {
        let p = palette(cx);
        h_flex()
            .h(px(32.))
            .w(px(150.))
            .flex_none()
            .px(px(14.))
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(p.control)
            .text_color(p.muted)
            .child(text)
    }

    fn volume_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let (rule, muted) = (p.rule, p.muted);
        let volume = &self.volumes[ix];
        let name = crate::app::volume_name(volume);
        let kind = if volume.removable { "Removable" } else { "Local Volume" };
        let mut notes = vec![volume.mount.display().to_string(), volume.fs_type.clone()];
        if volume.read_only {
            notes.push("read-only".into());
        }
        h_flex()
            .min_h(px(62.))
            .py(px(8.))
            .gap(px(16.))
            .items_center()
            .when(ix > 0, |d| d.border_t_1().border_color(rule))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_center()
                            .child(div().font_weight(FontWeight::MEDIUM).child(name))
                            .child(tag(kind, Tone::Neutral, cx))
                            .when(volume.network, |d| d.child(tag("Network share", Tone::Neutral, cx))),
                    )
                    .child(div().text_xs().text_color(muted).truncate().child(notes.join(" · "))),
            )
            .child(self.refresh_field(&self.volume_refresh[ix]))
            .child(
                div().w(px(36.)).flex_none().child(
                    Switch::new(("volume", ix))
                        .checked(self.settings.is_indexed(volume))
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            let volume = this.volumes[ix].clone();
                            this.settings.set_indexed(&volume, *checked);
                            this.commit(cx);
                        })),
                ),
            )
            // Room for the Forget link of an Offline Volume.
            .child(div().w(px(64.)).flex_none())
            .into_any_element()
    }

    fn theme_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        const CHOICES: [(&str, ThemeChoice); 3] = [
            ("System", ThemeChoice::System),
            ("Light", ThemeChoice::Light),
            ("Dark", ThemeChoice::Dark),
        ];
        let p = palette(cx);
        let (ground, border, text, muted) = (p.ground, p.border, p.text, p.muted);
        h_flex()
            .p(px(3.))
            .rounded_full()
            .children(CHOICES.map(|(label, choice)| {
                let on = self.settings.theme == choice;
                div()
                    .id(label)
                    .h(px(32.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .cursor_pointer()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if on { text } else { muted })
                    .when(on, |d| d.bg(ground).border_1().border_color(border))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.theme = choice;
                        theme::apply(choice, cx);
                        this.commit(cx);
                    }))
            }))
            .into_any_element()
    }

    /// The Add Remote dialog: SSH or S3 details for a read-only Remote. Nothing stands behind
    /// it until issue 15, so adding closes it without creating anything.
    pub fn open_add_remote(&self, window: &mut Window, cx: &mut App) {
        let (host, path) = (self.remote_host.clone(), self.remote_path.clone());
        window.open_dialog(cx, move |dialog, _, cx| {
            let p = palette(cx);
            let (surface, ground, border, muted, text) = (p.surface, p.ground, p.border, p.muted, p.text);
            let field = |label: &'static str, input: &Entity<InputState>| {
                v_flex().gap(px(8.)).child(div().font_weight(FontWeight::MEDIUM).child(label)).child(
                    h_flex()
                        .h(px(38.))
                        .px(px(6.))
                        .items_center()
                        .rounded_full()
                        .border_1()
                        .border_color(p.control)
                        .bg(ground)
                        .child(Input::new(input).appearance(false).bordered(false)),
                )
            };
            let seg = |label: &'static str, on: bool| {
                div()
                    .h(px(32.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if on { text } else { muted })
                    .when(on, |d| d.bg(ground).border_1().border_color(border))
                    .child(label)
            };
            let refresh = h_flex()
                .h(px(38.))
                .px(px(14.))
                .items_center()
                .justify_between()
                .rounded_full()
                .border_1()
                .border_color(p.control)
                .child("Manual")
                .child(Icon::new(IconName::ChevronDown).size(px(14.)).text_color(muted));
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
                        .child("Add Remote"),
                )
                .child(
                    v_flex()
                        .gap(px(11.))
                        .child(div().text_color(muted).child("Remotes are read-only: listed and sized, never changed, no Trash."))
                        .child(h_flex().child(h_flex().p(px(3.)).rounded_full().bg(surface).child(seg("SSH", true)).child(seg("S3", false))))
                        .child(field("Host", &host))
                        .child(field("Path", &path))
                        .child(v_flex().gap(px(8.)).child(div().font_weight(FontWeight::MEDIUM).child("Refresh")).child(refresh)),
                )
                .footer(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(div().flex_1().text_xs().text_color(muted).child("Credentials stay in the system keychain."))
                        .child(pill("remote-cancel", "Cancel").build(cx, |_, window, cx| window.close_dialog(cx)))
                        .child(pill("remote-add", "Add Remote").primary().build(cx, |_, window, cx| window.close_dialog(cx))),
                )
        });
    }
}

fn section(title: &'static str, note: &'static str, body: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .gap(px(16.))
        .items_start()
        .child(
            v_flex()
                .w(px(356.))
                .pr(px(24.))
                .pt(px(3.))
                .flex_none()
                .gap(px(6.))
                .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).child(title))
                .child(div().text_color(palette(cx).muted).child(note)),
        )
        .child(div().flex_1().min_w_0().child(body))
}

impl Render for SettingsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let (rule, muted, border) = (p.rule, p.muted, p.border);
        let rows: Vec<_> = (0..self.volumes.len()).map(|ix| self.volume_row(ix, cx)).collect();
        let offline: Vec<_> =
            (0..self.offline.len()).map(|ix| self.offline_row(ix, rows.is_empty() && ix == 0, cx)).collect();
        let indexed = self.volumes.iter().filter(|v| self.settings.is_indexed(v)).count();
        let add = pill("add-remote", "Add Remote")
            .small()
            .disabled((!REMOTES_AVAILABLE).then_some("Available once SSH Remotes land"))
            .build(cx, cx.listener(|this, _, window, cx| this.open_add_remote(window, cx)));
        let head = h_flex()
            .pb(px(7.))
            .text_xs()
            .text_color(muted)
            .gap(px(16.))
            .child(div().flex_1().child("Location"))
            .child(div().w(px(150.)).flex_none().child("Refresh interval"))
            .child(div().w(px(36.)).flex_none().child("Index"))
            .child(div().w(px(64.)).flex_none());
        let default = h_flex()
            .mt(px(8.))
            .pt(px(12.))
            .gap(px(16.))
            .items_center()
            .border_t_1()
            .border_color(rule)
            .child(
                v_flex().flex_1().child(div().font_weight(FontWeight::MEDIUM).child("Default refresh interval")).child(
                    div().text_xs().text_color(muted).child("For Volumes without their own interval, re-read while running"),
                ),
            )
            .child(self.refresh_field(&self.refresh))
            .child(div().w(px(36.)).flex_none())
            .child(div().w(px(64.)).flex_none());
        let locations = panel(cx)
            .child(h_flex().justify_end().mb(px(14.)).child(add))
            .child(head)
            .children(rows)
            .children(offline)
            .child(default);
        let showing = if cx.theme().is_dark() { "dark" } else { "light" };
        let theme = panel(cx)
            .flex()
            .items_center()
            .justify_between()
            .child(self.theme_buttons(cx))
            .child(caption(format!("Showing {showing}"), cx));
        let mut column = v_flex()
            .w_full()
            .max_w(px(1164.))
            .mx_auto()
            .px(px(32.))
            .pt(px(32.))
            .pb(px(48.))
            .gap(px(24.))
            .child(
                v_flex()
                    .gap(px(6.))
                    .mb(px(8.))
                    .child(div().text_size(px(24.)).line_height(px(32.)).child("Settings"))
                    .child(div().text_color(muted).child(format!(
                        "{indexed} of {} Volumes are indexed. Changes save as you make them.",
                        self.volumes.len()
                    ))),
            )
            .child(section(
                "Locations",
                "Volumes are indexed by default. Removable Volumes and mounted network shares are opt-in. An Offline Volume stays searchable until you forget it.",
                locations,
                cx,
            ))
            .child(div().h(px(1.)).bg(border))
            .child(section("Theme", "System follows the operating system and changes with it.", theme, cx));
        if let Some(problem) = &self.problem {
            column = column.child(div().text_color(p.warning).child(problem.clone()));
        }
        div().id("settings").size_full().overflow_y_scroll().child(column)
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
            .map(|v| Self::refresh_select(self.settings.refresh_minutes_for(v), Some(v.identity.clone()), window, cx))
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
