mod editor;

use editor::FastEditor;
use eframe::egui;
use egui_extras::syntax_highlighting::{self, CodeTheme};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

const TERM_ROWS: u16 = 30;
const TERM_COLS: u16 = 120;
const MAX_HIGHLIGHT_BYTES: usize = 16_384;

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

#[derive(PartialEq, Eq, Default, Clone, Copy)]
enum Tab {
    #[default]
    Editor,
    Terminal,
}

struct TerminalState {
    parser: vt100::Parser,
    pty_updated: bool,
}

struct FileNode {
    name: String,
    collapsed_label: String,
    expanded_label: String,
    path: PathBuf,
    is_dir: bool,
    expanded: bool,
    children_loaded: bool,
    children: Vec<FileNode>,
}

impl FileNode {
    fn new_root(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or("root"))
            .to_string();
        let collapsed_label = format!("▸ {name}");
        let expanded_label = format!("▾ {name}");
        let mut node = Self {
            name,
            collapsed_label,
            expanded_label,
            path,
            is_dir: true,
            expanded: true,
            children_loaded: false,
            children: Vec::new(),
        };
        node.load_children();
        node
    }

    fn load_children(&mut self) {
        self.children_loaded = true;
        self.children.clear();
        if let Ok(entries) = fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                let name = entry.file_name().to_string_lossy().into_owned();
                let (collapsed_label, expanded_label) = if is_dir {
                    (format!("▸ {name}"), format!("▾ {name}"))
                } else {
                    (String::new(), String::new())
                };
                self.children.push(Self {
                    name,
                    collapsed_label,
                    expanded_label,
                    path,
                    is_dir,
                    expanded: false,
                    children_loaded: false,
                    children: Vec::new(),
                });
            }
            // Folders first, then files sorted case-insensitively without per-comparison heap allocations
            self.children.sort_by(|a, b| {
                b.is_dir.cmp(&a.is_dir).then_with(|| {
                    a.name
                        .bytes()
                        .map(|c| c.to_ascii_lowercase())
                        .cmp(b.name.bytes().map(|c| c.to_ascii_lowercase()))
                        .then_with(|| a.name.cmp(&b.name))
                })
            });
        }
    }
}

fn render_file_node(
    ui: &mut egui::Ui,
    node: &mut FileNode,
    current_path: &Path,
    depth: usize,
    file_to_open: &mut Option<PathBuf>,
) {
    let indent = depth as f32 * 14.0;
    if node.is_dir {
        ui.horizontal(|ui| {
            ui.add_space(indent);
            let label = if node.expanded {
                node.expanded_label.as_str()
            } else {
                node.collapsed_label.as_str()
            };
            let resp = ui.selectable_label(false, label);
            if resp.clicked() {
                node.expanded = !node.expanded;
                if node.expanded && !node.children_loaded {
                    node.load_children();
                }
            }
        });
        if node.expanded {
            for child in &mut node.children {
                render_file_node(ui, child, current_path, depth + 1, file_to_open);
            }
        }
    } else {
        let is_selected = current_path == node.path;
        ui.horizontal(|ui| {
            ui.add_space(indent + 14.0);
            let resp = ui.selectable_label(is_selected, &node.name);
            if resp.clicked() {
                *file_to_open = Some(node.path.clone());
            }
        });
    }
}

