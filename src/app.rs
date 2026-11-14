//! Top-level application coordinator for RustEd.
//!
//! Manages tab navigation (Editor vs. Terminal), top action bar, bottom status bar,
//! and orchestrates interactions between the file explorer, active document, and terminal.

use crate::{
    document::{DiskSyncResult, Document},
    explorer::{FileExplorer, PendingTreeEdit, TreeAction},
    terminal::TerminalPane,
    watcher::{path_starts_with, paths_equal, FileWatcher},
};
use eframe::egui;
use std::{
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

#[derive(PartialEq, Eq, Default, Clone, Copy, Debug)]
pub enum Tab {
    #[default]
    Editor,
    Terminal,
}

pub struct TextEditor {
    pub tab: Tab,
    pub doc: Document,
    pub explorer: FileExplorer,
    pub terminal: TerminalPane,
    pub watcher: Option<FileWatcher>,
    /// Draft text for the top-bar project folder input so uncommitted edits don't mutate `explorer.folder_path`.
    folder_input: String,
    last_synced_folder: String,
    /// When set, the next editor frame grabs keyboard focus so the caret is active at startup.
    pub focus_editor_pending: bool,
}

impl Deref for TextEditor {
    type Target = Document;

    fn deref(&self) -> &Self::Target {
        &self.doc
    }
}

impl DerefMut for TextEditor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.doc
    }
}

impl TextEditor {
    pub fn new(ctx: egui::Context) -> Self {
        Self::new_with_folder(ctx, folder_from_args(std::env::args().skip(1)))
    }

    /// Creates the app showing `folder` (or the current directory when `None`).
    pub fn new_with_folder(ctx: egui::Context, folder: Option<PathBuf>) -> Self {
        ctx.set_zoom_factor(1.2);
        ctx.global_style_mut(|s| {
            s.scroll_animation = egui::style::ScrollAnimation::none();
        });
        ctx.options_mut(|o| {
            o.max_passes = std::num::NonZeroUsize::new(2).unwrap();
        });

        let initial_folder = folder
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let terminal = TerminalPane::new(ctx.clone(), Some(&initial_folder));
        let watcher = Some(FileWatcher::new(ctx, &initial_folder));
        let explorer = FileExplorer::new(initial_folder);
        let folder_input = explorer.folder_path.clone();
        let last_synced_folder = explorer.folder_path.clone();
        let doc = Document::new();

        Self {
            tab: Tab::Editor,
            doc,
            explorer,
            terminal,
            watcher,
            folder_input,
            last_synced_folder,
            focus_editor_pending: true,
        }
    }

    pub fn new_headless(folder: &Path) -> Self {
        let terminal = TerminalPane::new_headless();
        let explorer = FileExplorer::new(folder.to_path_buf());
        let folder_input = explorer.folder_path.clone();
        let last_synced_folder = explorer.folder_path.clone();
        let doc = Document::new();

        Self {
            tab: Tab::Editor,
            doc,
            explorer,
            terminal,
            watcher: None,
            folder_input,
            last_synced_folder,
            focus_editor_pending: false,
        }
    }

    pub fn open_file(&mut self, ctx: &egui::Context, chosen: PathBuf) {
        self.explorer.selected_node = Some((chosen.clone(), false));
        self.doc.open_file(ctx, chosen);
    }

    pub fn handle_tree_action(&mut self, ctx: &egui::Context, action: TreeAction) {
        self.explorer.handle_tree_action(ctx, &mut self.doc, action);
    }

    pub fn commit_tree_edit(&mut self, ctx: &egui::Context, edit: PendingTreeEdit) {
        self.explorer.commit_tree_edit(ctx, &mut self.doc, edit);
    }

    pub fn start_rename_selected(&mut self) {
        self.explorer.start_rename_selected();
    }

