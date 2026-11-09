//! Application entry point for RustEd.
//!
//! Configures native window options and launches the eframe / egui event loop
//! with the top-level `TextEditor` application state.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use rusted::TextEditor;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RustEd",
        options,
        Box::new(|cc| Ok(Box::new(TextEditor::new(cc.egui_ctx.clone())))),
    )
}