fn hash_language_salt(lang: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(lang, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn format_u32_stack(mut n: u32, buf: &mut [u8; 16]) -> &str {
    if n == 0 {
        buf[15] = b'0';
        return std::str::from_utf8(&buf[15..]).unwrap();
    }
    let mut idx = 16;
    while n > 0 {
        idx -= 1;
        buf[idx] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    std::str::from_utf8(&buf[idx..]).unwrap()
}

struct TextEditor {
    tab: Tab,
    path: String,
    text: String,
    text_revision: u64,
    file_name: String,
    language_ext: String,
    language_name: &'static str,
    language_salt: u64,
    line_ending: &'static str,
    folder_path: String,
    tree_root: Option<FileNode>,
    term: Arc<Mutex<TerminalState>>,
    term_screen_text: String,
    term_cursor_idx: usize,
    pty_writer: Option<Box<dyn Write + Send>>,
    _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    _pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    focus_terminal: bool,
    selecting_in_terminal: bool,
    /// Per-row logical line number (0 = wrapped continuation row), for the gutter.
    /// Rebuilt only when the editor's galley changes.
    gutter_cache: Option<(Arc<egui::Galley>, Vec<u32>)>,
    /// Line count from the last rebuild, used to size the gutter.
    line_count: usize,
    cursor_line: usize,
    cursor_col: usize,
    selected_chars: usize,
}

impl TextEditor {
    fn new(ctx: egui::Context) -> Self {
        ctx.set_zoom_factor(1.2);
        ctx.style_mut(|s| {
            s.scroll_animation = egui::style::ScrollAnimation::none();
        });
        ctx.options_mut(|o| {
            o.max_passes = std::num::NonZeroUsize::new(4).unwrap();
        });

        let term = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(TERM_ROWS, TERM_COLS, 0),
            pty_updated: true,
        }));
        let mut pty_writer = None;
        let mut pty_master = None;
        let mut pty_child = None;

        let pty_system = native_pty_system();
        if let Ok(pair) = pty_system.openpty(PtySize {
            rows: TERM_ROWS,
            cols: TERM_COLS,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            let mut cmd = CommandBuilder::new("cmd.exe");
            if let Ok(cwd) = std::env::current_dir() {
                cmd.cwd(cwd);
            }
            if let Ok(child) = pair.slave.spawn_command(cmd) {
                pty_child = Some(child);
            }
            drop(pair.slave);

            if let Ok(writer) = pair.master.take_writer() {
                pty_writer = Some(writer);
            }

            if let Ok(mut reader) = pair.master.try_clone_reader() {
                let term_clone = Arc::clone(&term);
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = reader.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut lock = term_clone.lock().unwrap();
                        lock.parser.process(&buf[..n]);
                        lock.pty_updated = true;
                        drop(lock);
                        ctx.request_repaint();
                    }
                });
            }

            pty_master = Some(pair.master);
        } else {
            let mut lock = term.lock().unwrap();
            lock.parser.process(b"Failed to open ConPTY.\r\n");
        }

        let initial_folder = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .into_owned();
        let tree_root = Some(FileNode::new_root(PathBuf::from(&initial_folder)));

        Self {
            tab: Tab::Editor,
            path: String::new(),
            text: String::new(),
            text_revision: 0,
            file_name: "Untitled".to_string(),
            language_ext: "rs".to_string(),
            language_name: "Plain Text",
            language_salt: hash_language_salt("rs"),
            line_ending: "LF",
            folder_path: initial_folder,
            tree_root,
            term,
            term_screen_text: String::new(),
            term_cursor_idx: 0,
            pty_writer,
            _pty_master: pty_master,
            _pty_child: pty_child,
            focus_terminal: false,
            selecting_in_terminal: false,
            gutter_cache: None,
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
        }
    }

    fn refresh_line_ending(&mut self) {
        let check_len = self.text.len().min(4096);
        self.line_ending = if self.text.as_bytes()[..check_len]
            .windows(2)
            .any(|w| w == b"\r\n")
        {
            "CRLF"
        } else {
            "LF"
        };
    }

    fn refresh_file_metadata(&mut self) {
        if self.path.is_empty() {
            self.file_name = "Untitled".to_string();
            self.language_ext = "rs".to_string();
        } else {
            let p = Path::new(&self.path);
            self.file_name = p
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.clone());
            self.language_ext = p
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("rs")
                .to_string();
        }
        self.language_name = detect_language_name(&self.path);
        self.language_salt = hash_language_salt(&self.language_ext);
        self.refresh_line_ending();
    }

    fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Some(writer) = &mut self.pty_writer {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    fn handle_terminal_input(&mut self, ctx: &egui::Context, term_focused: bool) {
        if !term_focused {
            return;
        }

        let mut to_send: Vec<u8> = Vec::new();
        let mut exit_selection_mode = false;
        let mut entered_selection_mode = false;

        ctx.input_mut(|i| {
            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        exit_selection_mode = true;
                        false
                    }
                    egui::Event::Paste(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        exit_selection_mode = true;
                        false
                    }
                    egui::Event::Copy => {
                        if self.selecting_in_terminal {
                            true
                        } else {
                            to_send.push(0x03);
                            false
                        }
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if modifiers.shift
                            && matches!(
                                key,
                                egui::Key::ArrowLeft
                                    | egui::Key::ArrowRight
                                    | egui::Key::ArrowUp
                                    | egui::Key::ArrowDown
                                    | egui::Key::Home
                                    | egui::Key::End
                            )
                        {
                            entered_selection_mode = true;
                            return true;
                        }

                        let seq: Option<&[u8]> = match key {
                            egui::Key::Enter => Some(b"\r"),
                            egui::Key::Backspace => Some(b"\x7f"),
                            egui::Key::Tab => Some(b"\t"),
                            egui::Key::Escape => Some(b"\x1b"),
                            egui::Key::Delete => Some(b"\x1b[3~"),
                            egui::Key::ArrowUp => Some(b"\x1b[A"),
                            egui::Key::ArrowDown => Some(b"\x1b[B"),
                            egui::Key::ArrowRight => Some(b"\x1b[C"),
                            egui::Key::ArrowLeft => Some(b"\x1b[D"),
                            egui::Key::Home => Some(b"\x1b[H"),
                            egui::Key::End => Some(b"\x1b[F"),
                            egui::Key::PageUp => Some(b"\x1b[5~"),
                            egui::Key::PageDown => Some(b"\x1b[6~"),
                            egui::Key::C if modifiers.ctrl => Some(b"\x03"),
                            egui::Key::D if modifiers.ctrl => Some(b"\x04"),
                            egui::Key::Z if modifiers.ctrl => Some(b"\x1a"),
                            egui::Key::L if modifiers.ctrl => Some(b"\x0c"),
                            _ => None,
                        };

                        if let Some(bytes) = seq {
                            to_send.extend_from_slice(bytes);
                            exit_selection_mode = true;
                            false
                        } else {
                            true
                        }
                    }
                    _ => true,
                }
            });
        });

        if entered_selection_mode {
            self.selecting_in_terminal = true;
        }
        if exit_selection_mode {
            self.selecting_in_terminal = false;
        }
        if !to_send.is_empty() {
            self.write_to_pty(&to_send);
        }
    }
}

