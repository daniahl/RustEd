//! Project folder and file explorer sidebar.
//!
//! Manages the directory tree hierarchy, lazy-loading child directories, inline renaming and creation
//! of files and folders, context menus, and trash/recycle bin operations.

use crate::{
    app::{SIDEBAR_DEFAULT_WIDTH, SIDEBAR_MIN_WIDTH},
    document::Document,
    watcher::{path_starts_with, path_strip_prefix, paths_equal},
};
use eframe::egui;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, PartialEq, Eq)]
pub enum TreeEditKind {
    NewFolder { parent_path: PathBuf },
    NewFile { parent_path: PathBuf },
    Rename { target_path: PathBuf, is_dir: bool },
}

#[derive(Clone)]
pub struct PendingTreeEdit {
    pub kind: TreeEditKind,
    pub input_text: String,
    pub had_focus: bool,
}

pub enum TreeAction {
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
    /// UI-triggered delete that asks for confirmation when unsaved in-memory edits would be lost.
    RequestDeleteNode {
        path: PathBuf,
        is_dir: bool,
    },
}

/// Validates a file or directory name for creation/renaming.
///
/// Returns `Ok(trimmed)` if valid, `Err(None)` if empty (silent cancel), or `Err(Some(msg))`
/// when invalid characters, trailing dots/spaces, or reserved Windows device names are detected.
pub fn validate_entry_name(raw_name: &str) -> Result<&str, Option<String>> {
    let name = raw_name.trim();
    if name.is_empty() {
        return Err(None);
    }
    if name == "." || name == ".." {
        return Err(Some(format!("Invalid name: {name}")));
    }
    if name.contains(['/', '\\']) {
        return Err(Some(format!(
            "Name cannot contain path separators: {name}"
        )));
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20)
    {
        return Err(Some(format!("Name contains invalid characters: {name}")));
    }
    if name.ends_with('.') {
        return Err(Some(format!("Name cannot end with a dot: {name}")));
    }
    // Check Windows reserved device names (with or without extension, e.g. `CON`, `NUL.txt`).
    let stem = name.split('.').next().unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    let is_reserved = matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    if is_reserved {
        return Err(Some(format!("Reserved system name: {name}")));
    }
    Ok(name)
}

#[cfg(target_os = "windows")]
pub fn move_to_trash(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };

    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        return false;
    };

    let mut path_str = abs_path.to_string_lossy().into_owned();
    if let Some(stripped) = path_str.strip_prefix(r"\\?\UNC\") {
        path_str = format!(r"\\{stripped}");
    } else if let Some(stripped) = path_str.strip_prefix(r"\\?\") {
        path_str = stripped.to_string();
    }
    while path_str.len() > 3 && (path_str.ends_with('\\') || path_str.ends_with('/')) {
        path_str.pop();
    }

    let wide: Vec<u16> = std::ffi::OsStr::new(&path_str)
        .encode_wide()
        .chain([0, 0])
        .collect();

    let mut file_op = SHFILEOPSTRUCTW {
        hwnd: Default::default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(wide.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
        fAnyOperationsAborted: false.into(),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };

    let ret = unsafe { SHFileOperationW(&mut file_op) };
    ret == 0 && !file_op.fAnyOperationsAborted.as_bool()
}

#[cfg(not(target_os = "windows"))]
pub fn move_to_trash(path: &Path) -> bool {
    if path.is_dir() {
        fs::remove_dir_all(path).is_ok()
    } else {
        fs::remove_file(path).is_ok()
    }
}

pub enum TreeEditOutcome {
    Commit,
    Cancel,
}

pub struct FileNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
    pub children_loaded: bool,
    pub children: Vec<FileNode>,
}

