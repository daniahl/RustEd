#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod editor;

use editor::FastEditor;
use eframe::egui;
use egui_extras::syntax_highlighting::{self, CodeTheme};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    collections::HashMap,
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

struct UnsavedFileState {
    text: String,
    saved_text: String,
    editor_state: Option<editor::FastEditorState>,
    line_count: usize,
    cursor_line: usize,
    cursor_col: usize,
    selected_chars: usize,
}

#[derive(Clone, PartialEq, Eq)]
enum TreeEditKind {
    NewFolder { parent_path: PathBuf },
    NewFile { parent_path: PathBuf },
    Rename { target_path: PathBuf, is_dir: bool },
}

#[derive(Clone)]
struct PendingTreeEdit {
    kind: TreeEditKind,
    input_text: String,
    had_focus: bool,
}

enum TreeAction {
    StartNewFolder(PathBuf),
    StartNewFile(PathBuf),
    StartRename {
        path: PathBuf,
        current_name: String,
        is_dir: bool,
    },
    DeleteNode {
        path: PathBuf,
        is_dir: bool,
    },
}

#[cfg(target_os = "windows")]
fn move_to_trash(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[cfg_attr(target_arch = "x86", repr(C, packed))]
    #[cfg_attr(not(target_arch = "x86"), repr(C))]
    struct SHFILEOPSTRUCTW {
        hwnd: *mut std::ffi::c_void,
        w_func: u32,
        p_from: *const u16,
        p_to: *const u16,
        f_flags: u16,
        f_any_operations_aborted: i32,
        h_name_mappings: *mut std::ffi::c_void,
        lpsz_progress_title: *const u16,
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHFileOperationW(lp_file_op: *mut SHFILEOPSTRUCTW) -> i32;
    }

    const FO_DELETE: u32 = 0x0003;
    const FOF_SILENT: u16 = 0x0004;
    const FOF_NOCONFIRMATION: u16 = 0x0010;
    const FOF_ALLOWUNDO: u16 = 0x0040;
    const FOF_NOERRORUI: u16 = 0x0400;

    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        return false;
    };

    let mut path_str = abs_path.to_string_lossy().into_owned();
    if let Some(stripped) = path_str.strip_prefix(r"\\?\") {
        path_str = stripped.to_string();
    }
    while path_str.len() > 3 && (path_str.ends_with('\\') || path_str.ends_with('/')) {
        path_str.pop();
    }

    let mut wide: Vec<u16> = std::ffi::OsStr::new(&path_str)
        .encode_wide()
        .chain([0, 0])
        .collect();

    let mut file_op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        w_func: FO_DELETE,
        p_from: wide.as_mut_ptr(),
        p_to: std::ptr::null(),
        f_flags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT,
        f_any_operations_aborted: 0,
        h_name_mappings: std::ptr::null_mut(),
        lpsz_progress_title: std::ptr::null(),
    };

    let ret = unsafe { SHFileOperationW(&mut file_op) };
    ret == 0 && file_op.f_any_operations_aborted == 0
}

#[cfg(not(target_os = "windows"))]
fn move_to_trash(path: &Path) -> bool {
    if path.is_dir() {
        fs::remove_dir_all(path).is_ok()
    } else {
        fs::remove_file(path).is_ok()
    }
}

enum TreeEditOutcome {
    Commit,
    Cancel,
}

struct FileNode {
    name: String,
    dirty_label: String,
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
            dirty_label: String::new(),
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

    fn find_mut(&mut self, target: &Path) -> Option<&mut FileNode> {
        if self.path == target {
            return Some(self);
        }
        for child in &mut self.children {
            if let Some(found) = child.find_mut(target) {
                return Some(found);
            }
        }
        None
    }

    fn load_children(&mut self) {
        self.children_loaded = true;
        let mut old_dirs: HashMap<PathBuf, (bool, bool, Vec<FileNode>)> = self
            .children
            .drain(..)
            .filter(|c| c.is_dir && (c.expanded || c.children_loaded))
            .map(|c| (c.path, (c.expanded, c.children_loaded, c.children)))
            .collect();

        if let Ok(entries) = fs::read_dir(&self.path) {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
                let name = entry.file_name().to_string_lossy().into_owned();
                let (dirty_label, collapsed_label, expanded_label) = if is_dir {
                    (String::new(), format!("▸ {name}"), format!("▾ {name}"))
                } else {
                    (format!("{name} *"), String::new(), String::new())
                };
                let (expanded, children_loaded, children) = if is_dir {
                    old_dirs
                        .remove(&path)
                        .unwrap_or((false, false, Vec::new()))
                } else {
                    (false, false, Vec::new())
                };
                self.children.push(Self {
                    name,
                    dirty_label,
                    collapsed_label,
                    expanded_label,
                    path,
                    is_dir,
                    expanded,
                    children_loaded,
                    children,
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

fn render_inline_tree_edit(
    ui: &mut egui::Ui,
    edit: &mut PendingTreeEdit,
    edit_outcome: &mut Option<TreeEditOutcome>,
) {
    let edit_id = egui::Id::new("tree_inline_editor");
    if !edit.had_focus {
        ui.memory_mut(|m| m.request_focus(edit_id));
        let mut state = egui::text_edit::TextEditState::load(ui.ctx(), edit_id).unwrap_or_default();
        let char_len = edit.input_text.chars().count();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(char_len),
        )));
        state.store(ui.ctx(), edit_id);
    }

    let resp = egui::TextEdit::singleline(&mut edit.input_text)
        .id(edit_id)
        .desired_width((ui.available_width() - 4.0).max(80.0))
        .show(ui)
        .response;

    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
    let escape_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));