fn render_vt100_screen(screen: &vt100::Screen) -> (String, usize) {
    let (rows, cols) = screen.size();
    let (cur_row, cur_col) = screen.cursor_position();

    let mut full_text = String::with_capacity((rows as usize) * (cols as usize + 1));
    let mut cursor_char_idx = 0;
    let mut total_chars = 0;

    for (r, line) in screen.rows(0, cols).enumerate() {
        let r_u16 = r as u16;
        let line_chars = line.chars().count();
        full_text.push_str(&line);
        total_chars += line_chars;

        if r_u16 == cur_row {
            let target_col = cur_col as usize;
            if target_col > line_chars {
                let pad = target_col - line_chars;
                for _ in 0..pad {
                    full_text.push(' ');
                }
                total_chars += pad;
            }
            cursor_char_idx = (total_chars - line_chars.max(target_col)) + target_col;
        }

        if r_u16 + 1 < rows {
            full_text.push('\n');
            total_chars += 1;
        }
    }

    (full_text, cursor_char_idx)
}

fn fix_accesskit_caret(ctx: &egui::Context, output: &egui::text_edit::TextEditOutput) {
    let Some(cursor_range) = output.cursor_range else {
        return;
    };

    let cursor_rect = output
        .galley
        .pos_from_cursor(&cursor_range.primary)
        .translate(output.galley_pos.to_vec2());

    // If an edit/paste or large jump puts the cursor outside the visible scroll clip rect,
    // request an immediate second pass in the same frame so ScrollArea scrolls to it INSTANTLY,
    // and clear the selection on this pass so Windows Magnifier NEVER sees off-screen coordinates.
    if !output.text_clip_rect.contains(cursor_rect.center()) {
        ctx.request_discard("scroll to cursor after paste/edit");
        ctx.accesskit_node_builder(output.response.id, |builder| {
            builder.clear_text_selection();
        });
        return;
    }

    let focus = cursor_range.primary.rcursor;
    let Some(row) = output.galley.rows.get(focus.row) else {
        return;
    };

    let widget_id = output.response.id;
    let row_id = widget_id.with(focus.row);

    // Pad the active row's AccessKit TextRun with a 1px leading anchor at x=0 (index 0)
    // and a 1px trailing anchor at the end of the row (index N+1), then report the caret at
    // `focus.column + 1`.
    // This prevents `accesskit_consumer` from treating column 0 (`focus.column == 0`) as
    // `is_run_start()` (which otherwise jumps backward to the previous row's far-right `\n`
    // when arrowing right across a line break) or treating an empty last line as `is_document_end()`.
    ctx.accesskit_node_builder(row_id, |builder| {
        let mut row_min = output.galley_pos + row.rect.min.to_vec2();
        let mut row_max = output.galley_pos + row.rect.max.to_vec2();

        // Hard-clamp row bounds to the visible text area so coordinates can NEVER escape beyond the window
        row_min.y = row_min.y.clamp(output.text_clip_rect.min.y, output.text_clip_rect.max.y);
        row_max.y = row_max.y.clamp(row_min.y, output.text_clip_rect.max.y);
        row_min.x = row_min.x.clamp(output.text_clip_rect.min.x, output.text_clip_rect.max.x);
        row_max.x = row_max.x.clamp(row_min.x, output.text_clip_rect.max.x);

        if row_max.x - row_min.x < 2.0 {
            row_max.x = row_min.x + 2.0;
        }
        let rect = egui::Rect::from_min_max(row_min, row_max);
        builder.set_bounds(egui::accesskit::Rect {
            x0: rect.min.x.into(),
            y0: rect.min.y.into(),
            x1: rect.max.x.into(),
            y1: rect.max.y.into(),
        });

        let glyph_count = row.glyphs.len();
        let mut value = String::with_capacity(glyph_count + 2);
        let mut character_lengths = Vec::<u8>::with_capacity(glyph_count + 2);
        let mut character_positions = Vec::<f32>::with_capacity(glyph_count + 2);
        let mut character_widths = Vec::<f32>::with_capacity(glyph_count + 2);

        // Index 0: leading anchor at x = 0.0 on this row
        value.push(' ');
        character_lengths.push(1);
        character_positions.push(0.0);
        character_widths.push(1.0);

        for glyph in &row.glyphs {
            let old_len = value.len();
            value.push(glyph.chr);
            character_lengths.push((value.len() - old_len) as u8);
            character_positions.push(glyph.pos.x - row.rect.min.x);
            character_widths.push(glyph.advance_width.max(1.0));
        }

        // Index N+1: trailing anchor at the right end of this row
        let end_x = row.rect.width();
        value.push(if row.ends_with_newline { '\n' } else { ' ' });
        character_lengths.push(1);
        character_positions.push(end_x);
        character_widths.push(1.0);

        let mut remaining = character_lengths.len();
        let mut word_lengths = Vec::with_capacity((remaining / 255) + 1);
        while remaining > 0 {
            let chunk = remaining.min(u8::MAX as usize);
            word_lengths.push(chunk as u8);
            remaining -= chunk;
        }
        builder.set_value(value);
        builder.set_character_lengths(character_lengths);
        builder.set_character_positions(character_positions);
        builder.set_character_widths(character_widths);
        builder.set_word_lengths(word_lengths);
    });

    ctx.accesskit_node_builder(widget_id, |builder| {
        if let Some(sel) = builder.text_selection() {
            let target_idx = (focus.column + 1).min(row.glyphs.len() + 1);
            let pos = egui::accesskit::TextPosition {
                node: sel.focus.node,
                character_index: target_idx,
            };
            builder.set_text_selection(egui::accesskit::TextSelection {
                anchor: pos,
                focus: pos,
            });
        }
    });
}

