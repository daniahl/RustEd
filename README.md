# RustEd

**RustEd - the fast, simple and accessible code editor for an AI-driven CLI-based workflow.**

RustEd is a lightweight native desktop code editor and integrated terminal written in Rust using [`eframe`](https://github.com/emilk/egui/tree/master/crates/eframe) / [`egui`](https://github.com/emilk/egui), [`syntect`](https://github.com/trishume/syntect), [`portable-pty`](https://github.com/wez/wezterm/tree/main/pty), and [`vt100`](https://docs.rs/vt100). It is designed to pair seamlessly with CLI-based AI coding assistants by combining a low-latency text editor, a project file explorer, and an embedded ConPTY terminal in a single keyboard- and assistive-technology-friendly workspace.

---

## Key Features

### First-Class Accessibility
RustEd is built from the ground up with assistive technologies—especially **Windows Magnifier** and screen readers—in mind via **AccessKit**:
- **Reliable Caret & Magnifier Tracking:** Both the code editor and the integrated terminal expose custom `MultilineTextInput` and active-row `TextRun` nodes with precise per-glyph bounding boxes (`character_positions`, `character_widths`, `character_lengths`).
- **Leading & Trailing Row Anchors:** Active text runs are padded with 1px leading and trailing anchors so `accesskit_consumer` never misinterprets column `0` (`is_run_start`) or empty trailing lines (`is_document_end`), preventing erratic caret jumps across line breaks.
- **Strict Viewport Clamping & Instant Scroll-to-Caret:** Row bounds are clamped to the visible scroll area, and edits or large pastes that move the caret off-screen trigger an immediate same-frame layout pass (`request_discard`) while clearing stale off-screen selections so screen magnifiers never jump outside the window.

### Fast, Low-Latency Code Editor (`FastEditor`)
- **Revision-Cached Text Layout:** Reuses `Galley` layouts in $O(1)$ across unchanged frames and coalesces multiple input events within a single frame into one layout pass.
- **Viewport-Culled Selection & Gutter Rendering:** Binary-searches visible rows (`partition_point`) for both text selection highlights and line-number gutter rendering without cloning the underlying `Galley` or allocating per-line strings.
- **Syntax Highlighting:** Automatic language detection and syntax highlighting via `egui_extras` + `syntect` for files up to 16 KB, seamlessly falling back to plain monospace layout on larger files to keep typing latency minimal.
- **Find & Find Next (`Ctrl+F` / `F3`):** Open the floating **Find** dialog via the top-bar **Find** button or `Ctrl+F` (pre-populated with the current single-line selection). Press `Enter` or click **Find** to close the dialog, jump to, and select the match in the editor. Press `F3` at any time to cycle through subsequent matches (wrapping around the document).
- **Multi-File Unsaved State Tracking:** Switch freely between files in the project tree without losing unsaved edits, cursor positions, selections, or undo/redo history. Modified files are marked with an asterisk (`*`) in the file tree, and the **Save** button dynamically enables only when the active file has unsaved changes.
- **Undo / Redo & Clipboard Support:** Full `Ctrl+Z` / `Ctrl+Y` history backed by shared `Arc<String>` snapshots, plus `Ctrl+C`, `Ctrl+X`, `Ctrl+V`, word-wise deletion (`Ctrl+Backspace`, `Ctrl+Delete`), and `PageUp` / `PageDown` navigation.

### Integrated ConPTY Terminal for AI CLI Workflows
- **Built-in Terminal Tab:** Spawns a native shell (`cmd.exe` via `portable-pty`) parsed through a `vt100` virtual screen (`120x30`), making it easy to run CLI-based AI agents, build tools, and Git commands right next to your code.
- **Keyboard & Selection Mode:** Supports standard terminal control sequences (`Ctrl+C`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+L`, arrow keys, `Home`/`End`, `PageUp`/`PageDown`), plus `Shift`+Arrow selection mode for copying terminal output.

### Workspace File Explorer & Status Bar
- **Lazy Directory Tree:** Expandable sidebar folder tree sorted case-insensitively with directories first and quick parent-folder (`..`) navigation.
- **Context Menu & Inline File Management:** Right-click folders or files in the explorer tree to **Create New Folder**, **Create New File**, **Rename** (also triggered with `F2`), or **Delete** (moves items to the Windows Recycle Bin). Creating or renaming uses an inline text input right inside the tree (`Enter` to commit, `Escape` to cancel) and automatically updates open and unsaved file paths when files or parent folders are renamed.
- **Live Status Bar:** Displays active file name, line count, byte size, cursor line/column, character selection count, indentation mode, encoding (`UTF-8`), line-ending sequence (`LF` / `CRLF`), and detected language.

---

## Project Structure

- `src/main.rs` — Application state (`TextEditor`), top bar, status bar, file explorer (`FileNode`), ConPTY terminal management, and terminal AccessKit caret synchronization.
- `src/editor.rs` — Custom `FastEditor` widget implementing revision-cached layout, coalesced event handling, viewport-culled selection painting, undo/redo management, and AccessKit `TextRun` generation.
- `tests/editor_integration.rs` — Headless integration tests covering the editor lifecycle, undo/redo, arrow-key navigation, scroll-to-caret on large pastes, coalesced relayouts, and AccessKit node invariants.

---

## Getting Started

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (Rust 2021 edition)

### Build and Run
```powershell
cargo run --release
```
*(Note: `Cargo.toml` sets `opt-level = 3` for all dependencies even in `dev` builds so text layout and syntax highlighting remain fast during development.)*

### Run Tests
```powershell
cargo test
```