    if escape_pressed {
        *edit_outcome = Some(TreeEditOutcome::Cancel);
    } else if enter_pressed || (edit.had_focus && resp.lost_focus()) {
        *edit_outcome = Some(TreeEditOutcome::Commit);
    }

    if resp.has_focus() {
        edit.had_focus = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_file_node(
    ui: &mut egui::Ui,
    node: &mut FileNode,
    current_path: &Path,
    is_current_dirty: bool,
    unsaved_files: &HashMap<PathBuf, UnsavedFileState>,
    selected_node: &mut Option<(PathBuf, bool)>,
    pending_edit: &mut Option<PendingTreeEdit>,
    tree_action: &mut Option<TreeAction>,
    edit_outcome: &mut Option<TreeEditOutcome>,
    depth: usize,
    file_to_open: &mut Option<PathBuf>,
) {
    let indent = depth as f32 * 14.0;
    let is_renaming_self = matches!(
        pending_edit,
        Some(e) if matches!(&e.kind, TreeEditKind::Rename { target_path, .. } if target_path == &node.path)
    );

    if node.is_dir {
        if is_renaming_self {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.label(if node.expanded { "▾" } else { "▸" });
                if let Some(edit) = pending_edit.as_mut() {
                    render_inline_tree_edit(ui, edit, edit_outcome);
                }
            });
        } else {
            let is_selected = selected_node
                .as_ref()
                .map(|(p, _)| p == &node.path)
                .unwrap_or(false);
            ui.horizontal(|ui| {
                ui.add_space(indent);
                let label = if node.expanded {
                    node.expanded_label.as_str()
                } else {
                    node.collapsed_label.as_str()
                };
                let resp = ui.selectable_label(is_selected, label);
                if resp.clicked() {
                    *selected_node = Some((node.path.clone(), true));
                    node.expanded = !node.expanded;
                    if node.expanded && !node.children_loaded {
                        node.load_children();
                    }
                }
                if resp.secondary_clicked() {
                    *selected_node = Some((node.path.clone(), true));
                }
                resp.context_menu(|ui| {
                    *selected_node = Some((node.path.clone(), true));
                    if ui.button("New Folder").clicked() {
                        *tree_action = Some(TreeAction::StartNewFolder(node.path.clone()));
                        ui.close_menu();
                    }
                    if ui.button("New File").clicked() {
                        *tree_action = Some(TreeAction::StartNewFile(node.path.clone()));
                        ui.close_menu();
                    }
                    if ui.button("Rename").clicked() {
                        *tree_action = Some(TreeAction::StartRename {
                            path: node.path.clone(),
                            current_name: node.name.clone(),
                            is_dir: true,
                        });
                        ui.close_menu();
                    }
                    if ui.button("Delete").clicked() {
                        *tree_action = Some(TreeAction::DeleteNode {
                            path: node.path.clone(),
                            is_dir: true,
                        });
                        ui.close_menu();
                    }
                });
            });
        }

        if node.expanded {
            if let Some(edit) = pending_edit.as_mut() {
                match &edit.kind {
                    TreeEditKind::NewFolder { parent_path } if parent_path == &node.path => {
                        ui.horizontal(|ui| {
                            ui.add_space((depth + 1) as f32 * 14.0);
                            ui.label("▸");
                            render_inline_tree_edit(ui, edit, edit_outcome);
                        });
                    }
                    TreeEditKind::NewFile { parent_path } if parent_path == &node.path => {
                        ui.horizontal(|ui| {
                            ui.add_space((depth + 1) as f32 * 14.0 + 14.0);
                            render_inline_tree_edit(ui, edit, edit_outcome);
                        });
                    }
                    _ => {}
                }
            }

            for child in &mut node.children {
                render_file_node(
                    ui,
                    child,
                    current_path,
                    is_current_dirty,
                    unsaved_files,
                    selected_node,
                    pending_edit,
                    tree_action,
                    edit_outcome,
                    depth + 1,
                    file_to_open,
                );
            }
        }
    } else if is_renaming_self {
        ui.horizontal(|ui| {
            ui.add_space(indent + 14.0);
            if let Some(edit) = pending_edit.as_mut() {
                render_inline_tree_edit(ui, edit, edit_outcome);
            }
        });
    } else {
        let is_selected = match selected_node.as_ref() {
            Some((sel_path, _)) => sel_path == &node.path,
            None => current_path == node.path,
        };
        let is_dirty = (current_path == node.path && is_current_dirty)
            || (!unsaved_files.is_empty() && unsaved_files.contains_key(&node.path));
        let label = if is_dirty {
            node.dirty_label.as_str()
        } else {
            node.name.as_str()
        };
        ui.horizontal(|ui| {
            ui.add_space(indent + 14.0);
            let resp = ui.selectable_label(is_selected, label);
            if resp.clicked() {
                *selected_node = Some((node.path.clone(), false));
                *file_to_open = Some(node.path.clone());
            }
            if resp.secondary_clicked() {
                *selected_node = Some((node.path.clone(), false));
            }
            resp.context_menu(|ui| {
                *selected_node = Some((node.path.clone(), false));
                if ui.button("Rename").clicked() {
                    *tree_action = Some(TreeAction::StartRename {
                        path: node.path.clone(),
                        current_name: node.name.clone(),
                        is_dir: false,
                    });
                    ui.close_menu();
                }
                if ui.button("Delete").clicked() {
                    *tree_action = Some(TreeAction::DeleteNode {
                        path: node.path.clone(),
                        is_dir: false,
                    });
                    ui.close_menu();
                }
            });
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
    saved_text: String,
    is_current_dirty: bool,
    unsaved_files: HashMap<PathBuf, UnsavedFileState>,
    text_revision: u64,
    file_name: String,
    language_ext: String,
    language_name: &'static str,
    language_salt: u64,
    line_ending: &'static str,
    folder_path: String,
    tree_root: Option<FileNode>,
    selected_node: Option<(PathBuf, bool)>,
    pending_tree_edit: Option<PendingTreeEdit>,
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
    find_open: bool,
    find_query: String,
    focus_find_input: bool,
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
            saved_text: String::new(),
            is_current_dirty: false,
            unsaved_files: HashMap::new(),
            text_revision: 0,
            file_name: "Untitled".to_string(),
            language_ext: "rs".to_string(),
            language_name: "Plain Text",
            language_salt: hash_language_salt("rs"),
            line_ending: "LF",
            folder_path: initial_folder,
            tree_root,
            selected_node: None,
            pending_tree_edit: None,
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
            find_open: false,
            find_query: String::new(),
            focus_find_input: false,
        }
    }

    fn open_find_dialog(&mut self, ctx: &egui::Context) {
        let editor_id = egui::Id::new("code_editor");
        if let Some(state) = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id)) {
            if let Some(range) = state.cursor.char_range() {
                if range.primary != range.secondary {
                    let [min_c, max_c] = range.sorted();
                    let selected: String = self
                        .text
                        .chars()
                        .skip(min_c.index)
                        .take(max_c.index - min_c.index)
                        .collect();
                    if !selected.is_empty() && !selected.contains('\n') && !selected.contains('\r')
                    {
                        self.find_query = selected;
                    }
                }
            }
        }
        self.find_open = true;
        self.focus_find_input = true;
    }

    fn find_next(&mut self, ctx: &egui::Context) -> bool {
        if self.find_query.is_empty() || self.text.is_empty() {
            return false;
        }
        let editor_id = egui::Id::new("code_editor");
        let mut state = ctx
            .data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id))
            .unwrap_or_default();

        let start_char_idx = state
            .cursor
            .char_range()
            .map(|r| r.primary.index.max(r.secondary.index))
            .unwrap_or(0);

        let start_byte = self
            .text
            .char_indices()
            .nth(start_char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len());

        let found_byte = self.text[start_byte..]
            .find(&self.find_query)
            .map(|rel| start_byte + rel)
            .or_else(|| self.text.find(&self.find_query));

        let Some(match_byte) = found_byte else {
            return false;
        };

        let match_start_char = self.text[..match_byte].chars().count();
        let match_end_char = match_start_char + self.find_query.chars().count();

        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(match_start_char),
            egui::text::CCursor::new(match_end_char),
        )));
        state.scroll_to_cursor = true;
        ctx.data_mut(|d| d.insert_temp(editor_id, state));
        ctx.memory_mut(|m| {
            m.request_focus(editor_id);
            m.set_focus_lock_filter(
                editor_id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    tab: true,
                    escape: true,
                },
            );
        });
        ctx.request_repaint();
        true
    }

    fn can_save(&self) -> bool {
        !self.path.is_empty() && self.is_current_dirty
    }

    #[allow(dead_code)]
    fn is_file_dirty(&self, path: &Path) -> bool {
        (self.is_current_dirty && Path::new(&self.path) == path)
            || (!self.unsaved_files.is_empty() && self.unsaved_files.contains_key(path))
    }

    fn save_current_file(&mut self) {
        if !self.path.is_empty() && self.is_current_dirty {
            if fs::write(&self.path, &self.text).is_ok() {
                self.saved_text.clone_from(&self.text);
                self.is_current_dirty = false;
                self.unsaved_files.remove(Path::new(&self.path));
            }
        }
    }

    fn open_file(&mut self, ctx: &egui::Context, chosen: PathBuf) {
        self.selected_node = Some((chosen.clone(), false));
        if !self.path.is_empty() && Path::new(&self.path) == chosen {
            return;
        }

        enum IncomingFile {
            Unsaved(UnsavedFileState),
            FromDisk(String),
        }

        let incoming = if let Some(unsaved) = self.unsaved_files.remove(&chosen) {
            IncomingFile::Unsaved(unsaved)
        } else if let Ok(content) = fs::read_to_string(&chosen) {
            IncomingFile::FromDisk(content)
        } else {
            return;
        };

        let editor_id = egui::Id::new("code_editor");
        if !self.path.is_empty() && self.is_current_dirty {
            let mut prev_state = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id));
            if let Some(ref mut st) = prev_state {
                st.layout_cache = None;
            }
            self.unsaved_files.insert(
                PathBuf::from(&self.path),
                UnsavedFileState {
                    text: std::mem::take(&mut self.text),
                    saved_text: std::mem::take(&mut self.saved_text),
                    editor_state: prev_state,
                    line_count: self.line_count,
                    cursor_line: self.cursor_line,
                    cursor_col: self.cursor_col,
                    selected_chars: self.selected_chars,
                },
            );
        }

        self.path = chosen.to_string_lossy().into_owned();
        self.text_revision = self.text_revision.wrapping_add(1);
        self.gutter_cache = None;

        match incoming {
            IncomingFile::Unsaved(unsaved) => {
                self.text = unsaved.text;
                self.saved_text = unsaved.saved_text;
                self.is_current_dirty = self.text != self.saved_text;
                self.line_count = unsaved.line_count;
                self.cursor_line = unsaved.cursor_line;
                self.cursor_col = unsaved.cursor_col;
                self.selected_chars = unsaved.selected_chars;
                self.refresh_file_metadata();

                ctx.data_mut(|d| {
                    let mut st = unsaved.editor_state.unwrap_or_default();
                    st.scroll_to_cursor = true;
                    d.insert_temp(editor_id, st);
                });
            }
            IncomingFile::FromDisk(content) => {
                self.line_count = content.bytes().filter(|&b| b == b'\n').count() + 1;
                self.saved_text = content.clone();
                self.text = content;
                self.is_current_dirty = false;
                self.cursor_line = 1;
                self.cursor_col = 1;
                self.selected_chars = 0;
                self.refresh_file_metadata();

                ctx.data_mut(|d| {
                    let mut st = editor::FastEditorState::default();
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(0),
                    )));
                    st.scroll_to_cursor = true;
                    d.insert_temp(editor_id, st);
                });
            }
        }
    }

    fn start_rename_selected(&mut self) {
        let Some((ref path, is_dir)) = self.selected_node else {
            return;
        };
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        self.pending_tree_edit = Some(PendingTreeEdit {
            kind: TreeEditKind::Rename {
                target_path: path.clone(),
                is_dir,
            },
            input_text: name,
            had_focus: false,
        });
    }

    fn handle_tree_action(&mut self, ctx: &egui::Context, action: TreeAction) {
        match action {
            TreeAction::StartNewFolder(parent_path) => {
                if let Some(root) = &mut self.tree_root {
                    if let Some(folder_node) = root.find_mut(&parent_path) {
                        if !folder_node.children_loaded {
                            folder_node.load_children();
                        }
                        folder_node.expanded = true;
                    }
                }
                self.pending_tree_edit = Some(PendingTreeEdit {
                    kind: TreeEditKind::NewFolder { parent_path },
                    input_text: String::new(),
                    had_focus: false,
                });
            }
            TreeAction::StartNewFile(parent_path) => {
                if let Some(root) = &mut self.tree_root {
                    if let Some(folder_node) = root.find_mut(&parent_path) {
                        if !folder_node.children_loaded {
                            folder_node.load_children();
                        }
                        folder_node.expanded = true;
                    }
                }
                self.pending_tree_edit = Some(PendingTreeEdit {
                    kind: TreeEditKind::NewFile { parent_path },
                    input_text: String::new(),
                    had_focus: false,
                });
            }
            TreeAction::StartRename {
                path,
                current_name,
                is_dir,
            } => {
                self.selected_node = Some((path.clone(), is_dir));
                self.pending_tree_edit = Some(PendingTreeEdit {
                    kind: TreeEditKind::Rename {
                        target_path: path,
                        is_dir,
                    },
                    input_text: current_name,
                    had_focus: false,
                });
            }
            TreeAction::DeleteNode { path, is_dir } => {
                let _ = self.delete_node(ctx, &path, is_dir);
            }
        }
    }

    fn delete_node(&mut self, ctx: &egui::Context, path: &Path, is_dir: bool) -> bool {
        if !move_to_trash(path) {
            return false;
        }

        if let Some((ref sel_path, _)) = self.selected_node {
            if sel_path == path || (is_dir && sel_path.starts_with(path)) {
                self.selected_node = None;
            }
        }

        if let Some(ref edit) = self.pending_tree_edit {
            let matches_deleted = match &edit.kind {
                TreeEditKind::NewFolder { parent_path }
                | TreeEditKind::NewFile { parent_path } => {
                    parent_path == path || (is_dir && parent_path.starts_with(path))
                }
                TreeEditKind::Rename { target_path, .. } => {
                    target_path == path || (is_dir && target_path.starts_with(path))
                }
            };
            if matches_deleted {
                self.pending_tree_edit = None;
            }
        }

        if is_dir {
            self.unsaved_files.retain(|k, _| !k.starts_with(path));
        } else {
            self.unsaved_files.remove(path);
        }

        if !self.path.is_empty() {
            let cur = Path::new(&self.path);
            if cur == path || (is_dir && cur.starts_with(path)) {
                self.path.clear();
                self.text.clear();
                self.saved_text.clear();
                self.is_current_dirty = false;
                self.text_revision = self.text_revision.wrapping_add(1);
                self.gutter_cache = None;
                self.line_count = 1;
                self.cursor_line = 1;
                self.cursor_col = 1;
                self.selected_chars = 0;
                self.refresh_file_metadata();

                let editor_id = egui::Id::new("code_editor");
                ctx.data_mut(|d| {
                    d.insert_temp(editor_id, editor::FastEditorState::default());
                });
            }
        }

        if let Some(parent) = path.parent() {
            if let Some(root) = &mut self.tree_root {
                if let Some(parent_node) = root.find_mut(parent) {
                    parent_node.load_children();
                }
            }
        }

        true
    }

    fn create_folder_in(&mut self, parent_path: &Path, name: &str) -> Option<PathBuf> {
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '\\']) {
            return None;
        }
        let new_path = parent_path.join(name);
        if new_path.exists() {
            return None;
        }
        fs::create_dir(&new_path).ok()?;
        if let Some(root) = &mut self.tree_root {
            if let Some(parent_node) = root.find_mut(parent_path) {
                parent_node.expanded = true;
                parent_node.load_children();
            }
        }
        self.selected_node = Some((new_path.clone(), true));
        Some(new_path)
    }

    fn create_file_in(
        &mut self,
        ctx: &egui::Context,
        parent_path: &Path,
        name: &str,
    ) -> Option<PathBuf> {
        let name = name.trim();
        if name.is_empty() || name.contains(['/', '\\']) {
            return None;
        }
        let new_path = parent_path.join(name);
        if new_path.exists() {
            return None;
        }
        fs::write(&new_path, "").ok()?;
        if let Some(root) = &mut self.tree_root {
            if let Some(parent_node) = root.find_mut(parent_path) {
                parent_node.expanded = true;
                parent_node.load_children();
            }
        }
        self.open_file(ctx, new_path.clone());
        Some(new_path)
    }

    fn rename_node(&mut self, old_path: &Path, new_name: &str, is_dir: bool) -> Option<PathBuf> {
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.contains(['/', '\\']) {
            return None;
        }
        let parent = old_path.parent()?;
        let new_path = parent.join(new_name);
        if new_path == old_path || new_path.exists() {
            return None;
        }
        fs::rename(old_path, &new_path).ok()?;

        if let Some((ref mut sel_path, _)) = self.selected_node {
            if sel_path == old_path {
                *sel_path = new_path.clone();
            } else if is_dir {
                if let Ok(rel) = sel_path.strip_prefix(old_path) {
                    *sel_path = new_path.join(rel);
                }
            }
        }

        if !self.path.is_empty() {
            let cur = Path::new(&self.path);
            if cur == old_path {
                self.path = new_path.to_string_lossy().into_owned();
                self.refresh_file_metadata();
            } else if is_dir {
                if let Ok(rel) = cur.strip_prefix(old_path) {
                    self.path = new_path.join(rel).to_string_lossy().into_owned();
                    self.refresh_file_metadata();
                }
            }
        }

        if !self.unsaved_files.is_empty() {
            if is_dir {
                let affected: Vec<PathBuf> = self
                    .unsaved_files
                    .keys()
                    .filter(|k| k.starts_with(old_path))
                    .cloned()
                    .collect();
                for k in affected {
                    if let (Ok(rel), Some(state)) =
                        (k.strip_prefix(old_path), self.unsaved_files.remove(&k))
                    {
                        self.unsaved_files.insert(new_path.join(rel), state);
                    }
                }
            } else if let Some(state) = self.unsaved_files.remove(old_path) {
                self.unsaved_files.insert(new_path.clone(), state);
            }
        }

        if let Some(root) = &mut self.tree_root {
            let was_expanded = is_dir
                && root
                    .find_mut(old_path)
                    .map(|n| n.expanded)
                    .unwrap_or(false);
            if let Some(parent_node) = root.find_mut(parent) {
                parent_node.load_children();
            }
            if was_expanded {
                if let Some(renamed_node) = root.find_mut(&new_path) {
                    renamed_node.expanded = true;
                    renamed_node.load_children();
                }
            }
        }

        Some(new_path)
    }

    fn commit_tree_edit(&mut self, ctx: &egui::Context, edit: PendingTreeEdit) {
        match edit.kind {
            TreeEditKind::NewFolder { parent_path } => {
                let _ = self.create_folder_in(&parent_path, &edit.input_text);
            }
            TreeEditKind::NewFile { parent_path } => {
                let _ = self.create_file_in(ctx, &parent_path, &edit.input_text);
            }
            TreeEditKind::Rename {
                target_path,
                is_dir,
            } => {
                let _ = self.rename_node(&target_path, &edit.input_text, is_dir);
            }
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
        self.update_ui(ctx);
    }
}