impl FileNode {
    pub fn new_root(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or("root"))
            .to_string();
        let mut node = Self {
            name,
            path,
            is_dir: true,
            expanded: true,
            children_loaded: false,
            children: Vec::new(),
        };
        node.load_children();
        node
    }

    pub fn find_mut(&mut self, target: &Path) -> Option<&mut FileNode> {
        if paths_equal(&self.path, target) {
            return Some(self);
        }
        for child in &mut self.children {
            if child.is_dir && !path_starts_with(target, &child.path) {
                continue;
            }
            if let Some(found) = child.find_mut(target) {
                return Some(found);
            }
        }
        None
    }

    /// Collects visible child entries `(path, is_dir)` in display order for keyboard navigation.
    pub(crate) fn collect_visible_entries(&self, out: &mut Vec<(PathBuf, bool)>) {
        if !self.is_dir || !self.expanded {
            return;
        }
        for child in &self.children {
            out.push((child.path.clone(), child.is_dir));
            if child.is_dir && child.expanded {
                child.collect_visible_entries(out);
            }
        }
    }

    pub fn visible_count(&self) -> usize {
        if !self.is_dir || !self.expanded {
            return 1;
        }
        let mut total = 1 + self.children.len();
        for child in &self.children {
            if !child.is_dir {
                break;
            }
            if child.expanded {
                total += child.visible_count() - 1;
            }
        }
        total
    }

    pub fn load_children(&mut self) {
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
                let (expanded, children_loaded, children) = if is_dir {
                    old_dirs
                        .remove(&path)
                        .or_else(|| {
                            let key = old_dirs.keys().find(|k| paths_equal(k, &path)).cloned()?;
                            old_dirs.remove(&key)
                        })
                        .unwrap_or((false, false, Vec::new()))
                } else {
                    (false, false, Vec::new())
                };
                self.children.push(Self {
                    name,
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
                    let a_bytes = a.name.as_bytes();
                    let b_bytes = b.name.as_bytes();
                    let min_len = a_bytes.len().min(b_bytes.len());
                    for i in 0..min_len {
                        let ca = a_bytes[i].to_ascii_lowercase();
                        let cb = b_bytes[i].to_ascii_lowercase();
                        if ca != cb {
                            return ca.cmp(&cb);
                        }
                    }
                    a_bytes.len().cmp(&b_bytes.len()).then_with(|| a.name.cmp(&b.name))
                })
            });
        }
    }

    /// Recursively refreshes all already-loaded directories in this subtree while leaving
    /// collapsed/unloaded directories lazy.
    pub fn refresh_loaded_subtree(&mut self) {
        if !self.is_dir || !self.children_loaded {
            return;
        }
        self.load_children();
        for child in &mut self.children {
            if child.is_dir && child.children_loaded {
                child.refresh_loaded_subtree();
            }
        }
    }

    /// Walks down loaded directories toward `changed_path` and refreshes the deepest loaded
    /// directory that actually owns the visible change, avoiding scans inside collapsed folders.
    pub fn refresh_for_path(&mut self, changed_path: &Path, refreshed: &mut Vec<PathBuf>) {
        if !self.is_dir || !self.children_loaded || !path_starts_with(changed_path, &self.path) {
            return;
        }
        if paths_equal(changed_path, &self.path) {
            if !refreshed.iter().any(|d| paths_equal(d, &self.path)) {
                self.load_children();
                refreshed.push(self.path.clone());
            }
            return;
        }

        let matching_idx = self
            .children
            .iter()
            .position(|c| path_starts_with(changed_path, &c.path));

        match matching_idx {
            Some(idx) => {
                let child = &self.children[idx];
                if child.is_dir {
                    if paths_equal(changed_path, &child.path) {
                        // Direct child directory created/deleted/renamed/replaced: refresh parent
                        // if missing, replaced by a file, or case-renamed; else refresh if loaded.
                        let name_changed = changed_path
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy() != child.name);
                        if !child.path.exists() || !child.path.is_dir() || name_changed {
                            if !refreshed.iter().any(|d| paths_equal(d, &self.path)) {
                                self.load_children();
                                refreshed.push(self.path.clone());
                            }
                        } else if child.children_loaded {
                            self.children[idx].refresh_for_path(changed_path, refreshed);
                        }
                    } else if child.children_loaded {
                        self.children[idx].refresh_for_path(changed_path, refreshed);
                    }
                    // If !child.children_loaded and changed_path is strictly inside it (e.g.
                    // `target\debug\...`), skip reloading so unexpanded trees stay fast.
                } else {
                    // Existing file child changed, deleted, replaced by a directory, or case-renamed.
                    let name_changed = paths_equal(changed_path, &child.path)
                        && changed_path
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy() != child.name);
                    if (!child.path.exists() || child.path.is_dir() || name_changed)
                        && !refreshed.iter().any(|d| paths_equal(d, &self.path))
                    {
                        self.load_children();
                        refreshed.push(self.path.clone());
                    }
                }
            }
            None => {
                // Missing direct child (newly created file/folder, or first level of `mkdir a\b`).
                if !refreshed.iter().any(|d| paths_equal(d, &self.path)) {
                    self.load_children();
                    refreshed.push(self.path.clone());
                }
            }
        }
    }
}

