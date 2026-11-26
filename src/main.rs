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
    /// Default window size (width, height) in logical points.
    const DEFAULT_WINDOW_SIZE: [f32; 2] = [1200.0, 800.0];
    /// Minimum window size (width, height) in logical points.
    const MIN_WINDOW_SIZE: [f32; 2] = [640.0, 400.0];

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(DEFAULT_WINDOW_SIZE)
        .with_min_inner_size(MIN_WINDOW_SIZE);
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
                    rusted::platform::install_window_hooks(handle.as_raw());
                }
            }
            Ok(Box::new(TextEditor::new(cc.egui_ctx.clone())))
        }),
    )
}
