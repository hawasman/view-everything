//! The app-level owner of the Engine (ADR-0012). The window is one view of it and the tray is
//! another: the window can close while indexing, Live updates and Tasks go on, and a window
//! opened later binds to the same Engine. The Engine itself knows nothing of GPUI.

use crate::engine::{Engine, Repaint};
use crate::settings::Settings;
use crate::tray::{Tray, TrayAction, TrayState};
use async_channel::{Receiver, Sender};
use gpui_kit::*;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use ve_core::CategoryMap;
use ve_scan::VolumeInfo;

/// How often the tray menu catches up with the running Tasks.
const TRAY_REFRESH: Duration = Duration::from_secs(1);

/// The window's side of two wake-up lines. Attaching a new window replaces the sender, which
/// ends the previous window's loops.
#[derive(Default)]
struct Wires {
    /// "Something the window shows changed" (the Engine's repaint closure).
    wake: Option<Sender<()>>,
    /// Volumes mounted or unmounted while a window is open.
    mounts: Option<Sender<Vec<VolumeInfo>>>,
}

pub struct Host {
    pub engine: Arc<Engine>,
    pub categories: CategoryMap,
    /// The Volumes mounted now.
    pub volumes: Vec<VolumeInfo>,
    pub repaint: Repaint,
    wires: Arc<Mutex<Wires>>,
    tray: Option<Tray>,
    keep_in_tray: bool,
    /// Live updates were not started because this is a login start (Linux: the helper asks
    /// for a password).
    live_paused: bool,
    /// `quit` has run: its window removal re-enters it through the window-closed hook.
    quitting: bool,
    /// Settings problems found at startup, shown by the first window.
    problems: Vec<String>,
}

impl Global for Host {}

/// What a new window needs from the host.
pub struct Attached {
    pub wake: Receiver<()>,
    pub mounts: Receiver<Vec<VolumeInfo>>,
    pub problems: Vec<String>,
}