pub fn render_inline_tree_edit(
    ui: &mut egui::Ui,
    edit: &mut PendingTreeEdit,
    edit_outcome: &mut Option<TreeEditOutcome>,
) {
    let edit_id = egui::Id::unique("tree_inline_editor");
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
pub fn render_file_node(
    ui: &mut egui::Ui,
    node: &mut FileNode,
    doc: &Document,
    selected_node: &mut Option<(PathBuf, bool)>,
    pending_edit: &mut Option<PendingTreeEdit>,
    tree_action: &mut Option<TreeAction>,
    edit_outcome: &mut Option<TreeEditOutcome>,
    depth: usize,
    file_to_open: &mut Option<PathBuf>,
) {
    let mut label_buf = String::with_capacity(64);
    render_file_node_with_buf(
        ui,
        node,
        doc,
        selected_node,
        pending_edit,
        tree_action,
        edit_outcome,
        depth,
        file_to_open,
        &mut label_buf,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_file_node_with_buf(
    ui: &mut egui::Ui,
    node: &mut FileNode,
    doc: &Document,
    selected_node: &mut Option<(PathBuf, bool)>,
    pending_edit: &mut Option<PendingTreeEdit>,
    tree_action: &mut Option<TreeAction>,
    edit_outcome: &mut Option<TreeEditOutcome>,
    depth: usize,
    file_to_open: &mut Option<PathBuf>,
    label_buf: &mut String,
) {
    let indent = depth as f32 * 14.0;
    let is_renaming_self = matches!(
        pending_edit,
        Some(e) if matches!(&e.kind, TreeEditKind::Rename { target_path, .. } if paths_equal(target_path, &node.path))
    );

    let has_pending_edit = match pending_edit {
        Some(e) => match &e.kind {
            TreeEditKind::NewFolder { parent_path } | TreeEditKind::NewFile { parent_path } => {
                path_starts_with(parent_path, &node.path)
            }
            TreeEditKind::Rename { target_path, .. } => path_starts_with(target_path, &node.path),
        },
        None => false,
    };

    let clip_rect = ui.clip_rect();
    let row_height = ui.spacing().interact_size.y.max(18.0) + ui.spacing().item_spacing.y;
    let cursor_y = ui.cursor().min.y;

    // Fast-path: if this node (and its entire subtree) is completely below the visible viewport,
    // advance cursor by the total height of its visible rows and return immediately.
    if !has_pending_edit && cursor_y > clip_rect.max.y {
        let count = node.visible_count();
        ui.add_space(count as f32 * row_height);
        return;
    }

    let row_above_clip =
        !has_pending_edit && !is_renaming_self && (cursor_y + row_height < clip_rect.min.y);

    if node.is_dir {
        if is_renaming_self {
            ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.label(if node.expanded { "▾" } else { "▸" });
                if let Some(edit) = pending_edit.as_mut() {
                    render_inline_tree_edit(ui, edit, edit_outcome);
                }
            });
        } else if row_above_clip {
            ui.add_space(row_height);
        } else {
            let is_selected = selected_node
                .as_ref()
                .map(|(p, _)| paths_equal(p, &node.path))
                .unwrap_or(false);
            ui.horizontal(|ui| {
                ui.add_space(indent);
                let prefix = if node.expanded { "▾ " } else { "▸ " };
                label_buf.clear();
                label_buf.push_str(prefix);
                label_buf.push_str(&node.name);
                let resp = ui.selectable_label(is_selected, label_buf.as_str());
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
                        ui.close();
                    }
                    if ui.button("New File").clicked() {
                        *tree_action = Some(TreeAction::StartNewFile(node.path.clone()));
                        ui.close();
                    }
                    if ui.button("Rename").clicked() {
                        *tree_action = Some(TreeAction::StartRename {
                            path: node.path.clone(),
                            current_name: node.name.clone(),
                            is_dir: true,
                        });
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        *tree_action = Some(TreeAction::RequestDeleteNode {
                            path: node.path.clone(),
                            is_dir: true,
                        });
                        ui.close();
                    }
                });
            });
        }

        if node.expanded {
            if let Some(edit) = pending_edit.as_mut() {
                match &edit.kind {
                    TreeEditKind::NewFolder { parent_path }
                        if paths_equal(parent_path, &node.path) =>
                    {
                        ui.horizontal(|ui| {
                            ui.add_space((depth + 1) as f32 * 14.0);
                            ui.label("▸");
                            render_inline_tree_edit(ui, edit, edit_outcome);
                        });
                    }
                    TreeEditKind::NewFile { parent_path }
                        if paths_equal(parent_path, &node.path) =>
                    {
                        ui.horizontal(|ui| {
                            ui.add_space((depth + 1) as f32 * 14.0 + 14.0);
                            render_inline_tree_edit(ui, edit, edit_outcome);
                        });
                    }
                    _ => {}
                }
            }

            for child in &mut node.children {
                render_file_node_with_buf(
                    ui,
                    child,
                    doc,
                    selected_node,
                    pending_edit,
                    tree_action,
                    edit_outcome,
                    depth + 1,
                    file_to_open,
                    label_buf,
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
    } else if row_above_clip {
        ui.add_space(row_height);
    } else {
        let current_path = &doc.path;
        let is_selected = match selected_node.as_ref() {
            Some((sel_path, _)) => paths_equal(sel_path, &node.path),
            None => !doc.path.as_os_str().is_empty() && paths_equal(current_path, &node.path),
        };
        let is_dirty = doc.is_file_dirty(&node.path);
        ui.horizontal(|ui| {
            ui.add_space(indent + 14.0);
            let resp = if is_dirty {
                label_buf.clear();
                label_buf.push_str(&node.name);
                label_buf.push_str(" *");
                ui.selectable_label(is_selected, label_buf.as_str())
            } else {
                ui.selectable_label(is_selected, &node.name)
            };
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
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    *tree_action = Some(TreeAction::RequestDeleteNode {
                        path: node.path.clone(),
                        is_dir: false,
                    });
                    ui.close();
                }
            });
        });
    }
}