    /// Applies a batch of filesystem change paths to the explorer tree and the active document.
    /// Returns the active document path if syncing hit a transient file lock and should be retried.
    pub fn process_fs_events(
        &mut self,
        ctx: &egui::Context,
        paths: &[PathBuf],
        rescan_all: bool,
    ) -> Option<PathBuf> {
        self.explorer.refresh_watched_paths(paths, rescan_all);

        if self.doc.path.is_empty() {
            return None;
        }
        let doc_path = PathBuf::from(&self.doc.path);
        let touches_doc = rescan_all
            || paths
                .iter()
                .any(|p| paths_equal(p, &doc_path) || path_starts_with(&doc_path, p));
        if touches_doc
            && matches!(
                self.doc.sync_with_disk(ctx, false),
                DiskSyncResult::RetryTransientError
            )
        {
            return Some(doc_path);
        }
        None
    }

    fn poll_watcher(&mut self, ctx: &egui::Context) {
        let Some((batch, due_retries)) = self.watcher.as_mut().and_then(|w| w.poll()) else {
            return;
        };
        if let Some(retry_path) = self.process_fs_events(ctx, &batch.paths, batch.rescan_all) {
            let attempt = due_retries
                .iter()
                .find(|(p, _)| paths_equal(p, &retry_path))
                .map(|(_, attempt)| *attempt)
                .unwrap_or(0);
            if let Some(watcher) = &mut self.watcher {
                watcher.schedule_retry(retry_path, attempt);
            }
        }
    }

