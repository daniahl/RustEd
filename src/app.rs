//! Top-level application coordinator for RustEd.
//!
//! Manages tab navigation (Editor vs. Terminal), top action bar, bottom status bar,
//! and orchestrates interactions between the file explorer, active document, and terminal.

use crate::{
    document::Document,
    explorer::{FileExplorer, PendingTreeEdit, TreeAction},
    terminal::TerminalPane,
};
use eframe::egui;
use std::{
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

#[derive(PartialEq, Eq, Default, Clone, Copy)]
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
        ctx.set_zoom_factor(1.2);
        ctx.style_mut(|s| {
            s.scroll_animation = egui::style::ScrollAnimation::none();
        });
        ctx.options_mut(|o| {
            o.max_passes = std::num::NonZeroUsize::new(4).unwrap();
        });

        let initial_folder = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let terminal = TerminalPane::new(ctx.clone());
        let explorer = FileExplorer::new(initial_folder);
        let doc = Document::new();

        Self {
            tab: Tab::Editor,
            doc,
            explorer,
            terminal,
        }
    }

    pub fn new_headless(folder: &Path) -> Self {
        let terminal = TerminalPane::new_headless();
        let explorer = FileExplorer::new(folder.to_path_buf());
        let doc = Document::new();

        Self {
            tab: Tab::Editor,
            doc,
            explorer,
            terminal,
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

    pub fn update_ui(&mut self, ctx: &egui::Context) {
        if self.tab == Tab::Editor {
            let (ctrl_f, f3) = ctx.input_mut(|i| {
                let ctrl_f = i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)
                    || i.consume_key(egui::Modifiers::CTRL, egui::Key::F);
                let f3 = i.consume_key(egui::Modifiers::NONE, egui::Key::F3);
                (ctrl_f, f3)
            });
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

        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let prev_tab = self.tab;
                ui.selectable_value(&mut self.tab, Tab::Editor, "Editor");
                ui.selectable_value(&mut self.tab, Tab::Terminal, "Terminal");
                if prev_tab != Tab::Terminal && self.tab == Tab::Terminal {
                    self.terminal.focus_terminal = true;
                }

                if self.tab == Tab::Editor {
                    ui.separator();
                    ui.label("Project folder:");
                    let resp = ui.text_edit_singleline(&mut self.explorer.folder_path);
                    let enter_pressed =
                        resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    if ui.button("Open").clicked() || enter_pressed {
                        self.explorer.open_folder(PathBuf::from(&self.explorer.folder_path));
                    }

                    if ui.button("..").on_hover_text("Parent folder").clicked() {
                        self.explorer.open_parent_folder();
                    }

                    if ui.add_enabled(self.doc.can_save(), egui::Button::new("Save")).clicked() {
                        self.doc.save_current_file();
                    }

                    if ui.button("Find").on_hover_text("Find (Ctrl+F)").clicked() {
                        self.doc.open_find_dialog(ctx);
                    }
                } else if self.tab == Tab::Terminal {
                    ui.separator();
                    if ui.button("Clear (cls)").clicked() {
                        self.terminal.write_to_pty(b"cls\r");
                        self.terminal.focus_terminal = true;
                    }
                    if ui.button("Ctrl+C").clicked() {
                        self.terminal.write_to_pty(b"\x03");
                        self.terminal.focus_terminal = true;
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

        let panel_frame = egui::Frame::central_panel(&ctx.style())
            .inner_margin(4.0)
            .fill(ctx.style().visuals.extreme_bg_color);

        match self.tab {
            Tab::Editor => {
                self.explorer.show_sidebar(ctx, &mut self.doc);
                self.doc.show_find_dialog(ctx);
                self.doc.show_editor(ctx, panel_frame);
            }
            Tab::Terminal => {
                self.terminal.show(ctx, panel_frame);
            }
        }
    }
}

impl eframe::App for TextEditor {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_ui(ctx);
    }
}