pub struct FileExplorer {
    pub folder_path: PathBuf,
    pub tree_root: Option<FileNode>,
    pub selected_node: Option<(PathBuf, bool)>,
    pub pending_tree_edit: Option<PendingTreeEdit>,
    /// Confirmation target `(path, is_dir)` when deleting an item with unsaved edits.
    pub pending_delete: Option<(PathBuf, bool)>,
    /// File path waiting to open after confirming what to do with a dirty `Untitled` buffer.
    pub pending_open_after_untitled: Option<PathBuf>,
    pub last_fs_error: Option<String>,
    label_buf: String,
}

impl FileExplorer {
    pub fn new(folder: PathBuf) -> Self {
        let tree_root = Some(FileNode::new_root(folder.clone()));
        Self {
            folder_path: folder,
            tree_root,
            selected_node: None,
            pending_tree_edit: None,
            pending_delete: None,
            pending_open_after_untitled: None,
            last_fs_error: None,
            label_buf: String::with_capacity(64),
        }
    }

    pub fn open_folder(&mut self, path: PathBuf) {
        self.pending_tree_edit = None;
        self.pending_delete = None;
        self.selected_node = None;
        self.last_fs_error = None;
        self.folder_path = path.clone();
        self.tree_root = Some(FileNode::new_root(path));
    }

    pub fn open_parent_folder(&mut self) {
        if let Some(parent) = self.folder_path.parent() {
            if !parent.as_os_str().is_empty() {
                self.open_folder(parent.to_path_buf());
            }
        }
    }

