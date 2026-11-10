//! RustEd core library.
//!
//! Exports the primary application state and supporting submodules:
//! - `app`: Main application coordinator and eframe integration.
//! - `document`: Document state, buffer management, syntax highlighting, line numbers, and Find dialog.
//! - `editor`: High-performance custom text editor widget with full AccessKit accessibility support.
//! - `explorer`: File tree sidebar, inline filesystem operations (create/rename), and recycle bin support.
//! - `terminal`: Embedded ConPTY terminal pane with VT100 emulation.

pub mod app;
pub mod document;
pub mod editor;
pub mod explorer;
pub mod piece_table;
pub mod terminal;

pub use app::TextEditor;
pub use piece_table::PieceTable;
