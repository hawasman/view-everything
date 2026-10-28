// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod engine;
mod filters;
mod format;
mod settings;
mod theme;
mod trash;
mod treemap;
mod view;

use settings::Settings;

/// Skip asking for administrator rights on Windows (the Volume is then walked, not read from
/// its Master File Table).
#[cfg(windows)]
const NO_ELEVATE: &str = "--no-elevate";

fn main() -> eframe::Result {
    #[cfg(windows)]
    if !std::env::args().any(|a| a == NO_ELEVATE) && !ve_scan::is_elevated() {
        // Ok(true): an elevated copy is starting and takes over. Declined or failed: carry on
        // without elevation and walk the folders instead.
        if let Ok(true) = ve_scan::relaunch_elevated() {
            return Ok(());
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
    let volumes = ve_scan::discover_volumes();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("view-everything")
            .with_app_id("view-everything")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "view-everything",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx, settings.theme.into());
            Ok(Box::new(app::App::new(
                &cc.egui_ctx,
                settings,
                categories,
                volumes,
                problems,
            )))
        }),
    )
}
