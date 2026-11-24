//! Document buffer, layout, and search management.
//!
//! Handles open and unsaved file buffers, automatic language detection, line ending sequence,
//! gutter line numbers, and the interactive Find / Find-Next dialog.

use crate::{
    editor::{self, EditorBuffer, FastEditor},
    folding::FoldState,
    piece_table::{PieceTable, PieceTableSnapshot},
    watcher::{path_starts_with, path_strip_prefix, paths_equal},
};
use eframe::egui;
use egui_extras::syntax_highlighting::{self, CodeTheme};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

pub const MAX_HIGHLIGHT_BYTES: usize = 65_536;

/// Flag OR'ed into gutter-cache row numbers marking wrapped continuation rows
/// (logical line continues from the previous visual row).
pub const WRAPPED_ROW_FLAG: u32 = 0x8000_0000;
/// Number of initial bytes scanned to detect CRLF line endings.
pub const CRLF_DETECT_WINDOW: usize = 4096;
/// Maximum selection length (in chars) that auto-populates the Find dialog.
pub const MAX_FIND_SEED_CHARS: usize = 1024;
/// Byte thresholds for human-readable file size formatting.
const KB: usize = 1024;
const MB: usize = 1024 * 1024;
/// Find dialog anchor offset from the top-right corner.
const FIND_DIALOG_OFFSET_X: f32 = -16.0;
const FIND_DIALOG_OFFSET_Y: f32 = 36.0;
/// Default width of the Find dialog input field.
const FIND_INPUT_WIDTH: f32 = 180.0;
/// Minimum gutter digit count (so "1" through "999" all get the same width).
const GUTTER_MIN_DIGITS: usize = 3;
/// Approximate width of one monospace digit in points.
const GUTTER_DIGIT_WIDTH: f32 = 9.0;
/// Extra padding added to the computed gutter width.
const GUTTER_PADDING: f32 = 16.0;
/// Width reserved in the gutter for clickable code-folding chevrons.
const FOLD_GUTTER_WIDTH: f32 = 16.0;
/// Right-side offset for the gutter line-number text (before the fold column).
const GUTTER_TEXT_RIGHT_OFFSET: f32 = 6.0;
/// Right-side offset for the gutter separator line.
const GUTTER_SEPARATOR_OFFSET: f32 = 4.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalConflict {
    ModifiedOnDisk,
    DeletedOnDisk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskSyncResult {
    Unchanged,
    Reloaded,
    ClosedDeleted,
    Conflict(ExternalConflict),
    RetryTransientError,
}

fn read_disk_stamp(path: &Path) -> Option<DiskStamp> {
    fs::metadata(path).ok().map(|m| DiskStamp {
        len: m.len(),
        modified: m.modified().ok(),
    })
}

fn is_transient_fs_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        matches!(err.raw_os_error(), Some(32) | Some(33))
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

fn take_unsaved_entry(
    map: &mut HashMap<PathBuf, UnsavedFileState>,
    target: &Path,
) -> Option<UnsavedFileState> {
    if let Some(exact) = map.remove(target) {
        return Some(exact);
    }
    let key = map.keys().find(|k| paths_equal(k, target)).cloned()?;
    map.remove(&key)
}

fn has_unsaved_entry(map: &HashMap<PathBuf, UnsavedFileState>, target: &Path) -> bool {
    map.contains_key(target) || map.keys().any(|k| paths_equal(k, target))
}

fn write_piece_table_atomic(path: &Path, text: &PieceTable) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();

    let mut temp_file = None;
    let mut temp_path = PathBuf::new();
    for attempt in 0u32..32 {
        let candidate = parent.join(format!(".{stem}.{pid}.{attempt}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => {
                temp_path = candidate;
                temp_file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => break,
        }
    }

    if let Some(file) = temp_file {
        let write_res = (|| -> std::io::Result<()> {
            let mut writer = std::io::BufWriter::new(file);
            text.write_to(&mut writer)?;
            let file = writer.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_res {
            let _ = fs::remove_file(&temp_path);
            return Err(err);
        }
        if fs::rename(&temp_path, path).is_ok() {
            return Ok(());
        }
        let _ = fs::remove_file(&temp_path);
    }

    // Fallback to direct write when atomic temp + rename isn't possible (e.g. symlink target or ACLs).
    let file = fs::File::create(path)?;
    let mut writer = std::io::BufWriter::new(file);
    text.write_to(&mut writer)?;
    let file = writer.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
}

pub struct UnsavedFileState {
    pub text: PieceTable,
    pub saved_snapshot: Option<PieceTableSnapshot>,
    pub saved_revision: u64,
    pub disk_stamp: Option<DiskStamp>,
    pub editor_state: Option<editor::FastEditorState>,
    pub folded_lines: BTreeSet<usize>,
    pub line_count: usize,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub selected_chars: usize,
}

pub struct Document {
    pub path: PathBuf,
    pub text: PieceTable,
    pub saved_snapshot: Option<PieceTableSnapshot>,
    pub saved_revision: u64,
    pub is_current_dirty: bool,
    pub unsaved_files: HashMap<PathBuf, UnsavedFileState>,
    pub text_revision: u64,
    pub file_name: String,
    pub language_ext: String,
    pub language_name: &'static str,
    pub(crate) language_salt: u64,
    pub line_ending: &'static str,
    /// Per-row logical line number (0 = wrapped continuation row), for the gutter.
    /// Rebuilt only when the editor's galley changes.
    pub gutter_cache: Option<(Arc<egui::Galley>, Vec<u32>)>,
    /// Code folding ranges and collapsed fold state.
    pub folding: FoldState,
    /// Line count from the last rebuild, used to size the gutter.
    pub line_count: usize,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub selected_chars: usize,
    last_cursor_status_key: Option<(u64, egui::text::CCursorRange)>,
    pub(crate) status_file_cache: Option<(usize, usize, String)>,
    pub(crate) status_pos_cache: Option<((usize, usize, usize), String)>,
    pub find_open: bool,
    pub find_query: String,
    pub focus_find_input: bool,
    pub focus_editor: bool,
    pub disk_stamp: Option<DiskStamp>,
    pub external_conflict: Option<ExternalConflict>,
    pub last_save_error: Option<String>,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        let text = PieceTable::new();
        let saved_snapshot = Some(text.snapshot());
        Self {
            path: PathBuf::new(),
            text,
            saved_snapshot,
            saved_revision: 0,
            is_current_dirty: false,
            unsaved_files: HashMap::new(),
            text_revision: 0,
            file_name: "Untitled".to_string(),
            language_ext: "txt".to_string(),
            language_name: "Plain Text",
            language_salt: hash_language_salt("txt"),
            line_ending: "LF",
            gutter_cache: None,
            folding: FoldState::new(),
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
            last_cursor_status_key: None,
            status_file_cache: None,
            status_pos_cache: None,
            find_open: false,
            find_query: String::new(),
            focus_find_input: false,
            focus_editor: false,
            disk_stamp: None,
            external_conflict: None,
            last_save_error: None,
        }
    }

    pub fn set_text(&mut self, text: impl Into<PieceTable>) {
        self.text = text.into();
        self.text_revision = self.text_revision.wrapping_add(1);
        self.gutter_cache = None;
        self.last_cursor_status_key = None;
        self.folding.clear();
        self.line_count = self.text.line_count();
        self.refresh_line_ending();
    }

    pub fn can_save(&self) -> bool {
        self.is_current_dirty
    }

    /// Returns `true` when the active buffer is an unsaved `Untitled` buffer with no disk path yet.
    pub fn is_untitled_dirty(&self) -> bool {
        self.path.as_os_str().is_empty() && self.is_current_dirty
    }

    /// Returns `true` if `path` (or any file inside `path` when `is_dir` is `true`) has unsaved edits.
    pub fn has_dirty_in_path(&self, path: &Path, is_dir: bool) -> bool {
        if !self.path.as_os_str().is_empty()
            && self.is_current_dirty
            && (paths_equal(&self.path, path) || (is_dir && path_starts_with(&self.path, path)))
        {
            return true;
        }
        self.unsaved_files.keys().any(|k| {
            paths_equal(k, path) || (is_dir && path_starts_with(k, path))
        })
    }

    /// Returns `true` if the editor currently has a non-empty text selection.
    pub fn has_selection(&self, ctx: &egui::Context) -> bool {
        let editor_id = editor::code_editor_id();
        ctx.data(|d| d.get_temp::<editor::FastEditorState>(editor_id))
            .and_then(|state| state.cursor.char_range())
            .is_some_and(|range| {
                let total = self.text.char_count();
                let [min_c, max_c] = range.sorted_cursors();
                min_c.index.0.min(total) < max_c.index.0.min(total)
            })
    }

    /// Returns `(from_line, to_line, selected_text)` for the current editor selection,
    /// where `from_line` and `to_line` are 1-based inclusive line numbers.
    /// Returns `None` when there is no non-empty selection.
    pub fn selected_text_with_lines(&self, ctx: &egui::Context) -> Option<(usize, usize, String)> {
        let editor_id = editor::code_editor_id();
        let state = ctx.data(|d| d.get_temp::<editor::FastEditorState>(editor_id))?;
        let range = state.cursor.char_range()?;
        let total = self.text.char_count();
        let [min_c, max_c] = range.sorted_cursors();
        let start_char = min_c.index.0.min(total);
        let end_char = max_c.index.0.min(total);
        if start_char >= end_char {
            return None;
        }
        let from_line = self.text.char_to_line_col(start_char).0 + 1;
        let to_line = self.text.char_to_line_col(end_char - 1).0 + 1;
        let selected = self.text.slice_chars(start_char..end_char);
        Some((from_line, to_line, selected))
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.can_save() || !self.unsaved_files.is_empty()
    }

    pub fn save_all(&mut self) -> bool {
        let mut all_ok = true;
        if self.is_untitled_dirty() {
            self.last_save_error =
                Some("Cannot save Untitled without choosing a destination path.".to_string());
            all_ok = false;
        } else if self.can_save() {
            self.save_current_file();
            if self.is_current_dirty {
                all_ok = false;
            }
        }
        let pending_paths: Vec<PathBuf> = self.unsaved_files.keys().cloned().collect();
        for path in pending_paths {
            let Some(state) = self.unsaved_files.get(&path) else {
                continue;
            };
            match write_piece_table_atomic(&path, &state.text) {
                Ok(()) => {
                    self.unsaved_files.remove(&path);
                }
                Err(err) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file");
                    self.last_save_error = Some(format!("Failed to save {name}: {err}"));
                    all_ok = false;
                }
            }
        }
        all_ok
    }

    pub fn is_file_dirty(&self, path: &Path) -> bool {
        (self.is_current_dirty && !self.path.as_os_str().is_empty() && paths_equal(&self.path, path))
            || (!self.unsaved_files.is_empty() && has_unsaved_entry(&self.unsaved_files, path))
    }

    /// Saves the active buffer to `target_path` (Save As), updating `self.path` and metadata on success.
    pub fn save_current_file_as(&mut self, target_path: PathBuf) -> bool {
        match write_piece_table_atomic(&target_path, &self.text) {
            Ok(()) => {
                self.path = target_path.clone();
                self.saved_snapshot = Some(self.text.snapshot());
                self.saved_revision = self.text_revision;
                self.is_current_dirty = false;
                self.last_save_error = None;
                self.disk_stamp = read_disk_stamp(&target_path);
                self.external_conflict = None;
                take_unsaved_entry(&mut self.unsaved_files, &target_path);
                self.refresh_file_metadata();
                true
            }
            Err(err) => {
                let name = target_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                self.last_save_error = Some(format!("Failed to save {name}: {err}"));
                false
            }
        }
    }

    pub fn save_current_file(&mut self) {
        if !self.path.as_os_str().is_empty() && self.is_current_dirty {
            let path = self.path.clone();
            match write_piece_table_atomic(&path, &self.text) {
                Ok(()) => {
                    self.saved_snapshot = Some(self.text.snapshot());
                    self.saved_revision = self.text_revision;
                    self.is_current_dirty = false;
                    self.last_save_error = None;
                    self.disk_stamp = read_disk_stamp(&path);
                    self.external_conflict = None;
                    take_unsaved_entry(&mut self.unsaved_files, &path);
                }
                Err(err) => {
                    self.last_save_error = Some(format!("Failed to save {}: {err}", self.file_name));
                }
            }
        }
    }

    pub fn open_file(&mut self, ctx: &egui::Context, chosen: PathBuf) {
        if !self.path.as_os_str().is_empty() && paths_equal(&self.path, &chosen) {
            return;
        }

        enum IncomingFile {
            Unsaved(Box<UnsavedFileState>),
            FromDisk(String, Option<DiskStamp>),
        }

        let incoming = if let Some(unsaved) = take_unsaved_entry(&mut self.unsaved_files, &chosen) {
            IncomingFile::Unsaved(Box::new(unsaved))
        } else {
            match fs::read_to_string(&chosen) {
                Ok(content) => IncomingFile::FromDisk(content, read_disk_stamp(&chosen)),
                Err(err) => {
                    self.last_save_error = Some(format!(
                        "Failed to open {}: {err}",
                        chosen
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("file")
                    ));
                    return;
                }
            }
        };

        let editor_id = editor::code_editor_id();
        if !self.path.as_os_str().is_empty() && self.is_current_dirty {
            let mut prev_state = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id));
            if let Some(ref mut st) = prev_state {
                st.clear_caches();
            }
            self.text.drop_cached_text();
            self.unsaved_files.insert(
                self.path.clone(),
                UnsavedFileState {
                    text: std::mem::take(&mut self.text),
                    saved_snapshot: self.saved_snapshot.take(),
                    saved_revision: self.saved_revision,
                    disk_stamp: self.disk_stamp.clone(),
                    editor_state: prev_state,
                    folded_lines: self.folding.folded_lines().clone(),
                    line_count: self.line_count,
                    cursor_line: self.cursor_line,
                    cursor_col: self.cursor_col,
                    selected_chars: self.selected_chars,
                },
            );
        }

        self.path = chosen.clone();
        self.text_revision = self.text_revision.wrapping_add(1);
        self.gutter_cache = None;
        self.last_cursor_status_key = None;
        self.folding.clear();
        self.external_conflict = None;
        self.last_save_error = None;

        match incoming {
            IncomingFile::Unsaved(unsaved) => {
                self.text = unsaved.text;
                self.saved_snapshot = unsaved.saved_snapshot;
                self.saved_revision = unsaved.saved_revision;
                self.is_current_dirty = self.text_revision != self.saved_revision;
                self.folding.set_folded_lines(unsaved.folded_lines);
                self.line_count = unsaved.line_count;
                self.cursor_line = unsaved.cursor_line;
                self.cursor_col = unsaved.cursor_col;
                self.selected_chars = unsaved.selected_chars;
                let current_stamp = read_disk_stamp(&chosen);
                match (&unsaved.disk_stamp, &current_stamp) {
                    (Some(_), None) => {
                        self.disk_stamp = unsaved.disk_stamp;
                        self.external_conflict = Some(ExternalConflict::DeletedOnDisk);
                    }
                    (Some(prev), Some(curr)) if prev != curr => {
                        if fs::read_to_string(&chosen)
                            .is_ok_and(|disk_text| self.text.eq_str(&disk_text))
                        {
                            self.disk_stamp = current_stamp;
                        } else {
                            self.disk_stamp = unsaved.disk_stamp;
                            self.external_conflict = Some(ExternalConflict::ModifiedOnDisk);
                        }
                    }
                    _ => {
                        self.disk_stamp = current_stamp;
                    }
                }
                self.refresh_file_metadata();

                let editor_state = unsaved.editor_state;
                ctx.data_mut(|d| {
                    let mut st = editor_state.unwrap_or_default();
                    st.scroll_to_cursor = true;
                    d.insert_temp(editor_id, st);
                });
            }
            IncomingFile::FromDisk(content, stamp) => {
                self.text = PieceTable::from_string(content);
                self.saved_snapshot = Some(self.text.snapshot());
                self.line_count = self.text.line_count();
                self.saved_revision = self.text_revision;
                self.is_current_dirty = false;
                self.cursor_line = 1;
                self.cursor_col = 1;
                self.selected_chars = 0;
                self.disk_stamp = stamp;
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

    /// Synchronizes the open document with disk when an external change is reported.
    ///
    /// Clean buffers reload in place while clamping the caret position; dirty buffers set
    /// `external_conflict` instead of overwriting unsaved user edits unless `force` is true.
    pub fn sync_with_disk(&mut self, ctx: &egui::Context, force: bool) -> DiskSyncResult {
        if self.path.as_os_str().is_empty() {
            return DiskSyncResult::Unchanged;
        }
        let path = self.path.clone();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !self.is_current_dirty {
                    self.close_if_deleted(ctx, &path, false);
                    ctx.request_repaint();
                    return DiskSyncResult::ClosedDeleted;
                }
                self.external_conflict = Some(ExternalConflict::DeletedOnDisk);
                ctx.request_repaint();
                return DiskSyncResult::Conflict(ExternalConflict::DeletedOnDisk);
            }
            Err(err) if is_transient_fs_error(&err) => {
                return DiskSyncResult::RetryTransientError;
            }
            Err(_) => return DiskSyncResult::Unchanged,
        };

        let stamp = DiskStamp {
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        if !force && self.disk_stamp.as_ref() == Some(&stamp) {
            return DiskSyncResult::Unchanged;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if !self.is_current_dirty {
                    self.close_if_deleted(ctx, &path, false);
                    ctx.request_repaint();
                    return DiskSyncResult::ClosedDeleted;
                }
                self.external_conflict = Some(ExternalConflict::DeletedOnDisk);
                ctx.request_repaint();
                return DiskSyncResult::Conflict(ExternalConflict::DeletedOnDisk);
            }
            Err(err) if is_transient_fs_error(&err) => {
                return DiskSyncResult::RetryTransientError;
            }
            Err(_) => return DiskSyncResult::Unchanged,
        };

        if self.text.eq_str(&content) {
            self.disk_stamp = Some(stamp);
            self.external_conflict = None;
            return DiskSyncResult::Unchanged;
        }

        if self.is_current_dirty && !force {
            self.external_conflict = Some(ExternalConflict::ModifiedOnDisk);
            ctx.request_repaint();
            return DiskSyncResult::Conflict(ExternalConflict::ModifiedOnDisk);
        }

        self.apply_disk_reload(ctx, content, Some(stamp));
        DiskSyncResult::Reloaded
    }

    fn apply_disk_reload(
        &mut self,
        ctx: &egui::Context,
        content: String,
        stamp: Option<DiskStamp>,
    ) {
        let prev_full_text: Arc<str> = self.text.with_str(|s| Arc::from(s));

        self.text = PieceTable::from_string(content);
        self.saved_snapshot = Some(self.text.snapshot());
        self.text_revision = self.text_revision.wrapping_add(1);
        self.saved_revision = self.text_revision;
        self.is_current_dirty = false;
        self.gutter_cache = None;
        self.last_cursor_status_key = None;
        self.line_count = self.text.line_count();
        self.disk_stamp = stamp;
        self.external_conflict = None;
        self.last_save_error = None;
        self.refresh_file_metadata();

        let editor_id = editor::code_editor_id();
        let total_chars = self.text.char_count();
        let mut st = ctx
            .data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id))
            .unwrap_or_default();
        let prev_range = st
            .cursor
            .char_range()
            .unwrap_or_else(|| egui::text::CCursorRange::one(egui::text::CCursor::new(0)));
        let primary_idx = prev_range.primary.index.0.min(total_chars);
        let secondary_idx = prev_range.secondary.index.0.min(total_chars);
        let clamped_range = egui::text::CCursorRange::two(
            egui::text::CCursor::new(secondary_idx),
            egui::text::CCursor::new(primary_idx),
        );
        st.cursor.set_char_range(Some(clamped_range));
        st.clear_caches();
        // Reset stale `PieceTableSnapshot` entries tied to the previous `PieceTable`'s `table_id`,
        // and seed a `FullText` checkpoint so `Ctrl+Z` can cleanly undo the reload.
        st.reset_undo_history();
        {
            let mut undoer = st.undoer.lock().unwrap_or_else(|e| e.into_inner());
            undoer.add_undo(&(prev_range, editor::UndoSnapshot::FullText(prev_full_text)));
        }
        st.last_undo_snapshot = Some(self.text.undo_snapshot());

        let (line, col) = self.text.char_to_line_col(primary_idx);
        self.folding
            .ensure_updated_buffer(&self.text, Some(self.text_revision));
        self.folding.unfold_containing(line);
        self.cursor_line = line + 1;
        self.cursor_col = col + 1;
        self.selected_chars = primary_idx.abs_diff(secondary_idx);
        ctx.data_mut(|d| d.insert_temp(editor_id, st));
        ctx.request_repaint();
    }

    pub fn close_if_deleted(&mut self, ctx: &egui::Context, path: &Path, is_dir: bool) {
        if is_dir {
            self.unsaved_files.retain(|k, _| !path_starts_with(k, path));
        } else {
            take_unsaved_entry(&mut self.unsaved_files, path);
        }

        if !self.path.as_os_str().is_empty() {
            let cur = &self.path;
            if paths_equal(cur, path) || (is_dir && path_starts_with(cur, path)) {
                self.path.clear();
                self.text.clear();
                self.saved_snapshot = Some(self.text.snapshot());
                self.is_current_dirty = false;
                self.text_revision = self.text_revision.wrapping_add(1);
                self.saved_revision = self.text_revision;
                self.gutter_cache = None;
                self.last_cursor_status_key = None;
                self.folding.clear();
                self.line_count = 1;
                self.cursor_line = 1;
                self.cursor_col = 1;
                self.selected_chars = 0;
                self.disk_stamp = None;
                self.external_conflict = None;
                self.last_save_error = None;
                self.refresh_file_metadata();

                let editor_id = editor::code_editor_id();
                ctx.data_mut(|d| {
                    d.insert_temp(editor_id, editor::FastEditorState::default());
                });
            }
        }
    }

    pub fn update_path_if_renamed(&mut self, old_path: &Path, new_path: &Path, is_dir: bool) {
        if !self.path.as_os_str().is_empty() {
            let cur = &self.path;
            if paths_equal(cur, old_path) {
                self.path = new_path.to_path_buf();
                self.refresh_file_metadata();
            } else if is_dir {
                if let Some(rel) = path_strip_prefix(cur, old_path) {
                    self.path = new_path.join(rel);
                    self.refresh_file_metadata();
                }
            }
        }

        if !self.unsaved_files.is_empty() {
            if is_dir {
                let affected: Vec<PathBuf> = self
                    .unsaved_files
                    .keys()
                    .filter(|k| path_starts_with(k, old_path))
                    .cloned()
                    .collect();
                for k in affected {
                    if let (Some(rel), Some(state)) =
                        (path_strip_prefix(&k, old_path), self.unsaved_files.remove(&k))
                    {
                        self.unsaved_files.insert(new_path.join(rel), state);
                    }
                }
            } else if let Some(state) = take_unsaved_entry(&mut self.unsaved_files, old_path) {
                self.unsaved_files.insert(new_path.to_path_buf(), state);
            }
        }
    }

    pub fn refresh_line_ending(&mut self) {
        self.line_ending = if self.text.has_crlf_in_initial_bytes(CRLF_DETECT_WINDOW) {
            "CRLF"
        } else {
            "LF"
        };
    }

    pub fn refresh_file_metadata(&mut self) {
        if self.path.as_os_str().is_empty() {
            self.file_name = "Untitled".to_string();
            self.language_ext = "txt".to_string();
        } else {
            let p = &self.path;
            self.file_name = p
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.path.to_string_lossy().into_owned());
            self.language_ext = p
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("txt")
                .to_string();
        }
        self.language_name = detect_language_name(&self.path.to_string_lossy());
        self.language_salt = hash_language_salt(&self.language_ext);
        self.folding
            .set_hash_comments(crate::folding::extension_uses_hash_comments(&self.language_ext));
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
            if bytes >= MB {
                let _ = write!(
                    s,
                    "{file_name} — {line_count} lines ({:.1} MB)",
                    bytes as f64 / MB as f64
                );
            } else if bytes >= 1024 {
                let _ = write!(
                    s,
                    "{file_name} — {line_count} lines ({:.1} KB)",
                    bytes as f64 / KB as f64
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
        let editor_id = editor::code_editor_id();
        if let Some(state) = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id)) {
            if let Some(range) = state.cursor.char_range() {
                if range.primary != range.secondary {
                    let [min_c, max_c] = range.sorted_cursors();
                    let count = max_c.index.0 - min_c.index.0;
                    if count > 0 && count <= MAX_FIND_SEED_CHARS {
                        let selected = self.text.slice_chars(min_c.index.0..max_c.index.0);
                        if !selected.is_empty()
                            && !selected.contains('\n')
                            && !selected.contains('\r')
                        {
                            self.find_query = selected;
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
        let editor_id = editor::code_editor_id();
        let mut state = ctx
            .data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id))
            .unwrap_or_default();

        let start_char_idx = state
            .cursor
            .char_range()
            .map(|r| r.primary.index.0.max(r.secondary.index.0))
            .unwrap_or(0);

        let start_byte = self.text.char_to_byte(start_char_idx);
        let match_byte = if let Some(mb) = self.text.find_from_byte(&self.find_query, start_byte) {
            mb
        } else if start_byte > 0 {
            match self.text.find_from_byte(&self.find_query, 0) {
                Some(mb) => mb,
                None => return false,
            }
        } else {
            return false;
        };

        let match_start_char = self.text.byte_to_char(match_byte);
        let match_end_char = match_start_char + self.find_query.chars().count();

        self.folding
            .ensure_updated_buffer(&self.text, Some(self.text_revision));
        let (start_line, _) = self.text.char_to_line_col(match_start_char);
        let (end_line, _) = self.text.char_to_line_col(match_end_char);
        for line in start_line..=end_line {
            self.folding.unfold_containing(line);
        }

        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(match_start_char),
            egui::text::CCursor::new(match_end_char),
        )));
        state.scroll_to_cursor = true;
        ctx.data_mut(|d| d.insert_temp(editor_id, state));
        editor::request_editor_focus(ctx, editor_id);
        ctx.request_repaint();
        true
    }

    fn current_cursor_doc_line(&self, ctx: &egui::Context) -> usize {
        let editor_id = editor::code_editor_id();
        if let Some(state) = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id)) {
            if let Some(range) = state.cursor.char_range() {
                return self.text.char_to_line_col(range.primary.index.0).0;
            }
        }
        self.cursor_line.saturating_sub(1)
    }

    fn clamp_cursor_out_of_hidden(&mut self, ctx: &egui::Context) {
        if !self.folding.has_active_folds() {
            return;
        }
        let editor_id = editor::code_editor_id();
        let Some(mut state) = ctx.data_mut(|d| d.get_temp::<editor::FastEditorState>(editor_id))
        else {
            return;
        };
        let Some(range) = state.cursor.char_range() else {
            return;
        };
        let (pri_line, _) = self.text.char_to_line_col(range.primary.index.0);
        let (sec_line, _) = self.text.char_to_line_col(range.secondary.index.0);
        let pri_hidden = self.folding.is_line_hidden(pri_line);
        let sec_hidden = self.folding.is_line_hidden(sec_line);
        if pri_hidden {
            let ancestor = self.folding.outermost_visible_ancestor(pri_line);
            let target_char = self.text.line_char_range(ancestor).end;
            let c = egui::text::CCursor::new(target_char);
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(c)));
            state.scroll_to_cursor = true;
            let (line, col) = self.text.char_to_line_col(target_char);
            self.cursor_line = line + 1;
            self.cursor_col = col + 1;
            self.selected_chars = 0;
            ctx.data_mut(|d| d.insert_temp(editor_id, state));
        } else if sec_hidden {
            let ancestor = self.folding.outermost_visible_ancestor(sec_line);
            let sec_target = self.text.line_char_range(ancestor).end;
            let new_sec = egui::text::CCursor::new(sec_target);
            state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                new_sec,
                range.primary,
            )));
            self.selected_chars = range.primary.index.0.abs_diff(sec_target);
            ctx.data_mut(|d| d.insert_temp(editor_id, state));
        }
    }

    /// Toggles the fold starting at 0-based `doc_line`.
    pub fn toggle_fold_at(&mut self, ctx: &egui::Context, doc_line: usize) -> bool {
        self.folding
            .ensure_updated_buffer(&self.text, Some(self.text_revision));
        if self.folding.toggle_fold(doc_line) {
            self.clamp_cursor_out_of_hidden(ctx);
            ctx.request_repaint();
            true
        } else {
            false
        }
    }

    /// Folds the region at the cursor line or its innermost enclosing foldable region.
    pub fn fold_current(&mut self, ctx: &egui::Context) -> bool {
        self.folding
            .ensure_updated_buffer(&self.text, Some(self.text_revision));
        let cur_line = self.current_cursor_doc_line(ctx);
        if self.folding.fold_at_or_enclosing(cur_line).is_some() {
            self.clamp_cursor_out_of_hidden(ctx);
            ctx.request_repaint();
            true
        } else {
            false
        }
    }

    /// Unfolds the region at the cursor line or its innermost enclosing folded region.
    pub fn unfold_current(&mut self, ctx: &egui::Context) -> bool {
        self.folding
            .ensure_updated_buffer(&self.text, Some(self.text_revision));
        let cur_line = self.current_cursor_doc_line(ctx);
        if self.folding.unfold_at_or_enclosing(cur_line).is_some() {
            ctx.request_repaint();
            true
        } else {
            false
        }
    }

    /// Collapses all foldable regions in the document.
    pub fn fold_all(&mut self, ctx: &egui::Context) -> bool {
        self.folding
            .ensure_updated_buffer(&self.text, Some(self.text_revision));
        if self.folding.fold_all() {
            self.clamp_cursor_out_of_hidden(ctx);
            ctx.request_repaint();
            true
        } else {
            false
        }
    }

    /// Expands all collapsed folds in the document.
    pub fn unfold_all(&mut self, ctx: &egui::Context) -> bool {
        if self.folding.unfold_all() {
            let editor_id = editor::code_editor_id();
            ctx.data_mut(|d| {
                if let Some(mut state) = d.get_temp::<editor::FastEditorState>(editor_id) {
                    state.scroll_to_cursor = true;
                    d.insert_temp(editor_id, state);
                }
            });
            ctx.request_repaint();
            true
        } else {
            false
        }
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
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(FIND_DIALOG_OFFSET_X, FIND_DIALOG_OFFSET_Y))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let find_id = egui::Id::unique("find_query_input");
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
                        .desired_width(FIND_INPUT_WIDTH)
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
                        || (enter_pressed && !self.find_query.is_empty())
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
            editor::request_editor_focus(ctx, editor::code_editor_id());
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

    /// Rebuilds the per-row logical line-number table only when the editor's galley changed.
    fn rebuild_gutter_cache(&mut self, ctx: &egui::Context, output: &editor::EditorOutput) {
        let stale = match &self.gutter_cache {
            Some((g, _)) => !Arc::ptr_eq(g, &output.galley),
            None => true,
        };
        if !stale {
            return;
        }
        let doc_lines = self.text.line_count().max(1);
        let has_folds = self.folding.has_active_folds();
        let mut nums = Vec::with_capacity(output.galley.rows.len());
        if output.virtualized {
            if has_folds {
                let vis = self.folding.visible_lines(doc_lines);
                for i in 0..output.galley.rows.len() {
                    let vis_idx = output.first_line + i;
                    let doc_line = vis.get(vis_idx).copied().unwrap_or(vis_idx);
                    nums.push((doc_line + 1) as u32);
                }
            } else {
                let base = output.first_line as u32;
                for i in 0..output.galley.rows.len() {
                    nums.push(base + i as u32 + 1);
                }
            }
        } else if has_folds {
            let vis = self.folding.visible_lines(doc_lines);
            let mut vis_idx = 0usize;
            let mut line_no = 1u32;
            let mut new_line = true;
            for row in &output.galley.rows {
                if new_line {
                    let doc_line = vis.get(vis_idx).copied().unwrap_or(vis_idx);
                    vis_idx += 1;
                    line_no = (doc_line + 1) as u32;
                    nums.push(line_no);
                } else {
                    nums.push(line_no | WRAPPED_ROW_FLAG);
                }
                new_line = row.ends_with_newline;
            }
        } else {
            let mut line_no = 0u32;
            let mut new_line = true;
            for row in &output.galley.rows {
                if new_line {
                    line_no += 1;
                    nums.push(line_no);
                } else {
                    nums.push(line_no | WRAPPED_ROW_FLAG);
                }
                new_line = row.ends_with_newline;
            }
        }
        let new_count = doc_lines;
        if new_count.max(1).ilog10() != self.line_count.max(1).ilog10() {
            ctx.request_repaint();
        }
        self.line_count = new_count;
        self.gutter_cache = Some((Arc::clone(&output.galley), nums));
    }

    /// Updates cursor line/column/selection status fields from the editor output.
    /// Skips scanning the buffer on idle frames where `(text_revision, cursor_range)` is unchanged.
    fn update_cursor_status(&mut self, ctx: &egui::Context, output: &editor::EditorOutput) {
        let Some(cursor_range) = output.cursor_range else {
            return;
        };
        let key = (self.text_revision, cursor_range);
        if !output.text_changed && self.last_cursor_status_key == Some(key) {
            return;
        }
        self.last_cursor_status_key = Some(key);
        let prev = (self.cursor_line, self.cursor_col, self.selected_chars);
        let (line, col) = self.text.char_to_line_col(cursor_range.primary.index.0);
        self.cursor_line = line + 1;
        self.cursor_col = col + 1;
        self.selected_chars = if cursor_range.is_empty() {
            0
        } else {
            cursor_range
                .primary
                .index
                .0
                .abs_diff(cursor_range.secondary.index.0)
        };
        if (self.cursor_line, self.cursor_col, self.selected_chars) != prev {
            ctx.request_repaint();
        }
    }

    /// Renders the save-error and external-conflict banners at the top of the editor panel.
    fn render_status_banners(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if let Some(err_msg) = self.last_save_error.as_deref() {
            let mut clear_err = false;
            ui.horizontal(|ui| {
                ui.colored_label(ui.visuals().error_fg_color, err_msg);
                if ui.button("Dismiss").clicked() {
                    clear_err = true;
                }
            });
            ui.separator();
            if clear_err {
                self.last_save_error = None;
            }
        }
        if let Some(conflict) = self.external_conflict {
            let mut do_reload = false;
            let mut do_close = false;
            let mut do_dismiss = false;
            ui.horizontal(|ui| {
                let msg = match conflict {
                    ExternalConflict::ModifiedOnDisk => {
                        "File changed on disk while you have unsaved edits."
                    }
                    ExternalConflict::DeletedOnDisk => {
                        "File was deleted on disk while you have unsaved edits."
                    }
                };
                ui.colored_label(ui.visuals().warn_fg_color, msg);
                match conflict {
                    ExternalConflict::ModifiedOnDisk => {
                        if ui.button("Reload from disk").clicked() {
                            do_reload = true;
                        }
                        if ui.button("Keep my edits").clicked() {
                            do_dismiss = true;
                        }
                    }
                    ExternalConflict::DeletedOnDisk => {
                        if ui.button("Close file").clicked() {
                            do_close = true;
                        }
                        if ui.button("Keep in editor").clicked() {
                            do_dismiss = true;
                        }
                    }
                }
            });
            ui.separator();
            if do_reload {
                self.sync_with_disk(ctx, true);
            } else if do_close {
                let p = self.path.clone();
                self.close_if_deleted(ctx, &p, false);
            } else if do_dismiss {
                self.external_conflict = None;
            }
        }
    }

    pub fn show_editor(&mut self, ui: &mut egui::Ui, panel_frame: egui::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let mut editor_text_changed = false;
        egui::CentralPanel::default().frame(panel_frame).show(ui, |ui| {
            self.render_status_banners(ui, ctx);

            let language_salt = self.language_salt;
            let text_revision = self.text_revision;

            let theme = CodeTheme::from_memory(ui.ctx(), ui.style());
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let text_color = ui.visuals().widgets.inactive.text_color();

            let available_size = ui.available_size();
            let is_large = self.text.len() > MAX_HIGHLIGHT_BYTES;
            egui::ScrollArea::vertical()
                .id_salt("editor_scroll_area")
                .auto_shrink([false, false])
                .animated(false)
                .show(ui, |ui| {
                    // Use the line count from the last galley rebuild instead of
                    // scanning the whole text every frame.
                    let line_count = self.line_count.max(1);
                    let digits = line_count.ilog10() as usize + 1;
                    let gutter_width = (digits.max(GUTTER_MIN_DIGITS) as f32) * GUTTER_DIGIT_WIDTH
                        + GUTTER_PADDING
                        + FOLD_GUTTER_WIDTH;

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

                        self.folding
                            .ensure_updated_buffer(&self.text, Some(text_revision));

                        let newline = if self.line_ending == "CRLF" { "\r\n" } else { "\n" };
                        let output = {
                            let language = self.language_ext.as_str();
                            let font_id_for_layout = font_id.clone();
                            let mut layouter = |ui: &egui::Ui, s: &str, wrap_width: f32| {
                                let layout_job = if s.len() <= MAX_HIGHLIGHT_BYTES {
                                    let mut job = syntax_highlighting::highlight(
                                        ui.ctx(),
                                        ui.style(),
                                        &theme,
                                        s,
                                        language,
                                    );
                                    job.wrap.max_width = wrap_width;
                                    job
                                } else {
                                    egui::text::LayoutJob::simple(
                                        s.to_owned(),
                                        font_id_for_layout.clone(),
                                        text_color,
                                        wrap_width,
                                    )
                                };
                                ui.fonts_mut(|f| f.layout_job(layout_job))
                            };
                            let mut show_fast_editor = |ui: &mut egui::Ui| {
                                FastEditor::new(
                                    editor::code_editor_id(),
                                    &mut self.text,
                                    editor_min_size,
                                    &mut layouter,
                                )
                                .cache_salt(language_salt)
                                .text_revision(text_revision)
                                .newline(newline)
                                .virtualized(is_large)
                                .fold_state(&mut self.folding)
                                .show(ui)
                            };
                            if is_large {
                                egui::ScrollArea::horizontal()
                                    .id_salt("editor_hscroll_area")
                                    .max_width(editor_min_size.x)
                                    .auto_shrink([false, false])
                                    .animated(false)
                                    .show(ui, |ui| show_fast_editor(ui))
                                    .inner
                            } else {
                                show_fast_editor(ui)
                            }
                        };
                        if self.focus_editor {
                            editor::request_editor_focus(ui.ctx(), output.response.id);
                            self.focus_editor = false;
                        }
                        editor_text_changed = output.text_changed;

                        self.rebuild_gutter_cache(ui.ctx(), &output);
                        self.update_cursor_status(ui.ctx(), &output);

                        let bottom_y = gutter_rect.max.y.max(output.response.rect.max.y);
                        let full_gutter_rect = egui::Rect::from_min_max(
                            gutter_rect.min,
                            egui::pos2(gutter_rect.max.x, bottom_y),
                        );
                        let gutter_resp = ui.interact(
                            full_gutter_rect,
                            egui::Id::unique("editor_gutter"),
                            egui::Sense::click(),
                        );

                        let clip_rect = ui.clip_rect();
                        let clicked_fold_line = paint_gutter(
                            ui,
                            &output.galley,
                            output.galley_pos,
                            clip_rect,
                            gutter_rect,
                            &gutter_resp,
                            self.gutter_cache.as_ref().map(|(_, n)| n.as_slice()),
                            self.cursor_line,
                            &self.folding,
                            &font_id,
                        );

                        let painter = ui.painter();
                        painter.line_segment(
                            [
                                egui::pos2(gutter_rect.max.x - GUTTER_SEPARATOR_OFFSET, gutter_rect.min.y),
                                egui::pos2(gutter_rect.max.x - GUTTER_SEPARATOR_OFFSET, bottom_y),
                            ],
                            ui.visuals().widgets.noninteractive.bg_stroke,
                        );

                        if let Some(doc_line) = clicked_fold_line {
                            self.toggle_fold_at(ctx, doc_line);
                            editor::request_editor_focus(ctx, editor::code_editor_id());
                        }
                    });
                });
        });

        if editor_text_changed {
            self.text_revision = self.text_revision.wrapping_add(1);
            self.last_cursor_status_key = None;
            if self
                .saved_snapshot
                .as_ref()
                .is_some_and(|snap| self.text.matches_snapshot(snap))
            {
                self.saved_revision = self.text_revision;
            }
            self.is_current_dirty = self.text_revision != self.saved_revision;
            ctx.request_repaint();
        }
    }
}

