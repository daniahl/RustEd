//! Top-level application coordinator for RustEd.
//!
//! Manages tab navigation (Editor vs. Terminal), top action bar, bottom status bar,
//! and orchestrates interactions between the file explorer, active document, and terminal.

use crate::{
    document::{DiskSyncResult, Document},
    explorer::{FileExplorer, PendingTreeEdit, TreeAction},
    terminal::TerminalPane,
    watcher::{path_starts_with, path_strip_prefix, paths_equal, FileWatcher},
};
use eframe::egui;
use std::{
    path::{Path, PathBuf},
};

#[derive(PartialEq, Eq, Default, Clone, Copy, Debug)]
pub enum Tab {
    #[default]
    Editor,
    Terminal,
}

/// Status bar height in points.
pub const STATUS_BAR_HEIGHT: f32 = 22.0;
/// Default sidebar width in points.
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 220.0;
/// Minimum sidebar width in points.
pub const SIDEBAR_MIN_WIDTH: f32 = 140.0;

pub fn project_folder_input_id() -> egui::Id {
    egui::Id::unique("project_folder_input")
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
    /// Shown when the user attempts to close the window with unsaved changes.
    pub confirm_close_open: bool,
    allow_close: bool,
}

impl TextEditor {
    /// Access the active document.
    pub fn doc(&self) -> &Document {
        &self.doc
    }

