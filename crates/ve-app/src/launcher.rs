//! The Launcher: a small keyboard-only window over the Index. A global shortcut (default
//! Alt+Space), the tray menu or `view-everything --launcher` opens it from anywhere, with or
//! without the app's main window. Typing lists Entries whose names match (`launch.match`); Enter
//! opens one with the system handler, Shift+Enter shows it in Browse, Esc or clicking elsewhere
//! closes it. A copy started with `--launcher` while the app is not running has no Engine to ask,
//! and says so.

mod hotkey;

pub use hotkey::{DEFAULT as DEFAULT_SHORTCUT, Shortcut, Status as ShortcutStatus};

use crate::theme::{FS_BODY, FS_SMALL, RADIUS_CARD, ROW_H, palette};
use crate::ui::{Tone, caption, tag};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use ve_engine::Engine;
use ve_engine::launch::{Kind, Match};

const WIDTH: f32 = 640.;
const HEIGHT: f32 = 440.;
/// The window's title, which window lists and tools see; the window itself draws none.
const TITLE: &str = "View Everything Launcher";

/// The one Launcher window of this process, and the text it last held (kept for the session,
/// so reopening continues where the user left off).
#[derive(Default)]
struct Open {
    window: Option<AnyWindowHandle>,
    text: String,
}

impl Global for Open {}

/// Opens the Launcher, or closes it when it is open.
pub fn toggle(cx: &mut App) {
    let open = cx.default_global::<Open>().window;
    if let Some(window) = open
        && window.update(cx, |_, window, _| window.remove_window()).is_ok()
    {
        return;
    }
    show(cx);
}

/// Opens the Launcher, bound to the app's Engine when this process hosts one.
pub fn show(cx: &mut App) {
    if let Some(window) = cx.default_global::<Open>().window
        && window.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }
    let engine = cx.try_global::<crate::host::Host>().map(|h| h.engine.clone());
    let text = cx.global::<Open>().text.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx))),
        titlebar: Some(TitlebarOptions {
            title: Some(TITLE.into()),
            ..Default::default()
        }),
        window_decorations: Some(WindowDecorations::Client),
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("view-everything".into()),
        ..Default::default()
    };
    match gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Launcher::new(engine, text, window, cx))) {
        Ok((window, _)) => {
            cx.global_mut::<Open>().window = Some(window);
            let _ = window.update(cx, |_, window, _| window.activate_window());
        }
        Err(e) => {
            eprintln!("Could not open the Launcher: {e}");
            // A copy started only for the Launcher has nothing else to do.
            if cx.try_global::<crate::host::Host>().is_none() {
                cx.quit();
            }
        }
    }
}

/// What the list shows.
enum Found {
    /// No text typed.
    Nothing,
    Matches(Vec<Match>),
    /// The Engine could not answer (the Index is still loading).
    Failed(String),
}

pub struct Launcher {
    /// `None` in a copy started with `--launcher` while the app is not running.
    engine: Option<Arc<Engine>>,
    input: Entity<InputState>,
    found: Found,
    selected: usize,
    /// Why the last Enter did nothing.
    note: Option<String>,
    /// The search for the latest text; replacing it drops the older one.
    search: Option<Task<()>>,
    scroll: UniformListScrollHandle,
    /// The window has been active: losing that closes it (and not the opening's own blur).
    was_active: bool,
    _subscriptions: Vec<Subscription>,
}

