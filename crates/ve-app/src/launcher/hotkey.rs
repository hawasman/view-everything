//! The Launcher's global shortcut. On a Wayland desktop it is asked of the GlobalShortcuts portal
//! (the desktop may bind another key, or none); elsewhere (X11, Windows, macOS) the key is grabbed
//! directly, which fails when another program holds it. Either way [`Shortcut`] says what came of
//! it, for Settings to show, and calls `on_press` when the key is pressed.

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use gpui_kit::*;
use std::fmt;
use std::rc::Rc;

/// The shortcut a new settings file starts with.
pub const DEFAULT: &str = "Alt+Space";

/// What a shortcut text names: modifiers and one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Keys {
    ctrl: bool,
    alt: bool,
    shift: bool,
    /// The Windows, Command or Meta key.
    logo: bool,
    key: Key,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Letter(u8),
    Digit(u8),
    F(u8),
    Space,
    Enter,
    Tab,
}

impl Keys {
    /// Reads text such as `Alt+Space` or `ctrl+shift+k`: `+`-separated, any case.
    pub fn parse(text: &str) -> Result<Keys, String> {
        let mut keys = Keys {
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
            key: Key::Space,
        };
        let mut key = None;
        for part in text.split('+').map(str::trim) {
            let lower = part.to_ascii_lowercase();
            let flag = match lower.as_str() {
                "ctrl" | "control" => &mut keys.ctrl,
                "alt" | "option" => &mut keys.alt,
                "shift" => &mut keys.shift,
                "super" | "meta" | "win" | "logo" | "cmd" | "command" => &mut keys.logo,
                _ => {
                    if key.is_some() {
                        return Err(format!(
                            "“{text}” names more than one key; use one key with modifiers, e.g. Alt+Space"
                        ));
                    }
                    key = Some(Self::key(&lower).ok_or_else(|| {
                        if part.is_empty() {
                            format!("“{text}” has an empty part")
                        } else {
                            format!("“{part}” is not a key this shortcut can use: a letter, digit, F1–F24, Space, Enter or Tab")
                        }
                    })?);
                    continue;
                }
            };
            *flag = true;
        }
        keys.key = key.ok_or_else(|| format!("“{text}” names no key, e.g. Alt+Space"))?;
        let modified = keys.ctrl || keys.alt || keys.shift || keys.logo;
        if !modified && !matches!(keys.key, Key::F(_)) {
            return Err(format!(
                "“{text}” needs Ctrl, Alt, Shift or Super, or it would take the key from typing"
            ));
        }
        Ok(keys)
    }

    fn key(lower: &str) -> Option<Key> {
        let bytes = lower.as_bytes();
        match lower {
            "space" => return Some(Key::Space),
            "enter" | "return" => return Some(Key::Enter),
            "tab" => return Some(Key::Tab),
            _ => {}
        }
        if let [b] = bytes {
            return match b {
                b'a'..=b'z' => Some(Key::Letter(*b)),
                b'0'..=b'9' => Some(Key::Digit(*b)),
                _ => None,
            };
        }
        let n: u8 = lower.strip_prefix('f')?.parse().ok()?;
        (1..=24).contains(&n).then_some(Key::F(n))
    }

