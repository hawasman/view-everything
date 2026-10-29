// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod autostart;
#[cfg(feature = "bench")]
mod bench;
mod categories;
mod cleaner;
mod compare;
mod dashboard;
mod dashboard_summary;
mod duplicates;
mod engine;
mod entry_actions;
mod explorer;
mod export;
mod filters;
mod format;
mod host;
mod locations;
mod motion;
mod scope;
mod selection;
mod selection_panel;
mod settings;
mod settings_page;
mod tasks;
mod theme;
mod trash;
mod tray;
mod tray_icon;
mod tree;
mod treemap;
mod ui;
#[cfg(feature = "updater")]
mod update;
mod view;

use gpui_kit::*;
use settings::Settings;

/// Skip asking for administrator rights on Windows (the Volume is then walked, not read from
/// its Master File Table).
#[cfg(windows)]
const NO_ELEVATE: &str = "--no-elevate";

fn main() {
    #[cfg(windows)]
    if !std::env::args().any(|a| a == NO_ELEVATE) && !ve_scan::is_elevated() {
        // Ok(true): an elevated copy is starting and takes over. Declined or failed: carry on
        // without elevation and walk the folders instead.
        if let Ok(true) = ve_scan::relaunch_elevated() {
            return;
        }
    }

    let mut problems = Vec::new();
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
            host::Host::start(&settings, categories, volumes, problems, login, cx);
        });
}