impl TextEditor {
    fn update_ui(&mut self, ctx: &egui::Context) {
        if self.tab == Tab::Editor {
            let (ctrl_f, f3) = ctx.input_mut(|i| {
                let ctrl_f = i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)
                    || i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
                let f3 = i.consume_key(egui::Modifiers::NONE, egui::Key::F3);
                (ctrl_f, f3)
            });
            if ctrl_f {
                self.open_find_dialog(ctx);
            }
            if f3 {
                if self.find_query.is_empty() {
                    self.open_find_dialog(ctx);
                } else {
                    self.find_next(ctx);
                }
            }
        }

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
                        self.pending_tree_edit = None;
                        self.selected_node = None;
                        self.tree_root =
                            Some(FileNode::new_root(PathBuf::from(&self.folder_path)));
                    }

                    if ui.button("..").on_hover_text("Parent folder").clicked() {
                        let p = PathBuf::from(&self.folder_path);
                        if let Some(parent) = p.parent() {
                            if !parent.as_os_str().is_empty() {
                                self.pending_tree_edit = None;
                                self.selected_node = None;
                                self.folder_path = parent.to_string_lossy().into_owned();
                                self.tree_root =
                                    Some(FileNode::new_root(parent.to_path_buf()));
                            }
                        }
                    }

                    if ui.add_enabled(self.can_save(), egui::Button::new("Save")).clicked() {
                        self.save_current_file();
                    }

                    if ui.button("Find").on_hover_text("Find (Ctrl+F)").clicked() {
                        self.open_find_dialog(ctx);
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
                        if self.pending_tree_edit.is_none()
                            && ui.input(|i| i.key_pressed(egui::Key::F2))
                        {
                            self.start_rename_selected();
                        }

                        let mut file_to_open: Option<PathBuf> = None;
                        let mut tree_action: Option<TreeAction> = None;
                        let mut edit_outcome: Option<TreeEditOutcome> = None;
                        let current_path = Path::new(&self.path);
                        let is_current_dirty = self.is_current_dirty;
                        let unsaved_files = &self.unsaved_files;
                        let selected_node = &mut self.selected_node;
                        let pending_edit = &mut self.pending_tree_edit;

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if let Some(root) = &mut self.tree_root {
                                    for child in &mut root.children {
                                        render_file_node(
                                            ui,
                                            child,
                                            current_path,
                                            is_current_dirty,
                                            unsaved_files,
                                            selected_node,
                                            pending_edit,
                                            &mut tree_action,
                                            &mut edit_outcome,
                                            0,
                                            &mut file_to_open,
                                        );
                                    }
                                }
                            });

                        if let Some(outcome) = edit_outcome {
                            if let Some(edit) = self.pending_tree_edit.take() {
                                if matches!(outcome, TreeEditOutcome::Commit) {
                                    self.commit_tree_edit(ctx, edit);
                                }
                            }
                            ctx.request_repaint();
                        }

                        if let Some(action) = tree_action {
                            self.handle_tree_action(ctx, action);
                            ctx.request_repaint();
                        }

                        if let Some(chosen) = file_to_open {
                            self.open_file(ctx, chosen);
                        }
                    });

                if self.find_open {
                    let mut open = self.find_open;
                    let mut trigger_find_next = false;
                    let mut close_dialog = false;

                    egui::Window::new("Find")
                        .open(&mut open)
                        .collapsible(false)
                        .resizable(false)
                        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 36.0))
                        .show(ctx, |ui| {
                            ui.horizontal(|ui| {
                                let find_id = egui::Id::new("find_query_input");
                                if self.focus_find_input {
                                    ui.memory_mut(|m| m.request_focus(find_id));
                                    let mut st =
                                        egui::text_edit::TextEditState::load(ui.ctx(), find_id)
                                            .unwrap_or_default();
                                    let len = self.find_query.chars().count();
                                    st.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::two(
                                            egui::text::CCursor::new(0),
                                            egui::text::CCursor::new(len),
                                        ),
                                    ));
                                    st.store(ui.ctx(), find_id);
                                }

                                let resp = egui::TextEdit::singleline(&mut self.find_query)
                                    .id(find_id)
                                    .hint_text("Search text...")
                                    .desired_width(180.0)
                                    .show(ui)
                                    .response;

                                if self.focus_find_input {
                                    resp.request_focus();
                                    let mut st =
                                        egui::text_edit::TextEditState::load(ui.ctx(), find_id)
                                            .unwrap_or_default();
                                    let len = self.find_query.chars().count();
                                    st.cursor.set_char_range(Some(
                                        egui::text::CCursorRange::two(
                                            egui::text::CCursor::new(0),
                                            egui::text::CCursor::new(len),
                                        ),
                                    ));
                                    st.store(ui.ctx(), find_id);
                                    self.focus_find_input = false;
                                }

                                let enter_pressed =
                                    ui.input(|i| i.key_pressed(egui::Key::Enter));
                                let escape_pressed =
                                    ui.input(|i| i.key_pressed(egui::Key::Escape));
                                if enter_pressed {
                                    ui.input_mut(|i| {
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
                                    });
                                }
                                if escape_pressed {
                                    ui.input_mut(|i| {
                                        i.consume_key(egui::Modifiers::NONE, egui::Key::Escape);
                                    });
                                }

                                if ui
                                    .add_enabled(
                                        !self.find_query.is_empty(),
                                        egui::Button::new("Find"),
                                    )
                                    .on_hover_text("Find (Enter)")
                                    .clicked()
                                    || enter_pressed
                                {
                                    trigger_find_next = true;
                                    close_dialog = true;
                                }

                                if escape_pressed {
                                    close_dialog = true;
                                }
                            });
                        });

                    if close_dialog {
                        open = false;
                        let editor_id = egui::Id::new("code_editor");
                        ctx.memory_mut(|m| {
                            m.request_focus(editor_id);
                            m.set_focus_lock_filter(
                                editor_id,
                                egui::EventFilter {
                                    horizontal_arrows: true,
                                    vertical_arrows: true,
                                    tab: true,
                                    escape: true,
                                },
                            );
                        });
                        ctx.input_mut(|i| {
                            i.events.retain(|e| {
                                !matches!(
                                    e,
                                    egui::Event::Text(_)
                                        | egui::Event::Paste(_)
                                        | egui::Event::Cut
                                        | egui::Event::Copy
                                )
                            });
                        });
                    }
                    self.find_open = open;

                    if trigger_find_next {
                        self.find_next(ctx);
                    }
                }

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
                    let was_dirty = self.is_current_dirty;
                    self.is_current_dirty = !self.path.is_empty() && self.text != self.saved_text;
                    if was_dirty != self.is_current_dirty {
                        ctx.request_repaint();
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn new_test_app(folder: &Path) -> TextEditor {
        let folder_str = folder.to_string_lossy().into_owned();
        TextEditor {
            tab: Tab::Editor,
            path: String::new(),
            text: String::new(),
            saved_text: String::new(),
            is_current_dirty: false,
            unsaved_files: HashMap::new(),
            text_revision: 0,
            file_name: "Untitled".to_string(),
            language_ext: "rs".to_string(),
            language_name: "Plain Text",
            language_salt: hash_language_salt("rs"),
            line_ending: "LF",
            folder_path: folder_str,
            tree_root: Some(FileNode::new_root(folder.to_path_buf())),
            selected_node: None,
            pending_tree_edit: None,
            term: Arc::new(Mutex::new(TerminalState {
                parser: vt100::Parser::new(TERM_ROWS, TERM_COLS, 0),
                pty_updated: false,
            })),
            term_screen_text: String::new(),
            term_cursor_idx: 0,
            pty_writer: None,
            _pty_master: None,
            _pty_child: None,
            focus_terminal: false,
            selecting_in_terminal: false,
            gutter_cache: None,
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
            find_open: false,
            find_query: String::new(),
            focus_find_input: false,
        }
    }

    #[test]
    fn test_find_dialog_ctrl_f_and_f3_find_next() {
        let temp_dir =
            std::env::temp_dir().join(format!("rusted_find_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let ctx = egui::Context::default();
        ctx.options_mut(|o| {
            o.max_passes = std::num::NonZeroUsize::new(4).unwrap();
        });
        let mut app = new_test_app(&temp_dir);
        app.text = "alpha beta alpha gamma alpha".to_string();
        app.text_revision = 1;

        // Render initial frame
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.update_ui(ctx);
        });
        assert!(!app.find_open);

        // Press Ctrl+F for the very first time in the session to open Find dialog
        let mut ctrl_f_input = egui::RawInput::default();
        ctrl_f_input.events.push(egui::Event::Key {
            key: egui::Key::F,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        });
        let _ = ctx.run(ctrl_f_input, |ctx| {
            app.update_ui(ctx);
        });
        assert!(app.find_open, "Ctrl+F should open the Find dialog");
        assert!(
            ctx.memory(|m| m.has_focus(egui::Id::new("find_query_input"))),
            "Find input should have focus immediately on the first Ctrl+F"
        );

        // Type search query "alpha" and press Enter in the Find dialog
        let mut type_and_enter = egui::RawInput::default();
        type_and_enter
            .events
            .push(egui::Event::Text("alpha".to_string()));
        type_and_enter.events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run(type_and_enter, |ctx| {
            app.update_ui(ctx);
        });
        assert!(!app.find_open, "Pressing Enter in Find dialog should close it");
        assert_eq!(
            app.text, "alpha beta alpha gamma alpha",
            "Pressing Enter in Find dialog should not insert a newline into the editor"
        );

        let editor_id = egui::Id::new("code_editor");
        assert!(
            ctx.memory(|m| m.has_focus(editor_id)),
            "Editor should have focus after Find"
        );
        let get_sel = |ctx: &egui::Context| -> Option<(usize, usize)> {
            let st = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id))?;
            let r = st.cursor.char_range()?;
            let [min_c, max_c] = r.sorted();
            Some((min_c.index, max_c.index))
        };

        assert_eq!(get_sel(&ctx), Some((0, 5)), "Find should select first 'alpha'");

        // Press Escape in the editor -> focus and selection should remain intact
        let mut esc_input = egui::RawInput::default();
        esc_input.events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run(esc_input, |ctx| {
            app.update_ui(ctx);
        });
        let focused_after_esc = ctx.memory(|m| m.focused());
        assert_eq!(
            focused_after_esc,
            Some(editor_id),
            "Pressing Escape in the editor should not drop editor focus"
        );
        assert_eq!(
            get_sel(&ctx),
            Some((0, 5)),
            "Pressing Escape in the editor should keep the selection"
        );

        let mut f3_input = egui::RawInput::default();
        f3_input.events.push(egui::Event::Key {
            key: egui::Key::F3,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });

        // Press F3 -> second occurrence at char index 11..16
        let _ = ctx.run(f3_input.clone(), |ctx| {
            app.update_ui(ctx);
        });
        assert_eq!(get_sel(&ctx), Some((11, 16)), "Second F3 should select second 'alpha'");

        // Press F3 again -> third occurrence at char index 23..28
        let _ = ctx.run(f3_input.clone(), |ctx| {
            app.update_ui(ctx);
        });
        assert_eq!(get_sel(&ctx), Some((23, 28)), "Third F3 should select third 'alpha'");

        // Press F3 again -> wraps around to first occurrence at 0..5
        let _ = ctx.run(f3_input, |ctx| {
            app.update_ui(ctx);
        });
        assert_eq!(get_sel(&ctx), Some((0, 5)), "Fourth F3 should wrap around to first 'alpha'");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_unsaved_state_asterisk_and_file_switching() {
        let temp_dir = std::env::temp_dir().join(format!("rusted_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let file_a = temp_dir.join("a.rs");
        let file_b = temp_dir.join("b.rs");
        fs::write(&file_a, "fn a() {}\n").unwrap();
        fs::write(&file_b, "fn b() {}\n").unwrap();

        let ctx = egui::Context::default();
        let mut app = new_test_app(&temp_dir);

        // 1. Initially no file is open -> Save button cannot save
        assert!(!app.can_save());

        // 2. Open file_a -> clean, Save button disabled
        app.open_file(&ctx, file_a.clone());
        assert_eq!(app.text, "fn a() {}\n");
        assert!(!app.can_save());
        assert!(!app.is_file_dirty(&file_a));

        // Focus editor in frame 1
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            app.update_ui(ctx);
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("code_editor")));
        });

        // 3. Edit file_a in frame 2
        let mut input = egui::RawInput::default();
        input.events.push(egui::Event::Text("// edited\n".to_string()));
        let _ = ctx.run(input, |ctx| {
            app.update_ui(ctx);
        });

        assert_eq!(app.text, "// edited\nfn a() {}\n");
        assert!(app.can_save(), "Save should be enabled after editing");
        assert!(app.is_file_dirty(&file_a), "file_a should be marked dirty");
        assert!(!app.is_file_dirty(&file_b), "file_b should still be clean");

        // Verify FileNode has the asterisk dirty label
        let root = app.tree_root.as_ref().unwrap();
        let node_a = root.children.iter().find(|n| n.path == file_a).unwrap();
        assert_eq!(node_a.dirty_label, "a.rs *");

        // 4. Switch to file_b without saving file_a
        app.open_file(&ctx, file_b.clone());
        assert_eq!(app.text, "fn b() {}\n");
        assert!(!app.can_save(), "Save should be disabled on unmodified file_b");
        assert!(
            app.is_file_dirty(&file_a),
            "file_a should remain dirty in state after switching to file_b"
        );
        assert!(!app.is_file_dirty(&file_b));

        // 5. Switch back to file_a -> unsaved content is restored and Save is enabled
        app.open_file(&ctx, file_a.clone());
        assert_eq!(app.text, "// edited\nfn a() {}\n");
        assert!(app.can_save(), "Save should be enabled when returning to unsaved file_a");
        assert!(app.is_file_dirty(&file_a));

        // 6. Save file_a -> written to disk, dirty cleared, Save disabled
        app.save_current_file();
        assert!(!app.can_save(), "Save should be disabled immediately after saving");
        assert!(!app.is_file_dirty(&file_a), "file_a should no longer be dirty after saving");
        assert_eq!(fs::read_to_string(&file_a).unwrap(), "// edited\nfn a() {}\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_tree_new_folder_new_file_rename_and_f2() {
        let temp_dir =
            std::env::temp_dir().join(format!("rusted_tree_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let sub_dir = temp_dir.join("src_dir");
        fs::create_dir_all(&sub_dir).unwrap();
        let initial_file = temp_dir.join("hello.txt");
        fs::write(&initial_file, "hello").unwrap();

        let ctx = egui::Context::default();
        let mut app = new_test_app(&temp_dir);

        // 1. Create a subfolder inside `src_dir` via New Folder action
        app.handle_tree_action(&ctx, TreeAction::StartNewFolder(sub_dir.clone()));
        if let Some(ref mut edit) = app.pending_tree_edit {
            edit.input_text = "nested".to_string();
        }
        let edit = app.pending_tree_edit.take().unwrap();
        app.commit_tree_edit(&ctx, edit);

        let nested_dir = sub_dir.join("nested");
        assert!(nested_dir.is_dir(), "New subfolder should exist on disk");
        assert_eq!(app.selected_node, Some((nested_dir.clone(), true)));

        // 2. Create a new file inside `nested` via New File action
        app.handle_tree_action(&ctx, TreeAction::StartNewFile(nested_dir.clone()));
        if let Some(ref mut edit) = app.pending_tree_edit {
            edit.input_text = "main.rs".to_string();
        }
        let edit = app.pending_tree_edit.take().unwrap();
        app.commit_tree_edit(&ctx, edit);

        let new_file = nested_dir.join("main.rs");
        assert!(new_file.is_file(), "New file should exist on disk");
        assert_eq!(Path::new(&app.path), new_file.as_path());
        assert_eq!(app.selected_node, Some((new_file.clone(), false)));

        // 3. Edit the open file (so it has unsaved state) and press F2 to rename it
        app.text = "fn main() {}".to_string();
        app.is_current_dirty = true;

        let mut f2_input = egui::RawInput::default();
        f2_input.events.push(egui::Event::Key {
            key: egui::Key::F2,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run(f2_input, |ctx| {
            app.update_ui(ctx);
        });

        assert!(
            app.pending_tree_edit.is_some(),
            "Pressing F2 should start renaming the selected node"
        );
        assert_eq!(
            app.pending_tree_edit.as_ref().unwrap().input_text,
            "main.rs"
        );

        // Type new name and press Enter to commit rename
        if let Some(ref mut edit) = app.pending_tree_edit {
            edit.input_text = "lib.rs".to_string();
        }
        let mut enter_input = egui::RawInput::default();
        enter_input.events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        let _ = ctx.run(enter_input, |ctx| {
            app.update_ui(ctx);
        });

        let renamed_file = nested_dir.join("lib.rs");
        assert!(!new_file.exists());
        assert!(renamed_file.is_file());
        assert_eq!(Path::new(&app.path), renamed_file.as_path());
        assert_eq!(app.file_name, "lib.rs");
        assert!(app.is_file_dirty(&renamed_file));

        // 4. Select the parent folder `src_dir`, press F2, and rename it to `app_dir`
        app.selected_node = Some((sub_dir.clone(), true));
        app.start_rename_selected();
        if let Some(ref mut edit) = app.pending_tree_edit {
            edit.input_text = "app_dir".to_string();
        }
        let edit = app.pending_tree_edit.take().unwrap();
        app.commit_tree_edit(&ctx, edit);

        let renamed_dir = temp_dir.join("app_dir");
        let moved_file = renamed_dir.join("nested").join("lib.rs");
        assert!(renamed_dir.is_dir());
        assert!(moved_file.is_file());
        assert_eq!(
            Path::new(&app.path),
            moved_file.as_path(),
            "Active file path should update when an ancestor folder is renamed"
        );

        // 5. Delete the folder via DeleteNode action (moves to Recycle Bin / Trash)
        app.handle_tree_action(
            &ctx,
            TreeAction::DeleteNode {
                path: renamed_dir.clone(),
                is_dir: true,
            },
        );
        assert!(!renamed_dir.exists(), "Deleted folder should be moved to trash");
        assert!(
            app.path.is_empty(),
            "Active editor path should be cleared when its folder is deleted"
        );
        assert!(!app.can_save());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