    /// The trigger as the XDG shortcuts specification writes it, for the portal.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn portal_trigger(&self) -> String {
        let mut text = String::new();
        for (on, name) in [(self.ctrl, "CTRL"), (self.alt, "ALT"), (self.shift, "SHIFT"), (self.logo, "LOGO")] {
            if on {
                text.push_str(name);
                text.push('+');
            }
        }
        match self.key {
            Key::Letter(b) | Key::Digit(b) => text.push(b as char),
            Key::F(n) => text.push_str(&format!("F{n}")),
            Key::Space => text.push_str("space"),
            Key::Enter => text.push_str("Return"),
            Key::Tab => text.push_str("Tab"),
        }
        text
    }

    fn hot_key(&self) -> HotKey {
        let mut mods = Modifiers::empty();
        for (on, m) in [
            (self.ctrl, Modifiers::CONTROL),
            (self.alt, Modifiers::ALT),
            (self.shift, Modifiers::SHIFT),
            (self.logo, Modifiers::META),
        ] {
            if on {
                mods |= m;
            }
        }
        // The W3C UI Events code names, which `Code` parses.
        let name = match self.key {
            Key::Letter(b) => format!("Key{}", b.to_ascii_uppercase() as char),
            Key::Digit(b) => format!("Digit{}", b as char),
            Key::F(n) => format!("F{n}"),
            Key::Space => "Space".to_owned(),
            Key::Enter => "Enter".to_owned(),
            Key::Tab => "Tab".to_owned(),
        };
        let code = name.parse::<Code>().expect("every Key names a Code");
        HotKey::new(Some(mods), code)
    }
}

/// The shortcut as Settings shows it, e.g. `Ctrl+Alt+K`.
impl fmt::Display for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.logo, "Super")] {
            if on {
                write!(f, "{name}+")?;
            }
        }
        match self.key {
            Key::Letter(b) => write!(f, "{}", b.to_ascii_uppercase() as char),
            Key::Digit(b) => write!(f, "{}", b as char),
            Key::F(n) => write!(f, "F{n}"),
            Key::Space => f.write_str("Space"),
            Key::Enter => f.write_str("Enter"),
            Key::Tab => f.write_str("Tab"),
        }
    }
}

/// What came of asking for the shortcut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Waiting for the desktop's answer.
    Pending,
    /// Pressing `trigger` opens the Launcher. `trigger` is what the desktop reports; on Wayland
    /// it may differ from what was asked for (`as_asked` is false then).
    Active { trigger: String, as_asked: bool },
    /// The desktop would not give it, with its reason.
    Refused(String),
}

/// Where the key comes from.
enum Backend {
    None,
    /// Grabbed directly (X11, Windows, macOS).
    Grab {
        manager: global_hotkey::GlobalHotKeyManager,
        key: HotKey,
    },
    /// Bound through the GlobalShortcuts portal: dropping the task stops listening, and the
    /// session, kept as soon as it exists, is closed on release so the desktop forgets the binding.
    #[cfg(target_os = "linux")]
    Portal {
        _listening: Task<()>,
        session: portal::Kept,
    },
}

/// The Launcher's global shortcut, re-bound when Settings changes it.
pub struct Shortcut {
    status: Status,
    backend: Backend,
    on_press: Rc<dyn Fn(&mut App)>,
}

impl Shortcut {
    /// Asks for the shortcut `text` names and calls `on_press` each time it is pressed.
    pub fn start(text: &str, on_press: impl Fn(&mut App) + 'static, cx: &mut App) -> Entity<Shortcut> {
        let shortcut = cx.new(|_| Shortcut {
            status: Status::Pending,
            backend: Backend::None,
            on_press: Rc::new(on_press),
        });
        let text = text.to_owned();
        shortcut.update(cx, |s, cx| {
            if let Err(e) = s.set(&text, cx) {
                s.status = Status::Refused(e);
            }
        });
        shortcut
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Replaces the shortcut with the one `text` names, returning it as Settings writes it.
    /// `Err` (nothing changes) when the text names no usable shortcut.
    pub fn set(&mut self, text: &str, cx: &mut Context<Self>) -> Result<String, String> {
        let keys = Keys::parse(text)?;
        self.release(cx);
        self.status = Status::Pending;
        if wayland() {
            #[cfg(target_os = "linux")]
            self.bind_portal(keys, cx);
        } else {
            self.grab(keys, cx);
        }
        cx.notify();
        Ok(keys.to_string())
    }

    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn release(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.backend, Backend::None) {
            Backend::Grab { manager, key } => {
                let _ = manager.unregister(key);
            }
            #[cfg(target_os = "linux")]
            Backend::Portal { session, .. } => {
                if let Some(session) = session.borrow_mut().take() {
                    cx.spawn(async move |_, _| {
                        let _ = session.close().await;
                    })
                    .detach();
                }
            }
            Backend::None => {}
        }
    }

