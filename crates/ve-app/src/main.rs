// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
#[cfg(feature = "bench")]
mod bench;
mod categories;
mod dashboard;
mod dashboard_summary;
mod engine;
mod explorer;
mod filters;
mod format;
mod scope;
mod selection;
mod settings;
mod settings_page;
mod theme;
mod trash;
mod tree;
mod treemap;
mod ui;
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

    gpui_kit::application().with_assets(theme::Assets).run(move |cx| {
        gpui_kit::init(cx);
        theme::init(settings.theme, cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx))),
            window_min_size: Some(size(px(720.), px(480.))),
            titlebar: Some(TitlebarOptions {
                title: Some("view-everything".into()),
                ..Default::default()
            }),
            app_id: Some("view-everything".into()),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| app::Shell::new(settings, categories, volumes, problems, window, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