fn detect_language_name(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "Rust",
        "toml" => "TOML",
        "json" => "JSON",
        "md" | "markdown" => "Markdown",
        "py" => "Python",
        "js" => "JavaScript",
        "ts" => "TypeScript",
        "jsx" => "JavaScript React",
        "tsx" => "TypeScript React",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "scss" | "sass" => "SCSS",
        "c" | "h" => "C",
        "cpp" | "cxx" | "cc" | "hpp" => "C++",
        "cs" => "C#",
        "go" => "Go",
        "java" => "Java",
        "sh" | "bash" | "zsh" => "Shell Script",
        "bat" | "cmd" => "Batch",
        "yaml" | "yml" => "YAML",
        "xml" => "XML",
        "sql" => "SQL",
        "lua" => "Lua",
        "r" => "R",
        "rb" => "Ruby",
        "php" => "PHP",
        "txt" => "Plain Text",
        _ => {
            if path.is_empty() {
                "Plain Text"
            } else {
                "Text"
            }
        }
    }
}

impl eframe::App for TextEditor {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let prev_tab = self.tab;
                ui.selectable_value(&mut self.tab, Tab::Editor, "Editor");
                ui.selectable_value(&mut self.tab, Tab::Terminal, "Terminal");
                if prev_tab != Tab::Terminal && self.tab == Tab::Terminal {
                    self.focus_terminal = true;
                }