    /// Mutably access the active document.
    pub fn doc_mut(&mut self) -> &mut Document {
        &mut self.doc
    }

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
        let folder_input = explorer.folder_path.to_string_lossy().into_owned();
        let last_synced_folder = explorer.folder_path.to_string_lossy().into_owned();
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
            confirm_close_open: false,
            allow_close: false,
        }
    }

    pub fn new_headless(folder: &Path) -> Self {
        let terminal = TerminalPane::new_headless();
        let explorer = FileExplorer::new(folder.to_path_buf());
        let folder_input = explorer.folder_path.to_string_lossy().into_owned();
        let last_synced_folder = explorer.folder_path.to_string_lossy().into_owned();
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
            confirm_close_open: false,
            allow_close: false,
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

    fn explain_filename(&self) -> String {
        if self.doc.path.as_os_str().is_empty() {
            return self.doc.file_name.clone();
        }
        if let Some(rel) = path_strip_prefix(&self.doc.path, &self.explorer.folder_path)
            .filter(|p| !p.as_os_str().is_empty())
        {
            return rel.to_string_lossy().replace('\\', "/");
        }
        self.doc.path.to_string_lossy().into_owned()
    }

    /// Sends the current editor selection to the integrated terminal as an AI Explain prompt,
    /// switches focus to the terminal tab, and submits it with Enter.
    /// Returns `true` when a non-empty selection was sent.
    pub fn ai_explain(&mut self, ctx: &egui::Context) -> bool {
        let Some((from_line, to_line, selected_text)) = self.doc.selected_text_with_lines(ctx)
        else {
            return false;
        };
        let filename = self.explain_filename();
        let prompt = format_ai_explain_prompt(&filename, from_line, to_line, &selected_text);
        self.tab = Tab::Terminal;
        self.terminal.focus_terminal = true;
        self.terminal.paste_and_submit(ctx, &prompt);
        ctx.request_repaint();
        true
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

        if self.doc.path.as_os_str().is_empty() {
            return None;
        }
        let doc_path = self.doc.path.clone();
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
            watcher.watch_root(&self.explorer.folder_path);
            let active = if !self.doc.path.as_os_str().is_empty() { Some(self.doc.path.as_path()) } else { None };
            watcher.watch_extra_file(active);
        }
    }

    pub fn save_active_file(&mut self) -> bool {
        if !self.doc.can_save() {
            return true;
        }
        if self.doc.is_untitled_dirty() {
            let Some(dest) = pick_save_file(&self.explorer.folder_path) else {
                return false;
            };
            if self.doc.save_current_file_as(dest.clone()) {
                self.explorer.selected_node = Some((dest.clone(), false));
                self.explorer.refresh_watched_paths(&[dest], false);
                return true;
            }
            return false;
        }
        self.doc.save_current_file();
        !self.doc.is_current_dirty
    }

    fn handle_close_confirmation(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_close
            && self.doc.has_unsaved_changes()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close_open = true;
        }

        if !self.confirm_close_open {
            return;
        }

        let mut open = self.confirm_close_open;
        let mut save_and_close = false;
        let mut discard_and_close = false;
        let mut cancel = false;

        egui::Window::new("Unsaved Changes")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("You have unsaved changes. Save before closing?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save & Close").clicked() {
                        save_and_close = true;
                    }
                    if ui.button("Discard & Close").clicked() {
                        discard_and_close = true;
                    }
                    if ui.button("Cancel").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape))
                    {
                        cancel = true;
                    }
                });
            });

        if save_and_close {
            let untitled_ok = if self.doc.is_untitled_dirty() {
                self.save_active_file()
            } else {
                true
            };
            if untitled_ok && self.doc.save_all() {
                self.allow_close = true;
                self.confirm_close_open = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.confirm_close_open = false;
                // Ensure the save error banner on the Editor tab is visible if close failed from Terminal.
                if self.doc.last_save_error.is_some() {
                    self.tab = Tab::Editor;
                }
            }
        } else if discard_and_close {
            self.allow_close = true;
            self.confirm_close_open = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel || !open {
            self.confirm_close_open = false;
        }
    }

    pub fn update_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.handle_close_confirmation(ctx);
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
            let folder_input_focused = ctx.memory(|m| m.has_focus(project_folder_input_id()));
            let (ctrl_s, ctrl_f, shift_f3, f3, fold_cur, unfold_cur, fold_all, unfold_all, ai_explain) =
                ctx.input_mut(|i| {
                    let ctrl_s = i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)
                        || i.consume_key(egui::Modifiers::CTRL, egui::Key::S);
                    let ctrl_f = i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)
                        || i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
                    let shift_f3 = i.consume_key(egui::Modifiers::SHIFT, egui::Key::F3);
                    let f3 = i.consume_key(egui::Modifiers::NONE, egui::Key::F3);

                    let (fold_cur, unfold_cur, fold_all, unfold_all, ai_explain) =
                        if !self.doc.find_open
                            && !self.confirm_close_open
                            && self.explorer.pending_tree_edit.is_none()
                            && self.explorer.pending_delete.is_none()
                            && self.explorer.pending_open_after_untitled.is_none()
                            && !folder_input_focused
                        {
                            let alt = egui::Modifiers::ALT;
                            let alt_shift = egui::Modifiers {
                                alt: true,
                                shift: true,
                                ..Default::default()
                            };
                            // Check Alt+Shift before Alt because egui's `consume_key` matches
                            // logically and can treat Shift as optional on arrow keys.
                            let fold_all = i.consume_key(alt_shift, egui::Key::ArrowLeft);
                            let unfold_all = i.consume_key(alt_shift, egui::Key::ArrowRight);
                            let fold_cur = i.consume_key(alt, egui::Key::ArrowLeft);
                            let unfold_cur = i.consume_key(alt, egui::Key::ArrowRight);
                            let ai_explain = i.consume_key(alt, egui::Key::E);
                            (fold_cur, unfold_cur, fold_all, unfold_all, ai_explain)
                        } else {
                            (false, false, false, false, false)
                        };
                    (
                        ctrl_s, ctrl_f, shift_f3, f3, fold_cur, unfold_cur, fold_all, unfold_all,
                        ai_explain,
                    )
                });
            if ctrl_s && self.doc.can_save() {
                self.save_active_file();
            }
            if ctrl_f {
                self.doc.open_find_dialog(ctx);
            }
            if shift_f3 {
                if self.doc.find_query.is_empty() {
                    self.doc.open_find_dialog(ctx);
                } else {
                    self.doc.find_prev(ctx);
                }
            } else if f3 {
                if self.doc.find_query.is_empty() {
                    self.doc.open_find_dialog(ctx);
                } else {
                    self.doc.find_next(ctx);
                }
            }
            if fold_cur {
                self.doc.fold_current(ctx);
            }
            if unfold_cur {
                self.doc.unfold_current(ctx);
            }
            if fold_all {
                self.doc.fold_all(ctx);
            }
            if unfold_all {
                self.doc.unfold_all(ctx);
            }
            if ai_explain {
                self.ai_explain(ctx);
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
                    let current_folder = self.explorer.folder_path.to_string_lossy();
                    if self.last_synced_folder != current_folder.as_ref() {
                        let owned = current_folder.into_owned();
                        self.folder_input = owned.clone();
                        self.last_synced_folder = owned;
                    }
                    ui.separator();
                    ui.label("Project folder:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.folder_input)
                            .id(project_folder_input_id()),
                    );
                    let enter_pressed =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    if enter_pressed {
                        let candidate = PathBuf::from(self.folder_input.trim());
                        if candidate.is_dir() {
                            self.explorer.open_folder(candidate);
                        } else {
                            self.explorer.last_fs_error =
                                Some(format!("Folder not found: {}", candidate.display()));
                        }
                        self.folder_input = self.explorer.folder_path.to_string_lossy().into_owned();
                        self.last_synced_folder = self.folder_input.clone();
                    } else if resp.lost_focus() {
                        // Discard uncommitted draft text when focus leaves without pressing Enter.
                        self.folder_input = self.last_synced_folder.clone();
                    }

                    if ui
                        .button("Open")
                        .on_hover_text("Choose a project folder")
                        .clicked()
                    {
                        if let Some(folder) = pick_folder(&self.explorer.folder_path) {
                            self.explorer.open_folder(folder);
                            self.folder_input = self.explorer.folder_path.to_string_lossy().into_owned();
                            self.last_synced_folder = self.folder_input.clone();
                        }
                    }

                    if ui
                        .add_enabled(self.doc.can_save(), egui::Button::new("Save"))
                        .on_hover_text("Save (Ctrl+S)")
                        .clicked()
                    {
                        self.save_active_file();
                    }

                    if ui.button("Find").on_hover_text("Find (Ctrl+F)").clicked() {
                        self.doc.open_find_dialog(ctx);
                    }

                    if ui
                        .add_enabled(self.doc.has_selection(ctx), egui::Button::new("AI Explain"))
                        .on_hover_text("AI Explain (Alt+E)")
                        .clicked()
                    {
                        self.ai_explain(ctx);
                    }
                } else if self.tab == Tab::Terminal {
                    ui.separator();
                    if ui
                        .button("Restart")
                        .on_hover_text("Restart the active terminal")
                        .clicked()
                    {
                        let cwd = &self.explorer.folder_path;
                        let working_dir = if cwd.is_dir() { Some(cwd.as_path()) } else { None };
                        self.terminal.restart(ctx, working_dir);
                    }
                }
            });
        });

        egui::Panel::bottom("status_bar")
            .exact_size(STATUS_BAR_HEIGHT)
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
                            ui.label(self.terminal.shell_label());
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
                self.explorer.keyboard_blocked = self.confirm_close_open;
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

/// Formats the prompt sent to the integrated terminal by the AI Explain feature.
pub fn format_ai_explain_prompt(
    filename: &str,
    from_line: usize,
    to_line: usize,
    selected_text: &str,
) -> String {
    format!("Explain {filename}:{from_line}-{to_line}: {selected_text}")
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

/// Shows the native folder-selection dialog, starting at `start_dir` when it exists.
fn pick_folder(start_dir: &Path) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Open project folder");
    if start_dir.is_dir() {
        dialog = dialog.set_directory(start_dir);
    }
    dialog.pick_folder()
}

/// Shows the native Save As dialog, starting at `start_dir` when it exists.
pub(crate) fn pick_save_file(start_dir: &Path) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title("Save file as");
    if start_dir.is_dir() {
        dialog = dialog.set_directory(start_dir);
    }
    dialog.save_file()
}

impl eframe::App for TextEditor {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.update_ui(ui);
    }
}
