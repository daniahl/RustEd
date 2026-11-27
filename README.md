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
- **Syntax Highlighting & Virtualized Scrolling:** Automatic language detection and syntax highlighting via `egui_extras` + `syntect` for files up to 64 KB, seamlessly switching to windowed virtualized layout with viewport-pinned vertical and horizontal scrollbars on larger files (up to 64 MB, with binary/UTF-8 guards) to keep typing latency minimal.
- **Code Folding (`Alt+Left` / `Alt+Right` / `Alt+Shift+Left` / `Alt+Shift+Right`):** Collapse and expand code blocks by matching `{ }` braces and indentation levels via clickable gutter chevrons, inline `...` badges, or keyboard shortcuts—`Alt+Left` to fold the current/enclosing block (repeat to fold outward), `Alt+Right` to unfold, `Alt+Shift+Left` to fold all, and `Alt+Shift+Right` to unfold all. Search matches (`Ctrl+F` / `F3`) automatically unfold containing blocks, Undo/Redo preserves shifted folds, and true document line numbers remain visible in the gutter.
- **Find & Find Next / Previous (`Ctrl+F` / `F3` / `Shift+F3`):** Open the floating **Find** dialog via the top-bar **Find** button or `Ctrl+F` (pre-populated with the current single-line selection). Press `Enter` (`Shift+Enter` for previous) or click **Find** / **Prev** to close the dialog, jump to, and select the match in the editor. Press `F3` / `Shift+F3` at any time to cycle forward or backward through matches (wrapping around the document, with streaming case-sensitive and Unicode case-insensitive search).
- **Multi-File Unsaved State, Atomic Saves & Conflict Protection:** Switch freely between files in the project tree without losing unsaved edits, cursor positions, selections, fold states, or undo/redo history. Modified files are marked with an asterisk (`*`) in the file tree, **Save** (`Ctrl+S`) writes atomically (preserving Windows file attributes via `ReplaceFileW`), **Save As** (`Ctrl+Shift+S`) saves to a new path, and conflict banners / close confirmation dialogs guard against overwriting external disk edits or discarding unsaved changes.
- **Undo / Redo & Clipboard Support:** Full `Ctrl+Z` / `Ctrl+Y` (`Ctrl+Shift+Z`) history backed by copy-on-write `PieceTableSnapshot`s (and non-destructive `FullText` checkpoints across disk reloads), multi-line `Tab` / `Shift+Tab` indentation, line-ending-normalized `Ctrl+V` paste, plus `Ctrl+C`, `Ctrl+X`, word-wise deletion (`Ctrl+Backspace`, `Ctrl+Delete`), and `PageUp` / `PageDown` navigation.

### Integrated ConPTY Terminal for AI CLI Workflows
- **AI Explain (`Alt+E`):** Select code in the editor and click **AI Explain** (or press `Alt+E`) to switch focus to the integrated terminal, paste `Explain {filename}:{from_line}-{to_line}: {selected_text}`, and press `Enter` directly into your active AI CLI shell (e.g., `agy`).
- **Built-in Terminal Tab:** Spawns a native shell (`cmd.exe` via `portable-pty`) parsed through a `vt100` virtual screen with an incrementally synced 2,000-line scrollback buffer and dynamic post-scrollbar viewport resizing, making it easy to run CLI-based AI agents, build tools, and Git commands right next to your code.
- **Keyboard & Mouse Selection:** Supports xterm-modified navigation keys (`Ctrl+Arrow`, `Shift+Arrow`, `Shift+Tab`, `Shift+Enter`, `Alt+Enter`), `Alt` word/meta bindings (`Alt+B`, `Alt+F`, `Alt+Backspace`), `DECCKM` application cursor mode, bracketed paste, standard control sequences (`Ctrl+C`, `Ctrl+X`, `Ctrl+D`, `Ctrl+Z`, `Ctrl+L`, `Home`/`End`, `PageUp`/`PageDown`, `F1`–`F12`), and mouse-drag selection (`Ctrl+C` or right-click to copy without moving the PTY caret).