                if self.tab == Tab::Editor {
                    ui.separator();
                    ui.label("Folder:");
                    let resp = ui.text_edit_singleline(&mut self.folder_path);
                    let enter_pressed =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    if ui.button("Open").clicked() || enter_pressed {
                        self.tree_root =
                            Some(FileNode::new_root(PathBuf::from(&self.folder_path)));
                    }

                    if ui.button("..").on_hover_text("Parent folder").clicked() {
                        let p = PathBuf::from(&self.folder_path);
                        if let Some(parent) = p.parent() {
                            if !parent.as_os_str().is_empty() {
                                self.folder_path = parent.to_string_lossy().into_owned();
                                self.tree_root =
                                    Some(FileNode::new_root(parent.to_path_buf()));
                            }
                        }
                    }

                    if !self.path.is_empty() && ui.button("Save").clicked() {
                        let _ = fs::write(&self.path, &self.text);
                    }
                } else if self.tab == Tab::Terminal {
                    ui.separator();
                    if ui.button("Clear (cls)").clicked() {
                        self.write_to_pty(b"cls\r");
                        self.focus_terminal = true;
                    }
                    if ui.button("Ctrl+C").clicked() {
                        self.write_to_pty(b"\x03");
                        self.focus_terminal = true;
                    }
                }
            });
        });

        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(22.0)
            .show(ctx, |ui| {
                ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
                ui.style_mut().spacing.item_spacing = egui::vec2(6.0, 0.0);
                ui.horizontal_centered(|ui| {
                    match self.tab {
                        Tab::Editor => {
                            let file_name = &self.file_name;
                            let line_count = self.line_count.max(1);
                            let bytes = self.text.len();
                            let size_str = if bytes >= 1024 * 1024 {
                                format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                            } else if bytes >= 1024 {
                                format!("{:.1} KB", bytes as f64 / 1024.0)
                            } else {
                                format!("{bytes} B")
                            };

                            ui.label(format!("{file_name} — {line_count} lines ({size_str})"));

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(self.language_name)
                                        .on_hover_text("Language Mode");

                                    ui.separator();
                                    ui.label(self.line_ending)
                                        .on_hover_text("End of Line Sequence");

                                    ui.separator();
                                    ui.label("UTF-8").on_hover_text("Encoding");

                                    ui.separator();
                                    ui.label("Spaces: 4").on_hover_text("Indentation");

                                    ui.separator();
                                    let pos_text = if self.selected_chars > 0 {
                                        format!(
                                            "Ln {}, Col {} ({} selected)",
                                            self.cursor_line, self.cursor_col, self.selected_chars
                                        )
                                    } else {
                                        format!("Ln {}, Col {}", self.cursor_line, self.cursor_col)
                                    };
                                    ui.label(pos_text).on_hover_text("Cursor Position");
                                },
                            );
                        }
                        Tab::Terminal => {
                            ui.label("Terminal — cmd.exe");
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(format!("{TERM_COLS}x{TERM_ROWS}"))
                                        .on_hover_text("Terminal Size");
                                    ui.separator();
                                    ui.label("UTF-8").on_hover_text("Encoding");
                                },
                            );
                        }
                    }
                });
            });

        let panel_frame = egui::Frame::central_panel(&ctx.style())
            .inner_margin(4.0)
            .fill(ctx.style().visuals.extreme_bg_color);

        match self.tab {
            Tab::Editor => {
                egui::SidePanel::left("explorer_sidebar")
                    .resizable(true)
                    .default_width(220.0)
                    .min_width(140.0)
                    .show(ctx, |ui| {
                        let mut file_to_open: Option<PathBuf> = None;
                        let current_path = Path::new(&self.path);

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if let Some(root) = &mut self.tree_root {
                                    for child in &mut root.children {
                                        render_file_node(
                                            ui,
                                            child,
                                            current_path,
                                            0,
                                            &mut file_to_open,
                                        );
                                    }
                                }
                            });

                        if let Some(chosen) = file_to_open {
                            if let Ok(content) = fs::read_to_string(&chosen) {
                                self.text = content;
                                self.path = chosen.to_string_lossy().into_owned();
                                self.text_revision = self.text_revision.wrapping_add(1);
                                self.refresh_file_metadata();
                                self.cursor_line = 1;
                                self.cursor_col = 1;
                                self.selected_chars = 0;
                                // Reset cursor to the start of the newly opened file
                                let editor_id = egui::Id::new("code_editor");
                                ctx.data_mut(|d| {
                                    if let Some(mut st) =
                                        d.get_temp::<editor::FastEditorState>(editor_id)
                                    {
                                        st.cursor.set_char_range(Some(
                                            egui::text::CCursorRange::one(
                                                egui::text::CCursor::new(0),
                                            ),
                                        ));
                                        st.scroll_to_cursor = true;
                                        d.insert_temp(editor_id, st);
                                    }
                                });
                            }
                        }
                    });

                let language = self.language_ext.as_str();
                let language_salt = self.language_salt;
                let text_revision = self.text_revision;

                let mut editor_text_changed = false;
                egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
                    let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
                    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                    let text_color = ui.visuals().widgets.inactive.text_color();
                    let font_id_for_layout = font_id.clone();

                    let mut layouter = |ui: &egui::Ui, text: &str, wrap_width: f32| {
                        let layout_job = if text.len() <= MAX_HIGHLIGHT_BYTES {
                            let mut job = syntax_highlighting::highlight(
                                ui.ctx(),
                                ui.style(),
                                &theme,
                                text,
                                language,
                            );
                            job.wrap.max_width = wrap_width;
                            job
                        } else {
                            egui::text::LayoutJob::simple(
                                text.to_owned(),
                                font_id_for_layout.clone(),
                                text_color,
                                wrap_width,
                            )
                        };
                        ui.fonts(|f| f.layout_job(layout_job))
                    };

                    let available_size = ui.available_size();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .animated(false)
                        .show(ui, |ui| {
                            // Use the line count from the last galley rebuild instead of
                            // scanning the whole text every frame.
                            let line_count = self.line_count.max(1);
                            let digits = line_count.ilog10() as usize + 1;
                            let gutter_width = (digits.max(3) as f32) * 9.0 + 20.0;

                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                let (gutter_rect, _) = ui.allocate_exact_size(
                                    egui::vec2(gutter_width, available_size.y),
                                    egui::Sense::hover(),
                                );

                                let editor_min_size = egui::vec2(
                                    (available_size.x - gutter_width).max(100.0),
                                    available_size.y,
                                );

                                let editor = FastEditor::new(
                                    egui::Id::new("code_editor"),
                                    &mut self.text,
                                    editor_min_size,
                                    &mut layouter,
                                )
                                .cache_salt(language_salt)
                                .text_revision(text_revision);
                                let output = editor.show(ui);
                                editor_text_changed = output.text_changed;

                                // Rebuild the row -> line-number table only when the galley changed.
                                let stale = match &self.gutter_cache {
                                    Some((g, _)) => !Arc::ptr_eq(g, &output.galley),
                                    None => true,
                                };
                                if stale {
                                    let mut nums = Vec::with_capacity(output.galley.rows.len());
                                    let mut line_no = 0u32;
                                    let mut new_line = true;
                                    for row in &output.galley.rows {
                                        if new_line {
                                            line_no += 1;
                                            nums.push(line_no);
                                        } else {
                                            nums.push(0);
                                        }
                                        new_line = row.ends_with_newline;
                                    }
                                    let new_count = (line_no as usize).max(1);
                                    if new_count.max(1).ilog10() != self.line_count.max(1).ilog10() {
                                        ui.ctx().request_repaint();
                                    }
                                    self.line_count = new_count;
                                    self.gutter_cache = Some((Arc::clone(&output.galley), nums));
                                }

                                if let Some(cursor_range) = output.cursor_range {
                                    let focus = cursor_range.primary.rcursor;
                                    self.cursor_line = if let Some((_, nums)) = &self.gutter_cache {
                                        if nums.is_empty() {
                                            1
                                        } else {
                                            let end = focus.row.min(nums.len() - 1);
                                            nums[..=end]
                                                .iter()
                                                .rev()
                                                .copied()
                                                .find(|&n| n > 0)
                                                .map(|n| n as usize)
                                                .unwrap_or(1)
                                        }
                                    } else {
                                        focus.row + 1
                                    };
                                    self.cursor_col = focus.column + 1;
                                    self.selected_chars = if cursor_range.is_empty() {
                                        0
                                    } else {
                                        cursor_range
                                            .primary
                                            .ccursor
                                            .index
                                            .abs_diff(cursor_range.secondary.ccursor.index)
                                    };
                                }

                                let clip_rect = ui.clip_rect();
                                let painter = ui.painter();
                                let active_row =
                                    output.cursor_range.map(|c| c.primary.rcursor.row);
                                if let Some((_, nums)) = &self.gutter_cache {
                                    let rows = &output.galley.rows;
                                    // Binary-search the first visible row; draw only visible rows.
                                    let top = clip_rect.min.y - output.galley_pos.y;
                                    let first = rows.partition_point(|r| r.rect.max.y < top);
                                    let mut num_buf = [0u8; 16];
                                    for row_idx in first..rows.len() {
                                        let row = &rows[row_idx];
                                        let y = output.galley_pos.y + row.rect.min.y;
                                        if y > clip_rect.max.y {
                                            break;
                                        }
                                        let n = nums.get(row_idx).copied().unwrap_or(0);
                                        if n == 0 {
                                            continue;
                                        }
                                        let color = if active_row == Some(row_idx) {
                                            ui.visuals().strong_text_color()
                                        } else {
                                            ui.visuals().weak_text_color()
                                        };
                                        let label_str = format_u32_stack(n, &mut num_buf);
                                        painter.text(
                                            egui::pos2(gutter_rect.max.x - 10.0, y),
                                            egui::Align2::RIGHT_TOP,
                                            label_str,
                                            font_id.clone(),
                                            color,
                                        );
                                    }
                                }

                                let bottom_y =
                                    gutter_rect.max.y.max(output.response.rect.max.y);
                                painter.line_segment(
                                    [
                                        egui::pos2(gutter_rect.max.x - 4.0, gutter_rect.min.y),
                                        egui::pos2(gutter_rect.max.x - 4.0, bottom_y),
                                    ],
                                    ui.visuals().widgets.noninteractive.bg_stroke,
                                );
                            });
                        });
                });

                if editor_text_changed {
                    self.text_revision = self.text_revision.wrapping_add(1);
                    self.refresh_line_ending();
                }
            }
            Tab::Terminal => {
                let term_id = egui::Id::new("conpty_terminal");
                let term_focused = ctx.memory(|m| m.has_focus(term_id));

                self.handle_terminal_input(ctx, term_focused);

                let pty_updated = {
                    let mut lock = self.term.lock().unwrap();
                    let updated = lock.pty_updated || self.term_screen_text.is_empty();
                    if updated {
                        lock.pty_updated = false;
                        let (text, idx) = render_vt100_screen(lock.parser.screen());
                        self.term_screen_text = text;
                        self.term_cursor_idx = idx;
                    }
                    updated
                };

                if pty_updated {
                    self.selecting_in_terminal = false;
                }

                if !self.selecting_in_terminal {
                    let mut state =
                        egui::text_edit::TextEditState::load(ctx, term_id).unwrap_or_default();
                    let ccursor = egui::text::CCursor::new(self.term_cursor_idx);
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::one(ccursor)));
                    state.store(ctx, term_id);
                }

                let screen_text = &mut self.term_screen_text;
                let focus_terminal = &mut self.focus_terminal;

                egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
                    let available_size = ui.available_size();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .animated(false)
                        .show(ui, |ui| {
                            let output = egui::TextEdit::multiline(screen_text)
                                .id(term_id)
                                .code_editor()
                                .frame(false)
                                .desired_width(f32::INFINITY)
                                .min_size(available_size)
                                .lock_focus(true)
                                .show(ui);

                            if *focus_terminal {
                                output.response.request_focus();
                                *focus_terminal = false;
                            }

                            fix_accesskit_caret(ctx, &output);
                        });
                });
            }
        }
    }
}
