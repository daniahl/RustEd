//! Application entry point for RustEd.
//!
//! Configures native window options and launches the eframe / egui event loop
//! with the top-level `TextEditor` application state.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use rusted::TextEditor;
use std::sync::Arc;

fn load_app_icon() -> Option<Arc<egui::IconData>> {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .ok()
        .map(Arc::new)
}

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([640.0, 400.0]);
    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "RustEd",
        options,
        Box::new(|cc| {
            #[cfg(windows)]
            {
                use raw_window_handle::HasWindowHandle as _;
                if let Ok(handle) = cc.window_handle() {
                    rusted::platform::suppress_alt_menu_mode(handle.as_raw());
                }
            }
            Ok(Box::new(TextEditor::new(cc.egui_ctx.clone())))
        }),
    )
}