/// Paints visible line-number gutter rows and interactive fold chevrons without allocating
/// per-line strings. Returns `Some(doc_line)` (0-based) if the user clicked a fold chevron.
///
/// Binary-searches the first visible row (`partition_point`) and only draws rows
/// whose numbers are not wrapped-continuation rows (`WRAPPED_ROW_FLAG` unset).
#[allow(clippy::too_many_arguments)]
fn paint_gutter(
    ui: &egui::Ui,
    galley: &Arc<egui::Galley>,
    galley_pos: egui::Pos2,
    clip_rect: egui::Rect,
    gutter_rect: egui::Rect,
    gutter_resp: &egui::Response,
    nums: Option<&[u32]>,
    active_line: usize,
    folding: &FoldState,
    font_id: &egui::FontId,
) -> Option<usize> {
    let nums = nums?;
    let rows = &galley.rows;
    let painter = ui.painter();
    let visuals = ui.visuals();

    let num_right_x = gutter_rect.max.x - GUTTER_SEPARATOR_OFFSET - FOLD_GUTTER_WIDTH - GUTTER_TEXT_RIGHT_OFFSET;
    let fold_left_x = gutter_rect.max.x - GUTTER_SEPARATOR_OFFSET - FOLD_GUTTER_WIDTH;
    let fold_right_x = gutter_rect.max.x - GUTTER_SEPARATOR_OFFSET;

    let hover_pos = ui.ctx().pointer_hover_pos();
    let click_pos = if gutter_resp.clicked() {
        gutter_resp.interact_pointer_pos()
    } else {
        None
    };

    // Binary-search the first visible row; draw only visible rows.
    let top = clip_rect.min.y - galley_pos.y;
    let first = rows.partition_point(|r| r.rect().max.y < top);
    let mut num_buf = [0u8; 16];
    let mut clicked_fold_line = None;

    for (row_idx, row) in rows.iter().enumerate().skip(first) {
        let row_top = galley_pos.y + row.rect().min.y;
        if row_top > clip_rect.max.y {
            break;
        }
        let n = nums.get(row_idx).copied().unwrap_or(0);
        if n == 0 || (n & WRAPPED_ROW_FLAG) != 0 {
            continue;
        }
        let color = if n as usize == active_line {
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        };
        let label_str = format_u32_stack(n, &mut num_buf);
        painter.text(
            egui::pos2(num_right_x, row_top),
            egui::Align2::RIGHT_TOP,
            label_str,
            font_id.clone(),
            color,
        );

        let doc_line = (n - 1) as usize;
        if folding.fold_range_at(doc_line).is_some() {
            let row_bottom = galley_pos.y + row.rect().max.y;
            let hit_rect = egui::Rect::from_min_max(
                egui::pos2(fold_left_x, row_top),
                egui::pos2(fold_right_x, row_bottom),
            );
            let is_folded = folding.is_folded(doc_line);
            let is_hovered = hover_pos.is_some_and(|p| hit_rect.contains(p));
            if is_hovered {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                painter.rect_filled(hit_rect.shrink(1.0), 2.0, visuals.widgets.hovered.bg_fill);
            }
            if click_pos.is_some_and(|p| hit_rect.contains(p)) {
                clicked_fold_line = Some(doc_line);
            }

            let center = hit_rect.center();
            let r = 3.5_f32;
            let points = if is_folded {
                // Right-pointing triangle (collapsed)
                vec![
                    egui::pos2(center.x - r * 0.55, center.y - r),
                    egui::pos2(center.x + r * 0.85, center.y),
                    egui::pos2(center.x - r * 0.55, center.y + r),
                ]
            } else {
                // Downward-pointing triangle (expanded)
                vec![
                    egui::pos2(center.x - r, center.y - r * 0.55),
                    egui::pos2(center.x + r, center.y - r * 0.55),
                    egui::pos2(center.x, center.y + r * 0.85),
                ]
            };
            let tri_color = if is_folded || is_hovered {
                visuals.strong_text_color()
            } else {
                visuals.weak_text_color()
            };
            painter.add(egui::Shape::convex_polygon(
                points,
                tri_color,
                egui::Stroke::NONE,
            ));
        }
    }

    clicked_fold_line
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