### Workspace File Explorer & Status Bar
- **Lazy Directory Tree (`Ctrl+Shift+E`):** Expandable sidebar folder tree sorted case-insensitively with directories first, quick parent-folder (`..`) navigation, and full keyboard-only navigation (`Ctrl+Shift+E` to toggle focus, `Up`/`Down`/`Left`/`Right`/`Enter`/`F2`/`Delete`/`Escape`).
- **Context Menu & Inline File Management:** Right-click folders or files in the explorer tree to **Create New Folder**, **Create New File**, **Rename** (also triggered with `F2`), or **Delete** (moves items to the Windows Recycle Bin). Creating or renaming uses an inline text input right inside the tree (`Enter` to commit, `Escape` to cancel) and automatically updates open and unsaved file paths when files or parent folders are renamed.
- **Live Status Bar:** Displays active file name, line count, byte size, cursor line/column, character selection count, indentation mode, encoding (`UTF-8`), line-ending sequence (`LF` / `CRLF`), and detected language.

---

## Keyboard Shortcuts

### Global & Workspace
| Shortcut | Action |
| --- | --- |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Switch between **Editor** and **Terminal** tabs |
| `Ctrl+Shift+E` | Toggle keyboard focus between **File Explorer** sidebar and **Editor** |
| `Ctrl+S` | Save active file (prompts **Save As** if `Untitled` or parent folder is missing) |
| `Ctrl+Shift+S` | **Save As...** (save active buffer to a new path) |
| `Ctrl+F` | Open **Find** dialog (seeded with current single-line selection) |
| `F3` / `Shift+F3` | Find Next / Find Previous match (wraps around document) |
| `Alt+E` | **AI Explain** selected code in the integrated terminal |

### Code Editor
| Shortcut | Action |
| --- | --- |
| `Left` / `Right` / `Up` / `Down` | Move caret (skips collapsed folds and `CRLF` pairs) |
| `Ctrl+Left` / `Ctrl+Right` | Move caret by word |
| `Home` / `End` | Move caret to start / end of line |
| `Ctrl+Home` / `Ctrl+End` | Move caret to start / end of document |
| `PageUp` / `PageDown` | Move caret up / down by 25 rows |
| `Shift` + Navigation Keys | Extend text selection |
| `Ctrl+A` | Select entire document (including folded regions) |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / Cut / Paste (normalizes pasted line endings to `LF` or `CRLF`) |
| `Ctrl+Z` | Undo |
| `Ctrl+Y` / `Ctrl+Shift+Z` | Redo |
| `Tab` / `Shift+Tab` | Indent (4 spaces) / Outdent current line or multi-line selection |
| `Enter` | Insert line break preserving leading indentation |
| `Backspace` / `Delete` | Delete previous / next character or active selection |
| `Ctrl+Backspace` / `Ctrl+Delete` | Delete previous / next word |

### Code Folding & Find Dialog
| Shortcut | Action |
| --- | --- |
| `Alt+Left` | Fold block at cursor or innermost enclosing unfolded block (repeat to fold outward) |
| `Alt+Right` | Unfold block at cursor or innermost enclosing folded block |
| `Alt+Shift+Left` | Fold all foldable blocks in the document |
| `Alt+Shift+Right` | Unfold all blocks in the document |
| `Enter` / `Shift+Enter` *(in Find)* | Jump to next / previous match, close Find dialog, and focus editor |
| `Escape` *(in Find)* | Close Find dialog and return focus to editor |

### File Explorer Sidebar
| Shortcut | Action |
| --- | --- |
| `Up` / `Down` | Move selection through visible folders and files |
| `Right` | Expand selected folder, or move to its first child when already expanded |
| `Left` | Collapse selected folder, or select its parent folder |
| `Enter` | Toggle folder expansion, or open selected file and focus editor |
| `F2` | Rename selected file or folder inline (`Enter` to commit, `Escape` to cancel) |
| `Delete` | Move selected item to Recycle Bin / Trash (prompts if unsaved edits exist) |
| `Escape` | Return keyboard focus to the code editor (or cancel active inline edit) |