impl Launcher {
    fn new(engine: Option<Arc<Engine>>, text: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Open a file or folder by name")
                .default_value(text.clone())
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        let handle = window.window_handle();
        let me = cx.entity().downgrade();
        let subscriptions = vec![
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.search(text, cx);
                }
            }),
            // Ahead of the text field, which would take Up, Down and Enter for itself.
            cx.intercept_keystrokes(move |event, window, cx| {
                if window.window_handle() != handle {
                    return;
                }
                let shift = event.keystroke.modifiers.shift;
                let handled = me
                    .update(cx, |this, cx| match event.keystroke.key.as_str() {
                        "escape" => {
                            window.remove_window();
                            true
                        }
                        "up" => this.select_by(-1, cx),
                        "down" => this.select_by(1, cx),
                        "enter" => {
                            this.activate(this.selected, shift, window, cx);
                            true
                        }
                        _ => false,
                    })
                    .unwrap_or(false);
                if handled {
                    cx.stop_propagation();
                }
            }),
            cx.observe_window_activation(window, |this, window, _| {
                let active = window.is_window_active();
                if this.was_active && !active {
                    window.remove_window();
                }
                this.was_active |= active;
            }),
        ];
        cx.on_release(|this: &mut Launcher, cx| {
            let text = this.input.read(cx).value().to_string();
            let open = cx.default_global::<Open>();
            open.window = None;
            open.text = text;
            // A copy started only for the Launcher ends with it.
            if cx.try_global::<crate::host::Host>().is_none() {
                cx.quit();
            }
        })
        .detach();
        let mut launcher = Launcher {
            engine,
            input,
            found: Found::Nothing,
            selected: 0,
            note: None,
            search: None,
            scroll: UniformListScrollHandle::new(),
            was_active: false,
            _subscriptions: subscriptions,
        };
        launcher.search(text, cx);
        launcher
    }

    /// Lists the Entries `text` matches, off the UI thread.
    fn search(&mut self, text: String, cx: &mut Context<Self>) {
        self.note = None;
        let Some(engine) = self.engine.clone() else {
            return;
        };
        if text.trim().is_empty() {
            self.search = None;
            self.found = Found::Nothing;
            cx.notify();
            return;
        }
        self.search = Some(cx.spawn(async move |this, cx| {
            let found = cx.background_spawn(async move { engine.launch_match(&text) }).await;
            let _ = this.update(cx, |this, cx| {
                this.found = match found {
                    Ok(matches) => Found::Matches(matches),
                    Err(e) => Found::Failed(e),
                };
                this.selected = 0;
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            });
        }));
    }

    fn matches(&self) -> &[Match] {
        match &self.found {
            Found::Matches(m) => m,
            _ => &[],
        }
    }

    /// Moves the selection `by` rows; `false` when there is nothing to select.
    fn select_by(&mut self, by: isize, cx: &mut Context<Self>) -> bool {
        let n = self.matches().len();
        if n == 0 {
            return false;
        }
        self.selected = (self.selected as isize + by).clamp(0, n as isize - 1) as usize;
        self.scroll.scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
        true
    }

    /// Enter opens the match with the system handler; Shift+Enter shows it in Browse in the
    /// main window. Either closes the Launcher; a match that cannot be opened leaves a note.
    fn activate(&mut self, ix: usize, show_in_browse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.matches().get(ix).cloned() else {
            return;
        };
        self.selected = ix;
        if show_in_browse {
            window.remove_window();
            let path = std::path::PathBuf::from(found.path);
            cx.defer(move |cx| crate::host::Host::show_in_browse(&path, cx));
            return;
        }
        if found.offline {
            self.note = Some(format!("{} is on an Offline Volume: plug it in to open it", found.name));
        } else if found.remote {
            self.note = Some(format!("{} is on a Remote: Shift+Enter shows it in Browse", found.name));
        } else {
            match opener::open(Path::new(&found.path)) {
                Ok(()) => return window.remove_window(),
                Err(e) => self.note = Some(format!("Could not open {}: {e}", found.name)),
            }
        }
        cx.notify();
    }

    fn row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = palette(cx);
        let m = &self.matches()[ix];
        let folder = Path::new(&m.path).parent().map(|f| f.display().to_string()).unwrap_or_default();
        h_flex()
            .id(("launcher-row", ix))
            .w_full()
            .h(px(ROW_H))
            .px(px(16.))
            .gap(px(12.))
            .items_center()
            .when(ix == self.selected, |d| d.bg(p.active))
            .hover(|s| s.bg(p.raised))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, false, window, cx)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(FS_BODY))
                            .text_color(if m.offline { p.muted } else { p.text })
                            .truncate()
                            .child(m.name.clone()),
                    )
                    .child(div().text_size(px(FS_SMALL)).text_color(p.muted).truncate().child(folder)),
            )
            .when(m.offline, |d| d.child(tag("Offline", Tone::Warning, cx)))
            .when(m.remote, |d| d.child(tag("Remote", Tone::Neutral, cx)))
            .child(tag(
                match m.kind {
                    Kind::File => "File",
                    Kind::Folder => "Folder",
                },
                Tone::Neutral,
                cx,
            ))
            .into_any_element()
    }

    /// The line shown instead of the list.
    fn message(&self, cx: &App) -> Option<String> {
        if self.engine.is_none() {
            return Some("View Everything is not running. Start it, then open the Launcher again.".into());
        }
        match &self.found {
            Found::Nothing => Some("Type part of a file or folder name".into()),
            Found::Failed(e) => Some(capitalized(e)),
            Found::Matches(m) if m.is_empty() => Some(format!("No Entry name holds “{}”", self.input.read(cx).value().trim())),
            Found::Matches(_) => None,
        }
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

impl Render for Launcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let body = match self.message(cx) {
            Some(message) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.))
                .text_size(px(FS_BODY))
                .text_color(p.muted)
                .child(message)
                .into_any_element(),
            None => crate::ui::scrolling_list(
                uniform_list(
                    "launcher-list",
                    self.matches().len(),
                    cx.processor(|this, range: Range<usize>, _, cx| range.map(|ix| this.row(ix, cx)).collect::<Vec<_>>()),
                ),
                &self.scroll,
            )
            .into_any_element(),
        };
        let hint = match &self.note {
            Some(note) => div().text_size(px(FS_SMALL)).text_color(p.warning).truncate().child(note.clone()),
            None if self.engine.is_none() => caption("Esc closes", cx),
            None => caption("Enter opens · Shift+Enter shows in Browse · Esc closes", cx),
        };
        v_flex()
            .size_full()
            .bg(p.surface)
            .border_1()
            .border_color(p.control)
            .rounded(px(RADIUS_CARD))
            .overflow_hidden()
            .child(
                h_flex()
                    .h(px(56.))
                    .flex_none()
                    .px(px(8.))
                    .border_b_1()
                    .border_color(p.border)
                    .child(Input::new(&self.input).appearance(false).bordered(false).large()),
            )
            .child(body)
            .child(
                h_flex()
                    .h(px(32.))
                    .flex_none()
                    .px(px(16.))
                    .border_t_1()
                    .border_color(p.border)
                    .child(hint),
            )
    }
}
