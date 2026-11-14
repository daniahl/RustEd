//! Project folder and file explorer sidebar.
//!
//! Manages the directory tree hierarchy, lazy-loading child directories, inline renaming and creation
//! of files and folders, context menus, and trash/recycle bin operations.

use crate::{
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
}

#[cfg(target_os = "windows")]
pub fn move_to_trash(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[allow(clippy::upper_case_acronyms)]
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
                        // Direct child directory created/deleted/renamed: refresh parent if missing
                        // on disk, or refresh the directory itself if loaded.
                        if !child.path.exists() {
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
                    // Existing file child changed or was deleted; reload parent only if deleted/new
                    // metadata changed directory listing.
                    if (!child.path.exists() || child.path.is_dir())
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
                        *tree_action = Some(TreeAction::DeleteNode {
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
        let current_path = Path::new(&doc.path);
        let is_selected = match selected_node.as_ref() {
            Some((sel_path, _)) => paths_equal(sel_path, &node.path),
            None => !doc.path.is_empty() && paths_equal(current_path, &node.path),
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
                    *tree_action = Some(TreeAction::DeleteNode {
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
    pub folder_path: String,
    pub tree_root: Option<FileNode>,
    pub selected_node: Option<(PathBuf, bool)>,
    pub pending_tree_edit: Option<PendingTreeEdit>,
    label_buf: String,
}

impl FileExplorer {
    pub fn new(folder: PathBuf) -> Self {
        let folder_path = folder.to_string_lossy().into_owned();
        let tree_root = Some(FileNode::new_root(folder));
        Self {
            folder_path,
            tree_root,
            selected_node: None,
            pending_tree_edit: None,
            label_buf: String::with_capacity(64),
        }
    }

    pub fn open_folder(&mut self, path: PathBuf) {
        self.pending_tree_edit = None;
        self.selected_node = None;
        self.folder_path = path.to_string_lossy().into_owned();
        self.tree_root = Some(FileNode::new_root(path));
    }

    pub fn open_parent_folder(&mut self) {
        let p = PathBuf::from(&self.folder_path);
        if let Some(parent) = p.parent() {
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
            return false;
        }

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

    pub fn create_file_in(
        &mut self,
        ctx: &egui::Context,
        doc: &mut Document,
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

    pub fn show_sidebar(&mut self, ui: &mut egui::Ui, doc: &mut Document) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        egui::Panel::left("explorer_sidebar")
            .resizable(true)
            .default_size(220.0)
            .min_size(140.0)
            .show(ui, |ui| {
                if self.pending_tree_edit.is_none() && ui.input(|i| i.key_pressed(egui::Key::F2)) {
                    self.start_rename_selected();
                }

                let mut file_to_open: Option<PathBuf> = None;
                let mut tree_action: Option<TreeAction> = None;
                let mut edit_outcome: Option<TreeEditOutcome> = None;
                let selected_node = &mut self.selected_node;
                let pending_edit = &mut self.pending_tree_edit;
                let label_buf = &mut self.label_buf;

                let area_bottom = ui.max_rect().bottom();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
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
                    self.selected_node = Some((chosen.clone(), false));
                    doc.open_file(ctx, chosen);
                }
            });
    }
}