### Integrated Terminal
| Shortcut | Action |
| --- | --- |
| `Ctrl+C` | Copy selected text when a mouse selection is active; otherwise send `^C` (`SIGINT`) |
| `Right-Click` | Copy active mouse selection and clear selection without moving PTY caret |
| `Ctrl+V` / `Shift+Insert` | Paste clipboard (uses bracketed paste when active; normalizes `CRLF` to `LF`) |
| `Ctrl+X` | Send `^X` (`0x18`) |
| `Ctrl+D` / `Ctrl+Z` / `Ctrl+L` | Send standard `^D` (`EOF`), `^Z`, `^L`, and `Ctrl+A`–`Ctrl+Z` control bytes |
| `Shift+Enter` / `Alt+Enter` | Send modified newline (`\x1b[13;2u` / `\x1b\r`) for multiline CLI prompts |
| `Shift+Tab` | Send backtab (`\x1b[Z`) |
| `Alt+Backspace` / `Alt+A`–`Alt+Z` | Send Meta/word-editing sequences (`\x1b\x7f`, `\x1ba`–`\x1bz`) |
| `Shift+Delete` | Send xterm modified Delete (`\x1b[3;2~`) |
| `Arrows`, `Home`/`End`, `PgUp`/`PgDn`, `F1`–`F12` | Send VT100/xterm navigation & function sequences (with `Shift`/`Alt`/`Ctrl` & `DECCKM`) |

---

## Project Structure

- `src/main.rs` — Native window entry point (`eframe::run_native`) and command-line folder startup.
- `src/lib.rs` — Library root exporting editor modules (`TextEditor`, `PieceTable`, etc.) for integration tests.
- `src/app.rs` — Top-level application state (`TextEditor`), top bar, tab switching (`Editor` / `Terminal`), status bar, folder picker, and watcher synchronization.
- `src/document.rs` — Document management (`Document`), atomic file saves, disk conflict detection/reload, syntax-highlighted/virtualized `FastEditor` integration, line-number gutter, and Find dialog.
- `src/editor/` — Custom `FastEditor` widget (`mod.rs`), `EditorBuffer` trait (`buffer.rs`), windowed virtualization (`virtualized.rs`), and AccessKit `TextRun` generation (`accesskit.rs`).
- `src/piece_table.rs` — Rope/chunk-cached `PieceTable` buffer with line-feed checkpoints, CRLF-aware line indexing, zero-flatten incremental search (`find_from_byte`), and structural snapshots.
- `src/explorer.rs` — Lazy-loading `FileExplorer` tree with case-insensitive sorting, inline create/rename, and Windows Recycle Bin deletion.
- `src/folding.rs` — Single-pass `{ }` brace and indentation-level code folding (`FoldState`, `FoldView`) with display-to-document coordinate projection.
- `src/terminal.rs` — Windows ConPTY `TerminalPane` with lazy startup, background I/O threads, `vt100` screen parsing, DSR cursor query replies (`process_chunk_with_dsr`), and clean PTY shutdown on drop/restart.
- `src/watcher.rs` — Windows `ReadDirectoryChangesW` filesystem watcher and Windows-aware path comparison helpers (`paths_equal`, `path_starts_with`, `path_strip_prefix`).
- `tests/cases/` — Unit and headless GUI integration tests (`app_integration.rs`, `document_tests.rs`, `editor_buffer_tests.rs`, `editor_integration.rs`, `explorer_tests.rs`, `folding_tests.rs`, `piece_table_tests.rs`, `terminal_tests.rs`, `watcher_tests.rs`).

---

## Getting Started

### Prerequisites
- [Rust toolchain](https://rustup.rs/) (Rust 2021 edition)

### Build and Run
```powershell
cargo run --release
# Optional: open a project folder and/or initial file on startup
cargo run --release -- path\to\project path\to\project\src\main.rs
```
*(Note: `Cargo.toml` sets `opt-level = 3` for all dependencies even in `dev` builds so text layout and syntax highlighting remain fast during development.)*

### Run Tests
```powershell
cargo test
```
