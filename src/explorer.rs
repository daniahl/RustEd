//! Project folder and file explorer sidebar.
//!
//! Manages the directory tree hierarchy, lazy-loading child directories, inline renaming and creation
//! of files and folders, context menus, and trash/recycle bin operations.

use crate::document::{Document, UnsavedFileState};
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
    pub dirty_label: String,
    pub collapsed_label: String,
    pub expanded_label: String,
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

    pub fn find_mut(&mut self, target: &Path) -> Option<&mut FileNode> {
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

pub fn render_inline_tree_edit(
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
pub fn render_file_node(
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

pub struct FileExplorer {
    pub folder_path: String,
    pub tree_root: Option<FileNode>,
    pub selected_node: Option<(PathBuf, bool)>,
    pub pending_tree_edit: Option<PendingTreeEdit>,
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
            if sel_path == old_path {
                *sel_path = new_path.clone();
            } else if is_dir {
                if let Ok(rel) = sel_path.strip_prefix(old_path) {
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

    pub fn show_sidebar(&mut self, ctx: &egui::Context, doc: &mut Document) {
        egui::SidePanel::left("explorer_sidebar")
            .resizable(true)
            .default_width(220.0)
            .min_width(140.0)
            .show(ctx, |ui| {
                if self.pending_tree_edit.is_none() && ui.input(|i| i.key_pressed(egui::Key::F2)) {
                    self.start_rename_selected();
                }

                let mut file_to_open: Option<PathBuf> = None;
                let mut tree_action: Option<TreeAction> = None;
                let mut edit_outcome: Option<TreeEditOutcome> = None;
                let current_path = Path::new(&doc.path);
                let is_current_dirty = doc.is_current_dirty;
                let unsaved_files = &doc.unsaved_files;
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