    fn grab(&mut self, keys: Keys, cx: &mut Context<Self>) {
        #[cfg(all(unix, not(target_os = "macos")))]
        if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
            self.status = Status::Refused("No X11 display to register a global shortcut with".into());
            return;
        }
        let manager = match global_hotkey::GlobalHotKeyManager::new() {
            Ok(manager) => manager,
            Err(e) => {
                self.status = Status::Refused(e.to_string());
                return;
            }
        };
        let key = keys.hot_key();
        if let Err(e) = manager.register(key) {
            self.status = Status::Refused(match e {
                global_hotkey::Error::AlreadyRegistered(_) => format!("{keys} is already taken by another program"),
                e => e.to_string(),
            });
            return;
        }
        self.status = Status::Active {
            trigger: keys.to_string(),
            as_asked: true,
        };
        self.backend = Backend::Grab { manager, key };
        listen_for_grabs(cx);
    }

    #[cfg(target_os = "linux")]
    fn bind_portal(&mut self, keys: Keys, cx: &mut Context<Self>) {
        let session = portal::Kept::default();
        let kept = session.clone();
        let listening = cx.spawn(async move |this, cx| {
            let (presses, status) = match portal::bind(&keys.portal_trigger(), &kept).await {
                Ok((presses, trigger)) => {
                    // The desktop may spell the trigger its own way; read back, it may still be the one asked for.
                    let as_asked = Keys::parse(&trigger) == Ok(keys);
                    (Some(presses), Status::Active { trigger, as_asked })
                }
                Err(reason) => (None, Status::Refused(reason)),
            };
            let still_here = this.update(cx, |s, cx| {
                s.status = status;
                cx.notify();
            });
            let (Some(mut presses), Ok(())) = (presses, still_here) else {
                return;
            };
            while presses.next().await.is_some() {
                let Ok(on_press) = this.read_with(cx, |s, _| s.on_press.clone()) else {
                    return;
                };
                cx.update(|cx| on_press(cx));
            }
        });
        self.backend = Backend::Portal {
            _listening: listening,
            session,
        };
    }
}

/// Whether the desktop is a Wayland session, where only the portal can give a global shortcut.
/// That holds for an X11 (Xwayland) window too: the compositor sees every key first, so an X11
/// grab "succeeds" yet never sees a key the compositor binds itself (KDE's Alt+Space).
fn wayland() -> bool {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    cfg!(target_os = "linux") && (set("WAYLAND_DISPLAY") || std::env::var_os("XDG_SESSION_TYPE").is_some_and(|t| t == "wayland"))
}

/// Forwards presses of grabbed keys to the app's thread. The handler is process-wide and set
/// once; the one Shortcut entity alive receives them.
fn listen_for_grabs(cx: &mut Context<Shortcut>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let mut first = false;
    STARTED.call_once(|| first = true);
    if !first {
        return;
    }
    let (tx, rx) = async_channel::unbounded::<()>();
    global_hotkey::GlobalHotKeyEvent::set_event_handler(Some(move |event: global_hotkey::GlobalHotKeyEvent| {
        if event.state() == global_hotkey::HotKeyState::Pressed {
            let _ = tx.try_send(());
        }
    }));
    cx.spawn(async move |this, cx| {
        while rx.recv().await.is_ok() {
            let Ok(on_press) = this.read_with(cx, |s, _| s.on_press.clone()) else {
                return;
            };
            cx.update(|cx| on_press(cx));
        }
    })
    .detach();
}

#[cfg(target_os = "linux")]
mod portal {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_lite::Stream;
    pub use futures_lite::StreamExt;
    use std::cell::RefCell;
    use std::pin::Pin;
    use std::rc::Rc;

    use crate::app::APP_ID;

    const ID: &str = "launcher";

    pub type Session = ashpd::desktop::Session<GlobalShortcuts>;