    fn sync_watcher(&mut self) {
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_root(Path::new(&self.explorer.folder_path));
            let active = (!self.doc.path.is_empty()).then(|| Path::new(&self.doc.path));
            watcher.watch_extra_file(active);
        }
    }

    pub fn update_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.poll_watcher(ctx);
        let ctrl_tab = ctx.input_mut(|i| {
            let ctrl = i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Tab);
            let ctrl_shift = i.consume_key(
                egui::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                egui::Key::Tab,
            ) || i.consume_key(
                egui::Modifiers {
                    command: true,
                    shift: true,
                    ..Default::default()
                },
                egui::Key::Tab,
            );
            ctrl || ctrl_shift
        });
        if ctrl_tab {
            self.tab = match self.tab {
                Tab::Editor => {
                    self.terminal.focus_terminal = true;
                    Tab::Terminal
                }
                Tab::Terminal => {
                    self.focus_editor_pending = true;
                    Tab::Editor
                }
            };
        }

        if self.tab == Tab::Editor {
            let (ctrl_s, ctrl_f, f3) = ctx.input_mut(|i| {
                let ctrl_s = i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)
                    || i.consume_key(egui::Modifiers::CTRL, egui::Key::S);
                let ctrl_f = i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)
                    || i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
                let f3 = i.consume_key(egui::Modifiers::NONE, egui::Key::F3);
                (ctrl_s, ctrl_f, f3)
            });
            if ctrl_s && self.doc.can_save() {
                self.doc.save_current_file();
            }
            if ctrl_f {
                self.doc.open_find_dialog(ctx);
            }
            if f3 {
                if self.doc.find_query.is_empty() {
                    self.doc.open_find_dialog(ctx);
                } else {
                    self.doc.find_next(ctx);
                }
            }
        }

        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                let prev_tab = self.tab;
                ui.selectable_value(&mut self.tab, Tab::Editor, "Editor");
                ui.selectable_value(&mut self.tab, Tab::Terminal, "Terminal");
                if prev_tab != self.tab {
                    match self.tab {
                        Tab::Terminal => self.terminal.focus_terminal = true,
                        Tab::Editor => self.focus_editor_pending = true,
                    }
                }

                if self.tab == Tab::Editor {
                    if self.last_synced_folder != self.explorer.folder_path {
                        self.folder_input = self.explorer.folder_path.clone();
                        self.last_synced_folder = self.explorer.folder_path.clone();
                    }
                    ui.separator();
                    ui.label("Project folder:");
                    let resp = ui.text_edit_singleline(&mut self.folder_input);
                    let enter_pressed =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    if enter_pressed {
                        self.explorer.open_folder(PathBuf::from(&self.folder_input));
                        self.folder_input = self.explorer.folder_path.clone();
                        self.last_synced_folder = self.explorer.folder_path.clone();
                    }

                    if ui
                        .button("Open")
                        .on_hover_text("Choose a project folder")
                        .clicked()
                    {
                        if let Some(folder) = pick_folder(&self.explorer.folder_path) {
                            self.explorer.open_folder(folder);
                            self.folder_input = self.explorer.folder_path.clone();
                            self.last_synced_folder = self.explorer.folder_path.clone();
                        }
                    }

                    if ui
                        .add_enabled(self.doc.can_save(), egui::Button::new("Save"))
                        .on_hover_text("Save (Ctrl+S)")
                        .clicked()
                    {
                        self.doc.save_current_file();
                    }

                    if ui.button("Find").on_hover_text("Find (Ctrl+F)").clicked() {
                        self.doc.open_find_dialog(ctx);
                    }
                } else if self.tab == Tab::Terminal {
                    ui.separator();
                    if ui
                        .button("Restart")
                        .on_hover_text("Restart the active terminal")
                        .clicked()
                    {
                        let cwd = Path::new(&self.explorer.folder_path);
                        let working_dir = if cwd.is_dir() { Some(cwd) } else { None };
                        self.terminal.restart(ctx, working_dir);
                    }
                }
            });
        });

        egui::Panel::bottom("status_bar")
            .exact_size(22.0)
            .show(ui, |ui| {
                ui.style_mut().override_text_style = Some(egui::TextStyle::Small);
                ui.style_mut().spacing.item_spacing = egui::vec2(6.0, 0.0);
                ui.horizontal_centered(|ui| {
                    match self.tab {
                        Tab::Editor => {
                            ui.label(self.doc.status_file_label());

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(self.doc.language_name)
                                        .on_hover_text("Language Mode");

                                    ui.separator();
                                    ui.label(self.doc.line_ending)
                                        .on_hover_text("End of Line Sequence");

                                    ui.separator();
                                    ui.label("UTF-8").on_hover_text("Encoding");

                                    ui.separator();
                                    ui.label("Spaces: 4").on_hover_text("Indentation");

                                    ui.separator();
                                    ui.label(self.doc.status_pos_label())
                                        .on_hover_text("Cursor Position");
                                },
                            );
                        }
                        Tab::Terminal => {
                            ui.label("Terminal — cmd.exe");
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(self.terminal.size_label())
                                        .on_hover_text("Terminal Size");
                                    ui.separator();
                                    ui.label("UTF-8").on_hover_text("Encoding");
                                },
                            );
                        }
                    }
                });
            });

        let panel_frame = egui::Frame::central_panel(&ctx.global_style())
            .inner_margin(4.0)
            .fill(ctx.global_style().visuals.extreme_bg_color);

        match self.tab {
            Tab::Editor => {
                self.explorer.show_sidebar(ui, &mut self.doc);
                if self.focus_editor_pending {
                    self.focus_editor_pending = false;
                    self.doc.focus_editor = true;
                    crate::editor::request_editor_focus(ctx, crate::editor::code_editor_id());
                }
                self.doc.show_find_dialog(ctx);
                self.doc.show_editor(ui, panel_frame);
            }
            Tab::Terminal => {
                self.terminal.show(ui, panel_frame);
            }
        }
        self.sync_watcher();
    }
}

/// Returns the first command-line argument that names an existing folder, made absolute.
/// Non-folder arguments (flags, files, missing paths) are ignored.
pub fn folder_from_args<I: IntoIterator<Item = String>>(args: I) -> Option<PathBuf> {
    args.into_iter().map(PathBuf::from).find(|p| p.is_dir()).map(|p| {
        // A bare Windows drive ("G:") is drive-relative, i.e. the current directory on that
        // drive. When given as an argument the user means the drive root ("G:\").
        let mut parts = p.components();
        if let (Some(std::path::Component::Prefix(prefix)), None) = (parts.next(), parts.next()) {
            let mut root = prefix.as_os_str().to_os_string();
            root.push(std::path::MAIN_SEPARATOR_STR);
            return PathBuf::from(root);
        }
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir().map_or(p.clone(), |cwd| cwd.join(&p))
        }
    })
}

/// Shows the native folder-selection dialog, starting at start_dir when it exists.
fn pick_folder(start_dir: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Open project folder");
    if Path::new(start_dir).is_dir() {
        dialog = dialog.set_directory(start_dir);
    }
    dialog.pick_folder()
}

impl eframe::App for TextEditor {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_ui(ui);
    }
}