    /// Updates loaded explorer tree nodes for a batch of filesystem watcher events while leaving
    /// unexpanded subdirectories lazy and preserving active inline edits unless their target vanished.
    pub fn refresh_watched_paths(&mut self, paths: &[PathBuf], rescan_all: bool) {
        if let Some(root) = &mut self.tree_root {
            if rescan_all {
                root.refresh_loaded_subtree();
            } else {
                let mut refreshed = Vec::new();
                for path in paths {
                    root.refresh_for_path(path, &mut refreshed);
                }
            }
        }

        if let Some((ref sel_path, _)) = self.selected_node {
            if !sel_path.exists() {
                self.selected_node = None;
            }
        }

        if let Some(ref edit) = self.pending_tree_edit {
            let missing = match &edit.kind {
                TreeEditKind::NewFolder { parent_path }
                | TreeEditKind::NewFile { parent_path } => !parent_path.exists(),
                TreeEditKind::Rename { target_path, .. } => !target_path.exists(),
            };
            if missing {
                self.pending_tree_edit = None;
            }
        }
    }

    pub fn start_rename_selected(&mut self) {
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

    pub fn handle_tree_action(
        &mut self,
        ctx: &egui::Context,
        doc: &mut Document,
        action: TreeAction,
    ) {
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
                let _ = self.delete_node(ctx, doc, &path, is_dir);
            }
            TreeAction::RequestDeleteNode { path, is_dir } => {
                if doc.has_dirty_in_path(&path, is_dir) {
                    self.pending_delete = Some((path, is_dir));
                } else {
                    let _ = self.delete_node(ctx, doc, &path, is_dir);
                }
            }
        }
    }

    pub fn delete_node(
        &mut self,
        ctx: &egui::Context,
        doc: &mut Document,
        path: &Path,
        is_dir: bool,
    ) -> bool {
        if !move_to_trash(path) {
            self.last_fs_error = Some(format!("Failed to delete {}", path.display()));
            return false;
        }
        self.last_fs_error = None;

        if let Some((ref sel_path, _)) = self.selected_node {
            if paths_equal(sel_path, path) || (is_dir && path_starts_with(sel_path, path)) {
                self.selected_node = None;
            }
        }

        if let Some(ref edit) = self.pending_tree_edit {
            let matches_deleted = match &edit.kind {
                TreeEditKind::NewFolder { parent_path }
                | TreeEditKind::NewFile { parent_path } => {
                    paths_equal(parent_path, path)
                        || (is_dir && path_starts_with(parent_path, path))
                }
                TreeEditKind::Rename { target_path, .. } => {
                    paths_equal(target_path, path)
                        || (is_dir && path_starts_with(target_path, path))
                }
            };
            if matches_deleted {
                self.pending_tree_edit = None;
            }
        }

        doc.close_if_deleted(ctx, path, is_dir);

        if let Some(parent) = path.parent() {
            if let Some(root) = &mut self.tree_root {
                if let Some(parent_node) = root.find_mut(parent) {
                    parent_node.load_children();
                }
            }
        }

        true
    }

    pub fn create_folder_in(&mut self, parent_path: &Path, name: &str) -> Option<PathBuf> {
        let name = match validate_entry_name(name) {
            Ok(valid) => valid,
            Err(err_msg) => {
                if let Some(msg) = err_msg {
                    self.last_fs_error = Some(msg);
                }
                return None;
            }
        };
        let new_path = parent_path.join(name);
        if let Err(err) = fs::create_dir(&new_path) {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                self.last_fs_error = Some(format!("Folder already exists: {name}"));
            } else {
                self.last_fs_error = Some(format!("Failed to create folder {name}: {err}"));
            }
            return None;
        }
        self.last_fs_error = None;
        if let Some(root) = &mut self.tree_root {
            if let Some(parent_node) = root.find_mut(parent_path) {
                parent_node.expanded = true;
                parent_node.load_children();
            }
        }
        self.selected_node = Some((new_path.clone(), true));
        Some(new_path)
    }

    pub fn create_file_in(
        &mut self,
        ctx: &egui::Context,
        doc: &mut Document,
        parent_path: &Path,
        name: &str,
    ) -> Option<PathBuf> {
        let name = match validate_entry_name(name) {
            Ok(valid) => valid,
            Err(err_msg) => {
                if let Some(msg) = err_msg {
                    self.last_fs_error = Some(msg);
                }
                return None;
            }
        };
        let new_path = parent_path.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_path)
        {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                self.last_fs_error = Some(format!("File already exists: {name}"));
                return None;
            }
            Err(err) => {
                self.last_fs_error = Some(format!("Failed to create file {name}: {err}"));
                return None;
            }
        }
        self.last_fs_error = None;
        if let Some(root) = &mut self.tree_root {
            if let Some(parent_node) = root.find_mut(parent_path) {
                parent_node.expanded = true;
                parent_node.load_children();
            }
        }
        self.selected_node = Some((new_path.clone(), false));
        doc.open_file(ctx, new_path.clone());
        Some(new_path)
    }

    pub fn rename_node(
        &mut self,
        doc: &mut Document,
        old_path: &Path,
        new_name: &str,
        is_dir: bool,
    ) -> Option<PathBuf> {
        let new_name = match validate_entry_name(new_name) {
            Ok(valid) => valid,
            Err(err_msg) => {
                if let Some(msg) = err_msg {
                    self.last_fs_error = Some(msg);
                }
                return None;
            }
        };
        let parent = old_path.parent()?;
        let new_path = parent.join(new_name);
        if paths_equal(&new_path, old_path) {
            if new_path == old_path {
                return None;
            }
        } else if new_path.exists() {
            self.last_fs_error = Some(format!("Target already exists: {new_name}"));
            return None;
        }
        if let Err(err) = fs::rename(old_path, &new_path) {
            self.last_fs_error = Some(format!("Failed to rename to {new_name}: {err}"));
            return None;
        }
        self.last_fs_error = None;

        if let Some((ref mut sel_path, _)) = self.selected_node {
            if paths_equal(sel_path, old_path) {
                *sel_path = new_path.clone();
            } else if is_dir {
                if let Some(rel) = path_strip_prefix(sel_path, old_path) {
                    *sel_path = new_path.join(rel);
                }
            }
        }

        doc.update_path_if_renamed(old_path, &new_path, is_dir);

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

    pub fn commit_tree_edit(
        &mut self,
        ctx: &egui::Context,
        doc: &mut Document,
        edit: PendingTreeEdit,
    ) {
        match edit.kind {
            TreeEditKind::NewFolder { parent_path } => {
                let _ = self.create_folder_in(&parent_path, &edit.input_text);
            }
            TreeEditKind::NewFile { parent_path } => {
                let _ = self.create_file_in(ctx, doc, &parent_path, &edit.input_text);
            }
            TreeEditKind::Rename {
                target_path,
                is_dir,
            } => {
                let _ = self.rename_node(doc, &target_path, &edit.input_text, is_dir);
            }
        }
    }

    fn show_confirmation_dialogs(&mut self, ctx: &egui::Context, doc: &mut Document) {
        if let Some((path, is_dir)) = self.pending_delete.clone() {
            let mut open = true;
            let mut confirm_delete = false;
            let mut cancel = false;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());

            egui::Window::new("Confirm Delete")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "\"{name}\" has unsaved edits in memory that will be discarded if deleted."
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Delete & Discard Edits").clicked() {
                            confirm_delete = true;
                        }
                        if ui.button("Cancel").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            cancel = true;
                        }
                    });
                });

            if confirm_delete {
                self.pending_delete = None;
                let _ = self.delete_node(ctx, doc, &path, is_dir);
                ctx.request_repaint();
            } else if cancel || !open {
                self.pending_delete = None;
            }
        }

        if let Some(target_file) = self.pending_open_after_untitled.clone() {
            let mut open = true;
            let mut save_and_open = false;
            let mut discard_and_open = false;
            let mut cancel = false;

            egui::Window::new("Unsaved Untitled Buffer")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label("Untitled has unsaved changes. Save before opening another file?");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save As & Open").clicked() {
                            save_and_open = true;
                        }
                        if ui.button("Discard & Open").clicked() {
                            discard_and_open = true;
                        }
                        if ui.button("Cancel").clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Escape))
                        {
                            cancel = true;
                        }
                    });
                });

            if save_and_open {
                if let Some(dest) = crate::app::pick_save_file(&self.folder_path) {
                    if doc.save_current_file_as(dest.clone()) {
                        self.refresh_watched_paths(&[dest], false);
                        self.pending_open_after_untitled = None;
                        self.selected_node = Some((target_file.clone(), false));
                        doc.open_file(ctx, target_file);
                    } else {
                        self.pending_open_after_untitled = None;
                    }
                }
                ctx.request_repaint();
            } else if discard_and_open {
                self.pending_open_after_untitled = None;
                self.selected_node = Some((target_file.clone(), false));
                doc.open_file(ctx, target_file);
                ctx.request_repaint();
            } else if cancel || !open {
                self.pending_open_after_untitled = None;
            }
        }
    }

    fn handle_keyboard_navigation(
        &mut self,
        ui: &egui::Ui,
        tree_action: &mut Option<TreeAction>,
        file_to_open: &mut Option<PathBuf>,
    ) {
        let (f2, up, down, left, right, enter, del) = ui.input(|i| {
            let no_mods = !i.modifiers.ctrl && !i.modifiers.alt && !i.modifiers.command;
            if !no_mods {
                return (false, false, false, false, false, false, false);
            }
            (
                i.key_pressed(egui::Key::F2),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Delete),
            )
        });

        if f2 {
            self.start_rename_selected();
            return;
        }
        if del {
            if let Some((path, is_dir)) = self.selected_node.clone() {
                *tree_action = Some(TreeAction::RequestDeleteNode { path, is_dir });
            }
            return;
        }
        if !(up || down || left || right || enter) {
            return;
        }

        let Some(root) = self.tree_root.as_mut() else {
            return;
        };
        let mut visible = Vec::new();
        root.collect_visible_entries(&mut visible);
        if visible.is_empty() {
            return;
        }

        let cur_idx = self
            .selected_node
            .as_ref()
            .and_then(|(p, _)| visible.iter().position(|(vp, _)| paths_equal(vp, p)));

        if down {
            let next = match cur_idx {
                Some(i) => (i + 1).min(visible.len() - 1),
                None => 0,
            };
            self.selected_node = Some(visible[next].clone());
        } else if up {
            let prev = match cur_idx {
                Some(i) => i.saturating_sub(1),
                None => visible.len() - 1,
            };
            self.selected_node = Some(visible[prev].clone());
        } else if let Some((sel_path, is_dir)) = self.selected_node.clone() {
            if right && is_dir {
                if let Some(node) = root.find_mut(&sel_path) {
                    if !node.expanded {
                        node.expanded = true;
                        if !node.children_loaded {
                            node.load_children();
                        }
                    } else if let Some(first_child) = node.children.first() {
                        self.selected_node =
                            Some((first_child.path.clone(), first_child.is_dir));
                    }
                }
            } else if left {
                let collapsed = if is_dir {
                    if let Some(node) = root.find_mut(&sel_path) {
                        if node.expanded {
                            node.expanded = false;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !collapsed {
                    if let Some(parent) = sel_path.parent() {
                        if !paths_equal(parent, &root.path) && path_starts_with(parent, &root.path)
                        {
                            self.selected_node = Some((parent.to_path_buf(), true));
                        }
                    }
                }
            } else if enter {
                if is_dir {
                    if let Some(node) = root.find_mut(&sel_path) {
                        node.expanded = !node.expanded;
                        if node.expanded && !node.children_loaded {
                            node.load_children();
                        }
                    }
                } else {
                    *file_to_open = Some(sel_path);
                }
            }
        }
    }

    pub fn show_sidebar(&mut self, ui: &mut egui::Ui, doc: &mut Document) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.show_confirmation_dialogs(ctx, doc);

        egui::Panel::left("explorer_sidebar")
            .resizable(true)
            .default_size(SIDEBAR_DEFAULT_WIDTH)
            .min_size(SIDEBAR_MIN_WIDTH)
            .show(ui, |ui| {
                if let Some(err_msg) = self.last_fs_error.as_deref() {
                    let mut clear_err = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(ui.visuals().error_fg_color, err_msg);
                        if ui.small_button("Dismiss").clicked() {
                            clear_err = true;
                        }
                    });
                    ui.separator();
                    if clear_err {
                        self.last_fs_error = None;
                    }
                }

                let mut file_to_open: Option<PathBuf> = None;
                let mut tree_action: Option<TreeAction> = None;
                let mut edit_outcome: Option<TreeEditOutcome> = None;
                let mut navigate_parent = false;

                let sidebar_keys_active = !doc.find_open
                    && self.pending_tree_edit.is_none()
                    && self.pending_delete.is_none()
                    && self.pending_open_after_untitled.is_none()
                    && !ui.memory(|m| {
                        m.has_focus(crate::app::project_folder_input_id())
                            || m.has_focus(crate::editor::code_editor_id())
                    });
                if sidebar_keys_active {
                    self.handle_keyboard_navigation(ui, &mut tree_action, &mut file_to_open);
                }

                let has_parent = self
                    .folder_path
                    .parent()
                    .is_some_and(|p| !p.as_os_str().is_empty());
                let selected_node = &mut self.selected_node;
                let pending_edit = &mut self.pending_tree_edit;
                let label_buf = &mut self.label_buf;

                let area_bottom = ui.max_rect().bottom();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if has_parent
                            && ui
                                .selectable_label(false, "..")
                                .on_hover_text("Open parent folder")
                                .clicked()
                        {
                            navigate_parent = true;
                        }
                        if let Some(root) = &mut self.tree_root {
                            let root_path = root.path.clone();
                            // Inline name editor for items being created at the project root.
                            if let Some(edit) = pending_edit.as_mut() {
                                match &edit.kind {
                                    TreeEditKind::NewFolder { parent_path }
                                        if paths_equal(parent_path, &root_path) =>
                                    {
                                        ui.horizontal(|ui| {
                                            ui.label("▸");
                                            render_inline_tree_edit(ui, edit, &mut edit_outcome);
                                        });
                                    }
                                    TreeEditKind::NewFile { parent_path }
                                        if paths_equal(parent_path, &root_path) =>
                                    {
                                        ui.horizontal(|ui| {
                                            ui.add_space(14.0);
                                            render_inline_tree_edit(ui, edit, &mut edit_outcome);
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            for child in &mut root.children {
                                render_file_node_with_buf(
                                    ui,
                                    child,
                                    doc,
                                    selected_node,
                                    pending_edit,
                                    &mut tree_action,
                                    &mut edit_outcome,
                                    0,
                                    &mut file_to_open,
                                    label_buf,
                                );
                            }
                            // Empty space below the tree: right-click creates items at the project root.
                            let remaining = (area_bottom - ui.cursor().top()).max(24.0);
                            let empty = ui.allocate_response(
                                egui::vec2(ui.available_width(), remaining),
                                egui::Sense::click(),
                            );
                            if empty.secondary_clicked() {
                                *selected_node = None;
                            }
                            empty.context_menu(|ui| {
                                if ui.button("New Folder").clicked() {
                                    tree_action = Some(TreeAction::StartNewFolder(root_path.clone()));
                                    ui.close();
                                }
                                if ui.button("New File").clicked() {
                                    tree_action = Some(TreeAction::StartNewFile(root_path.clone()));
                                    ui.close();
                                }
                            });
                        }
                    });

                if navigate_parent {
                    self.open_parent_folder();
                    ctx.request_repaint();
                }

                if let Some(outcome) = edit_outcome {
                    if let Some(edit) = self.pending_tree_edit.take() {
                        if matches!(outcome, TreeEditOutcome::Commit) {
                            self.commit_tree_edit(ctx, doc, edit);
                        }
                    }
                    ctx.request_repaint();
                }

                if let Some(action) = tree_action {
                    self.handle_tree_action(ctx, doc, action);
                    ctx.request_repaint();
                }

                if let Some(chosen) = file_to_open {
                    if doc.is_untitled_dirty() {
                        self.pending_open_after_untitled = Some(chosen);
                        ctx.request_repaint();
                    } else {
                        self.selected_node = Some((chosen.clone(), false));
                        doc.open_file(ctx, chosen);
                    }
                }
            });
    }
}