    /// Where a session is kept from the moment it exists, so a binding given up while the
    /// desktop still asks (a dialog open) is closed too.
    pub type Kept = Rc<RefCell<Option<Rc<Session>>>>;

    /// The presses of a bound shortcut.
    pub type Presses = Pin<Box<dyn Stream<Item = ()>>>;

    /// Binds the Launcher's shortcut, preferring `trigger`, keeping its session in `kept`.
    /// Returns its presses and the trigger the desktop chose, or why none was bound.
    pub async fn bind(trigger: &str, kept: &Kept) -> Result<(Presses, String), String> {
        // A connection of its own: the portal ties an app id to a connection once, and GPUI's
        // connection may have talked to the portal already.
        let connection = ashpd::zbus::Connection::session()
            .await
            .map_err(|e| format!("The desktop offers no global shortcuts ({e})"))?;
        let app_id = ashpd::AppID::try_from(APP_ID).expect("APP_ID is a valid app id");
        // An older portal without the host registry finds the app another way, so a failure
        // here is only told if the shortcut is then refused: the portal registers the id only
        // when it finds the app's desktop file (`<APP_ID>.desktop`, installed by the packages).
        let unregistered = ashpd::register_host_app_with_connection(connection.clone(), app_id)
            .await
            .err()
            .map(|e| format!("; it did not register the app ({e}), which needs {APP_ID}.desktop installed"))
            .unwrap_or_default();
        let refused = |e: ashpd::Error| format!("The desktop refused a global shortcut ({e}){unregistered}");
        let portal = GlobalShortcuts::with_connection(connection)
            .await
            .map_err(|e| format!("The desktop offers no global shortcuts ({e})"))?;
        let session = Rc::new(portal.create_session(Default::default()).await.map_err(refused)?);
        *kept.borrow_mut() = Some(session.clone());
        let shortcut = NewShortcut::new(ID, "Open the View Everything Launcher").preferred_trigger(Some(trigger));
        let bound = portal
            .bind_shortcuts(&session, &[shortcut], None, Default::default())
            .await
            .and_then(|request| request.response())
            .map_err(|e| match e {
                ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => "Declined in the desktop's dialog".to_owned(),
                e => refused(e),
            })?;
        let trigger = bound
            .shortcuts()
            .iter()
            .find(|s| s.id() == ID)
            .map(|s| s.trigger_description().to_owned())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| "The desktop bound no key to it; set one in its shortcut settings".to_owned())?;
        let presses = portal
            .receive_activated()
            .await
            .map_err(refused)?
            .filter(|a| a.shortcut_id() == ID)
            .map(|_| ());
        Ok((Box::pin(presses), trigger))
    }
}

#[cfg(target_os = "linux")]
use portal::StreamExt as _;

#[cfg(test)]
mod tests {
    use super::Keys;

    #[test]
    fn shortcut_text_reads_in_any_case_and_shows_in_one_spelling() {
        let keys = Keys::parse(" alt + space ").unwrap();
        assert_eq!(keys.to_string(), "Alt+Space");
        assert_eq!(keys.portal_trigger(), "ALT+space");
        let keys = Keys::parse("Shift+CONTROL+k").unwrap();
        assert_eq!(keys.to_string(), "Ctrl+Shift+K");
        assert_eq!(keys.portal_trigger(), "CTRL+SHIFT+k");
        assert_eq!(Keys::parse("F12").unwrap().to_string(), "F12");
        assert_eq!(Keys::parse("super+return").unwrap().to_string(), "Super+Enter");
    }

    #[test]
    fn shortcut_text_without_one_usable_key_is_refused() {
        // A bare letter would be taken from typing everywhere.
        assert!(Keys::parse("k").is_err());
        assert!(Keys::parse("Alt").is_err());
        assert!(Keys::parse("Alt+K+L").is_err());
        assert!(Keys::parse("Alt+F25").is_err());
        assert!(Keys::parse("Alt+Escape").is_err());
        assert!(Keys::parse("Alt+").is_err());
    }
}
