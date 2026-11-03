// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agent_notice;
mod app;
mod apps;
mod autostart;
#[cfg(feature = "bench")]
mod bench;
mod categories;
mod cleaner;
mod compare;
mod dashboard;
mod dashboard_summary;
mod duplicates;
mod entry_actions;
mod explorer;
mod export;
mod filters;
mod format;
mod host;
mod inspector;
mod launcher;
mod locations;
mod motion;
mod network;
mod performance;
mod scope;
mod selection;
mod settings;
mod settings_page;
mod tasks;
mod theme;
mod trace;
mod trash;
mod tray;
mod tray_icon;
mod tree;
mod treemap;
mod ui;
#[cfg(feature = "updater")]
mod update;

use gpui_kit::*;
use settings::Settings;

/// Skip asking for administrator rights on Windows (the Volume is then walked, not read from
/// its Master File Table).
#[cfg(windows)]
const NO_ELEVATE: &str = "--no-elevate";

/// Opens the running app's Launcher and exits; with no app running, a Launcher that says so.
/// A command to bind to a key where the desktop refuses the app's own global shortcut.
const LAUNCHER: &str = "--launcher";

fn main() {
    if std::env::args().any(|a| a == LAUNCHER) {
        return launcher_only();
    }
    #[cfg(windows)]
    if !std::env::args().any(|a| a == NO_ELEVATE) && !ve_scan::is_elevated() {
        // Ok(true): an elevated copy is starting and takes over. Declined or failed: carry on
        // without elevation and walk the folders instead.
        if let Ok(true) = ve_scan::relaunch_elevated() {
            return;
        }
    }

    let mut problems = Vec::new();
    let socket = single_instance(&mut problems);
    let settings = Settings::load().unwrap_or_else(|e| {
        problems.push(format!("Settings ignored: {e}"));
        Settings::default()
    });
    let categories = settings::load_categories().unwrap_or_else(|e| {
        problems.push(format!("Category overrides ignored: {e}"));
        ve_core::CategoryMap::builtin()
    });
    #[cfg_attr(not(feature = "bench"), allow(unused_mut))]
    let mut volumes = ve_scan::discover_volumes();
    // Measuring: index only the named Volume (see examples/gen_index.rs) instead of this machine's.
    #[cfg(feature = "bench")]
    if let Some(mount) = std::env::var_os("VE_BENCH_MOUNT") {
        volumes = vec![ve_scan::VolumeInfo {
            mount: mount.into(),
            identity: "bench".into(),
            fs_type: "ext4".into(),
            label: "bench".into(),
            removable: false,
            network: false,
            read_only: true,
        }];
    }

    // Screenshots: show only the Volumes named in VE_VOLUMES (comma-separated mount points).
    #[cfg(feature = "bench")]
    if let Ok(list) = std::env::var("VE_VOLUMES") {
        volumes.retain(|v| list.split(',').any(|m| v.mount == std::path::Path::new(m)));
    }

    // Screenshots: VE_FAKE_SHARE=/mnt/nas adds a network share that is not really there.
    #[cfg(feature = "bench")]
    if let Some(mount) = std::env::var_os("VE_FAKE_SHARE") {
        volumes.push(ve_scan::VolumeInfo {
            mount: mount.into(),
            identity: "nfs:nas.local:/export".into(),
            fs_type: "nfs4".into(),
            label: "nas.local:/export".into(),
            removable: false,
            network: true,
            read_only: false,
        });
    }

    let login = autostart::started_at_login();
    gpui_kit::application()
        .with_assets(theme::Assets)
        // The app outlives its windows while it sits in the tray; Host decides when to quit.
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::init(settings.theme, cx);
            motion::init(settings.reduce_motion, cx);
            host::Host::start(&settings, categories, volumes, problems, login, socket, cx);
        });
}

/// `--launcher`: asks the running app to open its Launcher. With none running (nothing answers
/// on the engine socket), opens a Launcher of its own that says so, and exits when it closes.
fn launcher_only() {
    use ve_engine::socket::{self, Client};
    if let Some(path) = socket::path() {
        match Client::connect(&path).and_then(|mut app| app.call("launcher", Default::default())) {
            Ok(_) => return,
            Err(e) if e.is_not_running() => {}
            // Something answers (another version, a hung app): not a case for "not running".
            Err(e) => {
                eprintln!("View Everything is running but did not open its Launcher: {e}");
                std::process::exit(1)
            }
        }
    }
    let settings = Settings::load().unwrap_or_default();
    gpui_kit::application()
        .with_assets(theme::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::init(settings.theme, cx);
            motion::init(settings.reduce_motion, cx);
            launcher::show(cx);
        });
}

/// The single-instance rule (ADR-0016): the first launch binds the engine socket and hosts the
/// Engine; a later one asks it to show its window and exits here (a login start just exits).
/// A socket that cannot be bound is a problem to show, not a reason not to start.
fn single_instance(problems: &mut Vec<String>) -> Option<ve_engine::socket::Listener> {
    use ve_engine::socket::{self, Claim};
    let path = socket::path()?;
    match socket::claim(&path) {
        Ok(Claim::Host(listener)) => Some(listener),
        Ok(Claim::Running(_)) if autostart::started_at_login() => std::process::exit(0),
        Ok(Claim::Running(mut app)) => match app.call("show", Default::default()) {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                eprintln!("View Everything is already running but did not show its window: {e}");
                std::process::exit(1)
            }
        },
        Ok(Claim::Incompatible(reason)) => {
            eprintln!("Another version of View Everything is running ({reason}); quit it first.");
            std::process::exit(1)
        }
        Err(e) => {
            problems.push(format!(
                "The engine socket {} could not be opened ({e}): `ve` reads the saved Index instead, and launching the app again starts a second copy",
                path.display()
            ));
            None
        }
    }
}
