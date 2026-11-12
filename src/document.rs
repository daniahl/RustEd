//! Document buffer, layout, and search management.
//!
//! Handles open and unsaved file buffers, automatic language detection, line ending sequence,
//! gutter line numbers, and the interactive Find / Find-Next dialog.

use crate::{
    editor::{self, FastEditor},
    piece_table::PieceTable,
};
use eframe::egui;
use egui_extras::syntax_highlighting::{self, CodeTheme};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAX_HIGHLIGHT_BYTES: usize = 16_384;

pub struct UnsavedFileState {
    pub text: PieceTable,
    pub saved_text: String,
    pub saved_revision: u64,
    pub editor_state: Option<editor::FastEditorState>,
    pub line_count: usize,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub selected_chars: usize,
}

pub struct Document {
    pub path: String,
    pub text: PieceTable,
    pub saved_text: String,
    pub saved_revision: u64,
    pub is_current_dirty: bool,
    pub unsaved_files: HashMap<PathBuf, UnsavedFileState>,
    pub text_revision: u64,
    pub file_name: String,
    pub language_ext: String,
    pub language_name: &'static str,
    pub language_salt: u64,
    pub line_ending: &'static str,
    /// Per-row logical line number (0 = wrapped continuation row), for the gutter.
    /// Rebuilt only when the editor's galley changes.
    pub gutter_cache: Option<(Arc<egui::Galley>, Vec<u32>)>,
    /// Line count from the last rebuild, used to size the gutter.
    pub line_count: usize,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub selected_chars: usize,
    pub status_file_cache: Option<(usize, usize, String)>,
    pub status_pos_cache: Option<((usize, usize, usize), String)>,
    pub find_open: bool,
    pub find_query: String,
    pub focus_find_input: bool,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        Self {
            path: String::new(),
            text: PieceTable::new(),
            saved_text: String::new(),
            saved_revision: 0,
            is_current_dirty: false,
            unsaved_files: HashMap::new(),
            text_revision: 0,
            file_name: "Untitled".to_string(),
            language_ext: "rs".to_string(),
            language_name: "Plain Text",
            language_salt: hash_language_salt("rs"),
            line_ending: "LF",
            gutter_cache: None,
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
            status_file_cache: None,
            status_pos_cache: None,
            find_open: false,
            find_query: String::new(),
            focus_find_input: false,
        }
    }

    pub fn set_text(&mut self, text: impl Into<PieceTable>) {
        self.text = text.into();
        self.text_revision = self.text_revision.wrapping_add(1);
        self.gutter_cache = None;
        self.line_count = self.text.line_count();
        self.refresh_line_ending();
    }

    pub fn can_save(&self) -> bool {
        !self.path.is_empty() && self.is_current_dirty
    }

    pub fn is_file_dirty(&self, path: &Path) -> bool {
        (self.is_current_dirty && Path::new(&self.path) == path)
            || (!self.unsaved_files.is_empty() && self.unsaved_files.contains_key(path))
    }

    pub fn save_current_file(&mut self) {
        if !self.path.is_empty() && self.is_current_dirty {
            if fs::write(&self.path, self.text.as_str()).is_ok() {
                self.saved_text = self.text.to_string();
                self.saved_revision = self.text_revision;
                self.is_current_dirty = false;
                self.unsaved_files.remove(Path::new(&self.path));
            }
        }
    }

    pub fn open_file(&mut self, ctx: &egui::Context, chosen: PathBuf) {
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
                st.accesskit_row_cache = None;
            }
            self.unsaved_files.insert(
                PathBuf::from(&self.path),
                UnsavedFileState {
                    text: std::mem::take(&mut self.text),
                    saved_text: std::mem::take(&mut self.saved_text),
                    saved_revision: self.saved_revision,
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
                self.saved_revision = unsaved.saved_revision;
                self.is_current_dirty = self.text_revision != self.saved_revision;
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
                self.text = PieceTable::from_string(content);
                self.saved_revision = self.text_revision;
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

    pub fn close_if_deleted(&mut self, ctx: &egui::Context, path: &Path, is_dir: bool) {
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
                self.saved_revision = self.text_revision;
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
    }

    pub fn update_path_if_renamed(&mut self, old_path: &Path, new_path: &Path, is_dir: bool) {
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
                self.unsaved_files.insert(new_path.to_path_buf(), state);
            }
        }
    }

    pub fn refresh_line_ending(&mut self) {
        self.line_ending = if self.text.has_crlf_in_initial_bytes(4096) {
            "CRLF"
        } else {
            "LF"
        };
    }

    pub fn refresh_file_metadata(&mut self) {
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
        self.status_file_cache = None;
        self.refresh_line_ending();
    }

    pub fn status_file_label(&mut self) -> &str {
        let line_count = self.line_count.max(1);
        let bytes = self.text.len();
        let needs_update = match &self.status_file_cache {
            Some((cached_lines, cached_bytes, _)) => {
                *cached_lines != line_count || *cached_bytes != bytes
            }
            None => true,
        };
        if needs_update {
            use std::fmt::Write;
            let mut s = self
                .status_file_cache
                .take()
                .map(|(_, _, s)| s)
                .unwrap_or_else(|| String::with_capacity(64));
            s.clear();
            let file_name = &self.file_name;
            if bytes >= 1024 * 1024 {
                let _ = write!(
                    s,
                    "{file_name} — {line_count} lines ({:.1} MB)",
                    bytes as f64 / (1024.0 * 1024.0)
                );
            } else if bytes >= 1024 {
                let _ = write!(
                    s,
                    "{file_name} — {line_count} lines ({:.1} KB)",
                    bytes as f64 / 1024.0
                );
            } else {
                let _ = write!(s, "{file_name} — {line_count} lines ({bytes} B)");
            }
            self.status_file_cache = Some((line_count, bytes, s));
        }
        &self.status_file_cache.as_ref().unwrap().2
    }

    pub fn status_pos_label(&mut self) -> &str {
        let key = (self.cursor_line, self.cursor_col, self.selected_chars);
        let needs_update = match &self.status_pos_cache {
            Some((cached_key, _)) => *cached_key != key,
            None => true,
        };
        if needs_update {
            use std::fmt::Write;
            let mut s = self
                .status_pos_cache
                .take()
                .map(|(_, s)| s)
                .unwrap_or_else(|| String::with_capacity(48));
            s.clear();
            if self.selected_chars > 0 {
                let _ = write!(
                    s,
                    "Ln {}, Col {} ({} selected)",
                    self.cursor_line, self.cursor_col, self.selected_chars
                );
            } else {
                let _ = write!(s, "Ln {}, Col {}", self.cursor_line, self.cursor_col);
            }
            self.status_pos_cache = Some((key, s));
        }
        &self.status_pos_cache.as_ref().unwrap().1
    }

    pub fn open_find_dialog(&mut self, ctx: &egui::Context) {
        let editor_id = egui::Id::new("code_editor");
        if let Some(state) = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id)) {
            if let Some(range) = state.cursor.char_range() {
                if range.primary != range.secondary {
                    let [min_c, max_c] = range.sorted();
                    let count = max_c.index - min_c.index;
                    let mut char_iter = self.text.char_indices();
                    let start_byte = if min_c.index == 0 {
                        Some(0)
                    } else {
                        char_iter.nth(min_c.index).map(|(b, _)| b)
                    };
                    if let Some(sb) = start_byte {
                        let end_byte = if count == 0 {
                            Some(sb)
                        } else {
                            char_iter
                                .nth(count - 1)
                                .map(|(b, c)| b + c.len_utf8())
                                .or(Some(self.text.len()))
                        };
                        if let Some(eb) = end_byte {
                            let selected = &self.text[sb..eb];
                            if !selected.is_empty()
                                && !selected.contains('\n')
                                && !selected.contains('\r')
                            {
                                self.find_query = selected.to_string();
                            }
                        }
                    }
                }
            }
        }
        self.find_open = true;
        self.focus_find_input = true;
    }

    pub fn find_next(&mut self, ctx: &egui::Context) -> bool {
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

        let start_byte = if start_char_idx == 0 {
            0
        } else {
            self.text
                .char_indices()
                .nth(start_char_idx)
                .map(|(b, _)| b)
                .unwrap_or(self.text.len())
        };

        let (_match_byte, match_start_char) =
            if let Some(rel) = self.text[start_byte..].find(&self.find_query) {
                let mb = start_byte + rel;
                let mc = start_char_idx + self.text[start_byte..mb].chars().count();
                (mb, mc)
            } else if let Some(mb) = self.text.find(&self.find_query) {
                let mc = self.text[..mb].chars().count();
                (mb, mc)
            } else {
                return false;
            };

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

    pub fn show_find_dialog(&mut self, ctx: &egui::Context) {
        if !self.find_open {
            return;
        }

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
                        let mut st = egui::text_edit::TextEditState::load(ui.ctx(), find_id)
                            .unwrap_or_default();
                        let len = self.find_query.chars().count();
                        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(len),
                        )));
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
                        let mut st = egui::text_edit::TextEditState::load(ui.ctx(), find_id)
                            .unwrap_or_default();
                        let len = self.find_query.chars().count();
                        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(len),
                        )));
                        st.store(ui.ctx(), find_id);
                        self.focus_find_input = false;
                    }

                    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let escape_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
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
                        .add_enabled(!self.find_query.is_empty(), egui::Button::new("Find"))
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

    pub fn show_editor(&mut self, ctx: &egui::Context, panel_frame: egui::Frame) {
        let language = self.language_ext.as_str();
        let language_salt = self.language_salt;
        let text_revision = self.text_revision;

        let mut editor_text_changed = false;
        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let text_color = ui.visuals().widgets.inactive.text_color();
            let font_id_for_layout = font_id.clone();

            let mut layouter = |ui: &egui::Ui, text: &PieceTable, wrap_width: f32| {
                let layout_job = if text.len() <= MAX_HIGHLIGHT_BYTES {
                    let mut job = syntax_highlighting::highlight(
                        ui.ctx(),
                        ui.style(),
                        &theme,
                        text.as_str(),
                        language,
                    );
                    job.wrap.max_width = wrap_width;
                    job
                } else {
                    egui::text::LayoutJob::simple(
                        text.build_string(),
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

                        let is_large = self.text.len() > MAX_HIGHLIGHT_BYTES;
                        let editor = FastEditor::new_with_buffer(
                            egui::Id::new("code_editor"),
                            &mut self.text,
                            editor_min_size,
                            &mut layouter,
                        )
                        .cache_salt(language_salt)
                        .text_revision(text_revision)
                        .virtualized(is_large);
                        let output = editor.show(ui);
                        editor_text_changed = output.text_changed;

                        // Rebuild the row -> line-number table only when the galley changed.
                        let stale = match &self.gutter_cache {
                            Some((g, _)) => !Arc::ptr_eq(g, &output.galley),
                            None => true,
                        };
                        if stale {
                            let mut nums = Vec::with_capacity(output.galley.rows.len());
                            let new_count = if output.virtualized {
                                let base = output.first_line as u32;
                                for i in 0..output.galley.rows.len() {
                                    nums.push(base + i as u32 + 1);
                                }
                                self.text.line_count().max(1)
                            } else {
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
                                (line_no as usize).max(1)
                            };
                            if new_count.max(1).ilog10() != self.line_count.max(1).ilog10() {
                                ui.ctx().request_repaint();
                            }
                            self.line_count = new_count;
                            self.gutter_cache = Some((Arc::clone(&output.galley), nums));
                        }

                        if let Some(cursor_range) = output.cursor_range {
                            let focus = cursor_range.primary.rcursor;
                            self.cursor_line = if output.virtualized {
                                focus.row + 1
                            } else if let Some((_, nums)) = &self.gutter_cache {
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
                        let active_row = output.cursor_range.map(|c| c.primary.rcursor.row);
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

                        let bottom_y = gutter_rect.max.y.max(output.response.rect.max.y);
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
            self.is_current_dirty =
                !self.path.is_empty() && self.text_revision != self.saved_revision;
            if was_dirty != self.is_current_dirty {
                ctx.request_repaint();
            }
        }
    }
}

pub fn hash_language_salt(lang: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(lang, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

pub fn format_u32_stack(mut n: u32, buf: &mut [u8; 16]) -> &str {
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

pub fn detect_language_name(path: &str) -> &'static str {
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