impl Host {
    /// Starts the Engine, the tray and the loops that serve them, and registers the host. Opens
    /// the first window unless this is a login start with a tray to wait in.
    pub fn start(settings: &Settings, categories: CategoryMap, volumes: Vec<VolumeInfo>, problems: Vec<String>, login: bool, cx: &mut App) {
        let wires = Arc::new(Mutex::new(Wires::default()));
        let repaint: Repaint = {
            let wires = wires.clone();
            Arc::new(move || {
                // One pending wake-up is enough: the window then reads everything that changed.
                if let Some(wake) = &wires.lock().wake {
                    let _ = wake.try_send(());
                }
            })
        };
        let indexed: Vec<VolumeInfo> = volumes.iter().filter(|v| settings.is_indexed(v)).cloned().collect();
        let paused = login && cfg!(target_os = "linux") && settings.live_helper;
        let engine = Engine::start(
            repaint.clone(),
            volumes.clone(),
            indexed.clone(),
            settings.remotes.clone(),
            settings.intervals(&indexed),
            categories.clone(),
            settings.live_helper && !paused,
        );
        engine.set_duplicate_exclusions(settings.duplicate_exclusions.clone());
        engine.set_remote_names(&settings.remote_names);
        let engine = Arc::new(engine);

        let (actions, chosen) = async_channel::unbounded::<TrayAction>();
        let tray = Tray::start(
            actions,
            TrayState {
                tasks: Vec::new(),
                live_paused: paused,
            },
        );
        let has_tray = tray.is_some();
        cx.set_global(Host {
            engine,
            categories,
            volumes,
            repaint,
            wires,
            tray,
            keep_in_tray: settings.keep_in_tray,
            live_paused: paused,
            quitting: false,
            problems,
        });

        // Quitting by any path (tray, update, the last window closing) saves the Index.
        cx.on_app_quit(|cx| {
            let host = cx.global_mut::<Host>();
            host.tray = None;
            host.engine.shutdown();
            async {}
        })
        .detach();
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() && !cx.global::<Host>().stays_in_tray() {
                Host::quit(cx);
            }
        })
        .detach();

        cx.spawn(async move |cx| {
            while let Ok(action) = chosen.recv().await {
                cx.update(|cx| Host::act(action, cx));
            }
        })
        .detach();
        if has_tray {
            cx.spawn(async move |cx| {
                let mut shown = TrayState {
                    tasks: Vec::new(),
                    live_paused: paused,
                };
                loop {
                    cx.background_executor().timer(TRAY_REFRESH).await;
                    cx.update(|cx| {
                        let host = cx.global::<Host>();
                        let now = TrayState {
                            tasks: host.engine.shared.tasks.running().into_iter().map(|t| t.label).collect(),
                            live_paused: host.live_paused,
                        };
                        if now != shown {
                            if let Some(tray) = &host.tray {
                                tray.update(now.clone());
                            }
                            shown = now;
                        }
                    });
                }
            })
            .detach();
        }
        Self::watch_mounts(cx);

        match (login, has_tray) {
            // A login start waits in the tray until the user opens the window.
            (true, true) => {}
            // No tray to wait in: the window opens normally but the app is not brought to the
            // front. It is never minimized, as nothing could be sure to restore it.
            (true, false) => {
                Self::open_window(cx);
            }
            (false, _) => {
                Self::open_window(cx);
                cx.activate(true);
            }
        }
    }

    /// Quits with the window and tray icon gone first: saving the Index can take seconds, and
    /// an app that looks frozen meanwhile is worse than one that has already vanished.
    pub fn quit(cx: &mut App) {
        if std::mem::replace(&mut cx.global_mut::<Host>().quitting, true) {
            return;
        }
        for window in cx.windows() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
        cx.global_mut::<Host>().tray = None;
        // Let the window removal reach the screen before the save blocks this thread.
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    /// Whether closing the last window leaves the app running.
    fn stays_in_tray(&self) -> bool {
        self.keep_in_tray && self.tray.is_some()
    }

    /// Whether a tray icon is showing.
    pub fn has_tray(&self) -> bool {
        self.tray.is_some()
    }

    pub fn set_keep_in_tray(&mut self, on: bool) {
        self.keep_in_tray = on;
    }

    /// Live updates are waiting for the user to start them (a login start).
    pub fn live_paused(&self) -> bool {
        self.live_paused
    }

    /// The user turned Live updates off (or on) in Settings: nothing is paused any more, and
    /// the setting decides.
    pub fn live_setting_changed(&mut self, on: bool) {
        // A paused login start is over either way. Turning Live updates on then starts them
        // (the window skipped the helper call while paused); off leaves nothing to stop.
        if std::mem::replace(&mut self.live_paused, false) && on {
            self.engine.set_live_helper(true);
        }
    }

    /// Starts the Live updates a login start held back.
    pub fn start_live(cx: &mut App) {
        let host = cx.global_mut::<Host>();
        if !std::mem::replace(&mut host.live_paused, false) {
            return;
        }
        host.engine.set_live_helper(true);
        (host.repaint)();
    }

    fn act(action: TrayAction, cx: &mut App) {
        match action {
            TrayAction::Open => {
                Self::open_window(cx);
                cx.activate(true);
            }
            TrayAction::SyncNow => cx.global::<Host>().engine.sync_now(),
            TrayAction::StartLive => Self::start_live(cx),
            TrayAction::Quit => Host::quit(cx),
        }
    }

    /// Shows the window: raises the open one or builds a new one bound to this host.
    pub fn open_window(cx: &mut App) -> Option<AnyWindowHandle> {
        if let Some(window) = cx.windows().into_iter().next() {
            let _ = window.update(cx, |_, window, _| window.activate_window());
            return Some(window);
        }
        // What the user changed while no window was open is in the file.
        let mut problems = std::mem::take(&mut cx.global_mut::<Host>().problems);
        let settings = Settings::load().unwrap_or_else(|e| {
            let problem = format!("Settings ignored: {e}");
            if !problems.contains(&problem) {
                problems.push(problem);
            }
            Settings::default()
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx))),
            window_min_size: Some(size(px(720.), px(480.))),
            titlebar: Some(TitlebarOptions {
                title: Some(crate::app::APP_NAME.into()),
                ..Default::default()
            }),
            app_id: Some("view-everything".into()),
            ..Default::default()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            let attached = cx.global::<Host>().attach(problems);
            cx.new(|cx| crate::app::Shell::new(settings, attached, window, cx))
        });
        match opened {
            Ok((window, _)) => Some(window),
            Err(e) => {
                eprintln!("Could not open the window: {e}");
                None
            }
        }
    }

    /// Connects a new window's wake-ups, replacing the previous window's.
    fn attach(&self, problems: Vec<String>) -> Attached {
        let (wake, woken) = async_channel::bounded::<()>(1);
        let (mounts, mounted) = async_channel::unbounded();
        {
            let mut wires = self.wires.lock();
            wires.wake = Some(wake);
            wires.mounts = Some(mounts);
        }
        Attached {
            wake: woken,
            mounts: mounted,
            problems,
        }
    }

    /// Follows Volumes mounting and unmounting for as long as the app runs. A window applies the
    /// change itself (offers, dialogs); with none open the Engine is told directly.
    fn watch_mounts(cx: &mut App) {
        #[cfg(feature = "bench")]
        if std::env::var_os("VE_BENCH_MOUNT").is_some() || std::env::var_os("VE_FAKE_SHARE").is_some() {
            return;
        }
        let (mounts, mounted) = async_channel::unbounded::<Vec<VolumeInfo>>();
        ve_scan::watch::watch(cx.global::<Host>().volumes.clone(), move |now| {
            let _ = mounts.send_blocking(now);
        });
        cx.spawn(async move |cx| {
            while let Ok(now) = mounted.recv().await {
                cx.update(|cx| {
                    let host = cx.global_mut::<Host>();
                    host.volumes = now.clone();
                    let window = host.wires.lock().mounts.as_ref().map(|m| m.try_send(now.clone()));
                    if matches!(window, Some(Ok(()))) {
                        return;
                    }
                    let settings = Settings::load().unwrap_or_default();
                    let indexed: Vec<VolumeInfo> = now.iter().filter(|v| settings.is_indexed(v)).cloned().collect();
                    host.engine.set_volumes(now, indexed.clone(), settings.remotes.clone());
                    host.engine.set_remote_names(&settings.remote_names);
                    host.engine.set_intervals(settings.intervals(&indexed));
                });
            }
        })
        .detach();
    }
}
