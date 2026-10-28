// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// The panels and Engine commands the egui build had return in later issues of the GPUI port.
#![allow(dead_code)]

mod app;
mod bench;
mod engine;
mod explorer;
mod filters;
mod format;
mod settings;
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
    let mut volumes = ve_scan::discover_volumes();
    // Measuring: index only the named Volume (see examples/gen_index.rs) instead of this machine's.
    if let Some(mount) = std::env::var_os("VE_BENCH_MOUNT") {
        volumes = vec![ve_scan::VolumeInfo {
            mount: mount.into(),
            fs_type: "ext4".into(),
            label: "bench".into(),
            removable: false,
            read_only: true,
        }];
    }

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            app::init_theme(settings.theme, cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(820.)),
                    cx,
                ))),
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
