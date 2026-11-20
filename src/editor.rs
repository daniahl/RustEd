//! High-performance text editor widget (`FastEditor`).
//!
//! Custom multiline text editing component optimized for large files, with syntax layout caching,
//! undo/redo history, scroll management, and screen-magnifier/screen-reader accessibility (AccessKit).

use crate::{
    folding::{FoldState, FoldView, FoldedRowMarker},
    piece_table::{PieceTable, PieceTableSnapshot},
};
use eframe::egui::{
    self,
    epaint::text::Galley,
    text::{CCursor, LayoutJob},
    text_selection::{visuals::paint_text_cursor, CCursorRange, TextCursorState},
    Color32, Event, Id, Key, Pos2, Rect, Response, Sense, Ui, Vec2,
};
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

pub const CODE_EDITOR_ID: &str = "code_editor";
pub const TERMINAL_ID: &str = "conpty_terminal";

/// Inner margin around the editor text area.
pub const EDITOR_MARGIN: Vec2 = Vec2::new(6.0, 2.0);
/// Minimum editor wrap width in points.
pub const MIN_WRAP_WIDTH: f32 = 100.0;
/// Number of extra lines rendered above/below the viewport in virtualized mode.
pub const VIRTUALIZED_OVERSCAN: usize = 4;
/// Initial window size (in chars) for word-boundary scanning.
pub const WORD_SCAN_WINDOW: usize = 256;
/// Number of rows to jump for PageUp/PageDown.
pub const PAGE_JUMP_ROWS: usize = 25;
/// Tab insertion string (4 spaces).
pub const TAB_INDENT: &str = "    ";

pub fn code_editor_id() -> Id {
    Id::unique(CODE_EDITOR_ID)
}

pub fn terminal_id() -> Id {
    Id::unique(TERMINAL_ID)
}

pub fn default_focus_lock_filter() -> egui::EventFilter {
    egui::EventFilter {
        horizontal_arrows: true,
        vertical_arrows: true,
        tab: true,
        escape: true,
    }
}

pub fn request_editor_focus(ctx: &egui::Context, id: Id) {
    ctx.memory_mut(|m| {
        m.request_focus(id);
        m.set_focus_lock_filter(id, default_focus_lock_filter());
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoSnapshot {
    PieceTable(PieceTableSnapshot),
    FullText(Arc<str>),
}

/// Abstraction over editable text buffers (String, PieceTable).
///
/// Unlike `egui::TextBuffer`, this trait uses a closure-based `with_str` API
/// so that buffers with interior mutability (e.g. PieceTable's lazy cache)
/// can provide safe `&str` access without `unsafe` code.
pub trait EditorBuffer {
    /// Access the full text via a closure. Implementations may use a
    /// zero-copy borrow (single-piece) or a lazily-built cache.
    fn with_str<R>(&self, f: impl FnOnce(&str) -> R) -> R;

    /// Streams contiguous `&str` chunks of the buffer in order without requiring flattening.
    fn for_each_chunk(&self, mut f: impl FnMut(&str)) {
        self.with_str(|s| f(s));
    }

    /// Insert `text` at character index, returning chars inserted.
    fn insert_text(&mut self, text: &str, char_index: usize) -> usize;

    /// Delete the character range `start..end`.
    fn delete_char_range(&mut self, range: Range<usize>);

    /// Replace the entire buffer with `text`.
    fn replace_with(&mut self, text: &str);

    // --- Convenience methods (formerly from egui::TextBuffer) ---

    /// Delete the selected range, returning the collapsed cursor.
    fn delete_selected(&mut self, cursor_range: &CCursorRange) -> CCursor {
        let [min, max] = cursor_range.sorted_cursors();
        self.delete_char_range(min.index.0..max.index.0);
        min
    }

    /// Insert `text` at `ccursor`, advancing the cursor. Returns chars inserted.
    fn insert_text_at(&mut self, ccursor: &mut CCursor, text: &str, _max_len: usize) -> usize {
        let inserted = self.insert_text(text, ccursor.index.0);
        ccursor.index.0 += inserted;
        inserted
    }

    /// Delete the character before `ccursor` (Backspace).
    fn delete_previous_char(&mut self, ccursor: CCursor) -> CCursor {
        let idx = ccursor.index.0;
        if idx > 0 {
            self.delete_char_range((idx - 1)..idx);
            CCursor::new(idx - 1)
        } else {
            ccursor
        }
    }

    /// Delete the character at `ccursor` (Delete key).
    fn delete_next_char(&mut self, ccursor: CCursor) -> CCursor {
        let idx = ccursor.index.0;
        if idx < self.char_count() {
            self.delete_char_range(idx..(idx + 1));
        }
        ccursor
    }

    // --- Higher-level text queries ---

    fn undo_snapshot(&self) -> UndoSnapshot {
        UndoSnapshot::FullText(self.with_str(|s| Arc::from(s)))
    }

    fn restore_undo_snapshot(&mut self, snapshot: &UndoSnapshot) {
        if let UndoSnapshot::FullText(text) = snapshot {
            self.replace_with(text);
        }
    }

    fn char_count(&self) -> usize {
        self.with_str(|s| s.chars().count())
    }

    fn line_count(&self) -> usize {
        self.with_str(|s| s.bytes().filter(|&b| b == b'\n').count() + 1)
    }

    fn line_to_char(&self, line_idx: usize) -> usize {
        if line_idx == 0 {
            return 0;
        }
        self.with_str(|s| {
            let mut line = 0usize;
            let mut chars = 0usize;
            for c in s.chars() {
                chars += 1;
                if c == '\n' {
                    line += 1;
                    if line == line_idx {
                        return chars;
                    }
                }
            }
            chars
        })
    }

    fn char_to_line_col(&self, char_idx: usize) -> (usize, usize) {
        self.with_str(|s| {
            let mut line = 0usize;
            let mut col = 0usize;
            for (i, c) in s.chars().enumerate() {
                if i == char_idx {
                    return (line, col);
                }
                if c == '\n' {
                    line += 1;
                    col = 0;
                } else {
                    col += 1;
                }
            }
            (line, col)
        })
    }

    fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        let start = self.line_to_char(line_idx);
        let total_lines = self.line_count();
        let mut end = if line_idx + 1 >= total_lines {
            self.char_count()
        } else {
            self.line_to_char(line_idx + 1).saturating_sub(1)
        };
        if end > start && self.slice_chars(end - 1..end) == "\r" {
            end -= 1;
        }
        start..end.max(start)
    }

    fn extract_char_range(&self, char_range: Range<usize>, out: &mut String) {
        out.clear();
        self.with_str(|s| {
            for c in s.chars().skip(char_range.start).take(char_range.end.saturating_sub(char_range.start)) {
                out.push(c);
            }
        });
    }

    fn slice_chars(&self, char_range: Range<usize>) -> String {
        let mut s = String::new();
        self.extract_char_range(char_range, &mut s);
        s
    }

    fn prev_word_ccursor(&self, ccursor: CCursor) -> CCursor {
        let end = ccursor.index.0.min(self.char_count());
        if end == 0 {
            return CCursor::new(0);
        }
        let mut window = WORD_SCAN_WINDOW;
        let mut buf = String::new();
        loop {
            let start = end.saturating_sub(window);
            self.extract_char_range(start..end, &mut buf);
            let local_len = end - start;
            let new_local = egui::TextBuffer::delete_previous_word(&mut buf, CCursor::new(local_len));
            if new_local.index.0 > 0 || start == 0 {
                return CCursor::new(start + new_local.index.0);
            }
            window = window.saturating_mul(2);
        }
    }

    fn next_word_ccursor(&self, ccursor: CCursor) -> CCursor {
        let total = self.char_count();
        let start = ccursor.index.0.min(total);
        if start >= total {
            return CCursor::new(total);
        }
        let mut window = WORD_SCAN_WINDOW;
        let mut buf = String::new();
        loop {
            let end = (start + window).min(total);
            let local_len = end - start;
            self.extract_char_range(start..end, &mut buf);
            let _ = egui::TextBuffer::delete_next_word(&mut buf, CCursor::new(0));
            let remaining = buf.chars().count();
            let deleted = local_len.saturating_sub(remaining);
            if remaining > 0 || end == total {
                return CCursor::new(start + deleted);
            }
            window = window.saturating_mul(2);
        }
    }

    fn delete_prev_word(&mut self, ccursor: CCursor) -> CCursor {
        let target = self.prev_word_ccursor(ccursor);
        let end = ccursor.index.0.min(self.char_count());
        if target.index.0 < end {
            self.delete_char_range(target.index.0..end);
        }
        target
    }

    fn delete_nxt_word(&mut self, ccursor: CCursor) -> CCursor {
        let start = ccursor.index.0.min(self.char_count());
        let target = self.next_word_ccursor(ccursor);
        if target.index.0 > start {
            self.delete_char_range(start..target.index.0);
        }
        CCursor::new(start)
    }
}

impl EditorBuffer for String {
    fn with_str<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        f(self.as_str())
    }

    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        egui::TextBuffer::insert_text(self, text, egui::text::CharIndex(char_index))
    }

    fn delete_char_range(&mut self, range: Range<usize>) {
        egui::TextBuffer::delete_char_range(
            self,
            egui::text::CharIndex(range.start)..egui::text::CharIndex(range.end),
        );
    }

    fn replace_with(&mut self, text: &str) {
        egui::TextBuffer::replace_with(self, text);
    }
}

impl EditorBuffer for PieceTable {
    fn with_str<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        PieceTable::with_str(self, f)
    }

    fn for_each_chunk(&self, f: impl FnMut(&str)) {
        PieceTable::for_each_chunk(self, f);
    }

    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        PieceTable::insert_text(self, text, char_index)
    }

    fn delete_char_range(&mut self, range: Range<usize>) {
        PieceTable::delete_char_range(self, range);
    }

    fn replace_with(&mut self, text: &str) {
        PieceTable::replace_with(self, text);
    }

    fn undo_snapshot(&self) -> UndoSnapshot {
        UndoSnapshot::PieceTable(self.snapshot())
    }

    fn restore_undo_snapshot(&mut self, snapshot: &UndoSnapshot) {
        match snapshot {
            UndoSnapshot::PieceTable(snap) => {
                let _ = self.restore_snapshot(snap);
            }
            UndoSnapshot::FullText(text) => self.replace_with(text),
        }
    }

    fn char_count(&self) -> usize {
        PieceTable::char_count(self)
    }

    fn line_count(&self) -> usize {
        PieceTable::line_count(self)
    }

    fn line_to_char(&self, line_idx: usize) -> usize {
        PieceTable::line_to_char(self, line_idx)
    }

    fn char_to_line_col(&self, char_idx: usize) -> (usize, usize) {
        PieceTable::char_to_line_col(self, char_idx)
    }

    fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        PieceTable::line_char_range(self, line_idx)
    }

    fn extract_char_range(&self, char_range: Range<usize>, out: &mut String) {
        PieceTable::extract_char_range(self, char_range, out);
    }

    fn slice_chars(&self, char_range: Range<usize>) -> String {
        PieceTable::slice_chars(self, char_range)
    }

    fn delete_prev_word(&mut self, ccursor: CCursor) -> CCursor {
        let target_start = self.prev_word_ccursor(ccursor);
        let end = ccursor.index.0.min(self.char_count());
        if target_start.index.0 < end {
            self.delete_char_range(target_start.index.0..end);
        }
        target_start
    }

    fn delete_nxt_word(&mut self, ccursor: CCursor) -> CCursor {
        let start = ccursor.index.0.min(self.char_count());
        let target_end = self.next_word_ccursor(ccursor);
        if target_end.index.0 > start {
            self.delete_char_range(start..target_end.index.0);
        }
        CCursor::new(start)
    }
}

type Undoer = egui::util::undoer::Undoer<(CCursorRange, UndoSnapshot)>;

#[derive(Clone, Default)]
pub struct FastEditorState {
    pub cursor: TextCursorState,
    /// Mouse-only selection range used when `pointer_moves_caret` is `false` (terminal mode).
    pub mouse_selection: Option<CCursorRange>,
    pub undoer: Arc<Mutex<Undoer>>,
    pub last_undo_snapshot: Option<UndoSnapshot>,
    pub last_interaction_time: f64,
    /// Set after an edit / caret move until the caret has actually been scrolled into view.
    pub scroll_to_cursor: bool,
    /// Last layout result, reused on frames where text, width and theme are unchanged.
    pub layout_cache: Option<Arc<LayoutCache>>,
    /// Last windowed layout result for virtualized large-file mode.
    pub virtual_layout_cache: Option<Arc<VirtualLayoutCache>>,
    /// Last active-row AccessKit `TextRun` state, reused on frames where galley, active row,
    /// and clamped row bounds are unchanged so idle frames do not re-allocate or re-emit the row node.
    pub accesskit_row_cache: Option<AccessKitRowCache>,
    /// Tracks whether the editor had focus on the previous frame so we can force a
    /// `UIA_Text_TextSelectionChangedEventId` transition on the frame after focus is gained.
    pub had_focus_prev_frame: bool,
    /// Set when the editor just gained focus; causes one repaint where the selection is updated
    /// while `old.is_text_pattern_supported()` is already true.
    pub pending_accesskit_selection_sync: bool,
    /// Cumulative egui pass number when this editor was last rendered.
    /// If the editor was not rendered on the immediately preceding pass (e.g. user switched tabs),
    /// the AccessKit tree dropped this widget's children, so any cached AccessKit row node must be invalidated.
    pub last_rendered_pass: Option<u64>,
}

/// Cached identity of the active AccessKit `TextRun` row node.
#[derive(Clone)]
pub struct AccessKitRowCache {
    pub galley: Arc<Galley>,
    pub row: usize,
    pub bounds: egui::accesskit::Rect,
}

/// Cached galley for a given text + layout parameters.
pub struct LayoutCache {
    key: (u32, u32, bool, u64),
    text_revision: Option<u64>,
    fold_version: u64,
    galley: Arc<Galley>,
    fold_view: Option<Arc<FoldView>>,
}

/// Cached windowed galley for virtualized large-file mode.
pub struct VirtualLayoutCache {
    key: (u32, bool, u64),
    text_revision: u64,
    fold_version: u64,
    first_line: usize,
    last_line: usize,
    window_start_char: usize,
    window_end_char: usize,
    galley: Arc<Galley>,
    fold_view: Option<Arc<FoldView>>,
}

#[allow(dead_code)]
pub struct EditorOutput {
    pub response: Response,
    pub galley: Arc<Galley>,
    pub galley_pos: Pos2,
    pub text_clip_rect: Rect,
    pub cursor_range: Option<CCursorRange>,
    pub text_changed: bool,
    pub first_line: usize,
    pub virtualized: bool,
    /// Active visual row index in `galley` (0-based within `galley.rows`).
    pub active_galley_row: Option<usize>,
}

enum Layouter<'a, T> {
    Str(&'a mut dyn FnMut(&Ui, &str, f32) -> Arc<Galley>),
    Buffer(&'a mut dyn FnMut(&Ui, &T, f32) -> Arc<Galley>),
}

impl<'a, T: EditorBuffer> Layouter<'a, T> {
    fn layout(&mut self, ui: &Ui, buffer: &T, wrap_width: f32) -> Arc<Galley> {
        match self {
            Layouter::Str(f) => {
                // Str layouter expects &str; use with_str to get it safely
                buffer.with_str(|s| f(ui, s, wrap_width))
            }
            Layouter::Buffer(f) => f(ui, buffer, wrap_width),
        }
    }

    fn layout_str(&mut self, ui: &Ui, text: &str, wrap_width: f32) -> Arc<Galley> {
        match self {
            Layouter::Str(f) => f(ui, text, wrap_width),
            Layouter::Buffer(_) => {
                let font_id = egui::TextStyle::Monospace.resolve(ui.style());
                let text_color = ui.visuals().widgets.inactive.text_color();
                let job = LayoutJob::simple(text.to_owned(), font_id, text_color, wrap_width);
                ui.fonts_mut(|f| f.layout_job(job))
            }
        }
    }
}

fn load_editor_state(ui: &Ui, id: Id) -> (FastEditorState, u64) {
    let mut state = ui
        .ctx()
        .data_mut(|d| d.get_temp::<FastEditorState>(id))
        .unwrap_or_default();

    let current_pass = ui.ctx().cumulative_pass_nr();
    if state.last_rendered_pass != Some(current_pass.saturating_sub(1)) {
        state.accesskit_row_cache = None;
        state.had_focus_prev_frame = false;
    }
    // egui keeps the widget focused while the OS window is inactive, so a window
    // focus change never shows up as a widget focus change. Treat an inactive
    // window as "not focused" so that regaining window focus re-triggers the
    // AccessKit selection sync (UIA_Text_TextSelectionChangedEventId) that
    // magnifiers / screen readers need to resume caret tracking.
    let window_focused = ui.input(|i| i.focused);
    if !window_focused {
        state.accesskit_row_cache = None;
        state.had_focus_prev_frame = false;
    }

    (state, current_pass)
}

fn restore_focus_on_escape(ui: &Ui, response: &Response, id: Id) -> bool {
    let escape_without_popup =
        !ui.ctx().any_popup_open() && ui.input(|i| i.key_pressed(Key::Escape));
    if (response.has_focus() || escape_without_popup)
        && ui.memory(|m| m.had_focus_last_frame(id) && m.focused().is_none())
    {
        response.request_focus();
    }
    response.has_focus()
}

fn feed_undoer_if_in_flux<T: EditorBuffer>(
    state: &mut FastEditorState,
    text: &T,
    now: f64,
    cursor_range: CCursorRange,
) {
    let mut undoer = state.undoer.lock().unwrap_or_else(|e| e.into_inner());
    if undoer.is_in_flux() {
        let snap = match &state.last_undo_snapshot {
            Some(s) => s.clone(),
            None => {
                let s = text.undo_snapshot();
                state.last_undo_snapshot = Some(s.clone());
                s
            }
        };
        undoer.feed_state(now, &(cursor_range, snap));
    }
}

fn collect_relevant_events(ui: &Ui) -> Vec<Event> {
    ui.input(|i| {
        let has_relevant = i.events.iter().any(|e| {
            matches!(
                e,
                Event::Key { .. }
                    | Event::Text(_)
                    | Event::Paste(_)
                    | Event::Copy
                    | Event::Cut
            )
        });
        if has_relevant {
            i.events.clone()
        } else {
            Vec::new()
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn paint_selection_rows(
    painter: &egui::Painter,
    galley: &Galley,
    galley_pos: Pos2,
    text_clip_rect: Rect,
    bg_fill: Color32,
    sel_range: CCursorRange,
    window: Range<usize>,
    fold_view: Option<&FoldView>,
) {
    if galley.rows.is_empty() {
        return;
    }
    let [min_c, max_c] = sel_range.sorted_cursors();
    let (clamped_min, clamped_max, starts_before, ends_after) = if let Some(fv) = fold_view {
        let span = fv.doc_span();
        if max_c.index.0 <= span.start || min_c.index.0 > span.end {
            // Note: allow `min_c == span.end` when selection continues past the window end.
            if !(min_c.index.0 == span.end && max_c.index.0 > span.end) {
                return;
            }
        }
        let sb = min_c.index.0 < span.start;
        let ea = max_c.index.0 > span.end;
        let cmin = if sb { 0 } else { fv.doc_to_disp(min_c.index.0) };
        let cmax = if ea {
            fv.disp_len()
        } else {
            fv.doc_to_disp(max_c.index.0)
        };
        if cmin == cmax && !ea {
            return;
        }
        (cmin, cmax, sb, ea)
    } else {
        if max_c.index.0 <= window.start || min_c.index.0 >= window.end {
            return;
        }
        let sb = min_c.index.0 < window.start;
        let ea = max_c.index.0 > window.end;
        let cmin = min_c.index.0.max(window.start) - window.start;
        let cmax = max_c.index.0.min(window.end) - window.start;
        (cmin, cmax, sb, ea)
    };
    let min_cursor = galley.layout_from_cursor(CCursor::new(clamped_min));
    let max_cursor = galley.layout_from_cursor(CCursor::new(clamped_max));
    let min_row = min_cursor.row;
    let max_row = max_cursor.row.min(galley.rows.len() - 1);
    let top = text_clip_rect.min.y - galley_pos.y;
    let first_vis = galley.rows.partition_point(|r| r.rect().max.y < top);
    let start_row = min_row.max(first_vis);

    if start_row > max_row {
        return;
    }

    for (row_idx, row) in galley.rows[start_row..=max_row]
        .iter()
        .enumerate()
        .map(|(i, r)| (start_row + i, r))
    {
        let row_top_y = galley_pos.y + row.rect().min.y;
        if row_top_y > text_clip_rect.max.y {
            break;
        }
        let left = if row_idx == min_row && !starts_before {
            row.x_offset(min_cursor.column)
        } else {
            0.0
        };
        let right = if row_idx == max_row && !ends_after {
            row.x_offset(max_cursor.column)
        } else {
            let newline_extra = if row.ends_with_newline {
                row.rect().height() * 0.5
            } else {
                0.0
            };
            row.rect().width() + newline_extra
        };
        let sel_rect = Rect::from_min_max(
            galley_pos + Vec2::new(left, row.rect().min.y),
            galley_pos + Vec2::new(right, row.rect().max.y),
        );
        painter.rect_filled(sel_rect, 0.0, bg_fill);
    }
}

/// Computes the screen rect of the inline `...` fold badge for `marker`.
fn fold_badge_rect(
    galley: &Galley,
    galley_pos: Pos2,
    marker: &FoldedRowMarker,
    badge_width: f32,
) -> Option<Rect> {
    let row = galley.rows.get(marker.galley_row)?;
    let left = galley_pos.x + row.rect().max.x + 6.0;
    let top = galley_pos.y + row.rect().min.y + 1.0;
    let bottom = (galley_pos.y + row.rect().max.y - 1.0).max(top + 2.0);
    Some(Rect::from_min_max(
        Pos2::new(left, top),
        Pos2::new(left + badge_width, bottom),
    ))
}

/// Returns the 0-based doc line of a visible inline `...` fold badge containing `pos`, if any.
fn hit_test_fold_placeholder(
    galley: &Galley,
    galley_pos: Pos2,
    clip_rect: Rect,
    fold_view: &FoldView,
    pos: Pos2,
    badge_width: f32,
) -> Option<usize> {
    if fold_view.folded_row_markers.is_empty() || galley.rows.is_empty() {
        return None;
    }
    let top = clip_rect.min.y - galley_pos.y;
    let first_vis = galley.rows.partition_point(|r| r.rect().max.y < top);
    let start_idx = fold_view
        .folded_row_markers
        .partition_point(|m| m.galley_row < first_vis);
    for marker in &fold_view.folded_row_markers[start_idx..] {
        let Some(row) = galley.rows.get(marker.galley_row) else {
            break;
        };
        if galley_pos.y + row.rect().min.y > clip_rect.max.y {
            break;
        }
        if let Some(badge_rect) = fold_badge_rect(galley, galley_pos, marker, badge_width) {
            if badge_rect.contains(pos) {
                return Some(marker.doc_line);
            }
        }
    }
    None
}

/// Paints inline `...` fold badges for visible folded lines.
#[allow(clippy::too_many_arguments)]
fn paint_fold_placeholders(
    ui: &Ui,
    painter: &egui::Painter,
    galley: &Galley,
    galley_pos: Pos2,
    clip_rect: Rect,
    fold_view: &FoldView,
    font_id: &egui::FontId,
    badge_width: f32,
) {
    if fold_view.folded_row_markers.is_empty() || galley.rows.is_empty() {
        return;
    }
    let top = clip_rect.min.y - galley_pos.y;
    let first_vis = galley.rows.partition_point(|r| r.rect().max.y < top);
    let start_idx = fold_view
        .folded_row_markers
        .partition_point(|m| m.galley_row < first_vis);
    let hover_pos = ui.ctx().pointer_hover_pos();
    let visuals = ui.visuals();

    for marker in &fold_view.folded_row_markers[start_idx..] {
        let Some(row) = galley.rows.get(marker.galley_row) else {
            break;
        };
        if galley_pos.y + row.rect().min.y > clip_rect.max.y {
            break;
        }
        let Some(badge_rect) = fold_badge_rect(galley, galley_pos, marker, badge_width) else {
            continue;
        };
        let hovered = hover_pos.is_some_and(|p| badge_rect.contains(p));
        if hovered {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let bg = if hovered {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.faint_bg_color
        };
        let stroke = if hovered {
            visuals.widgets.hovered.bg_stroke
        } else {
            visuals.widgets.noninteractive.bg_stroke
        };
        let fg = if hovered {
            visuals.strong_text_color()
        } else {
            visuals.weak_text_color()
        };
        painter.rect(badge_rect, 3.0, bg, stroke, egui::StrokeKind::Inside);
        painter.text(
            badge_rect.center(),
            egui::Align2::CENTER_CENTER,
            "...",
            font_id.clone(),
            fg,
        );
    }
}

/// Determines the inclusive 0-based `(start_line, start_col, end_line)` potentially modified by
/// `event` before it runs.
fn edit_affected_lines<T: EditorBuffer>(
    text: &T,
    event: &Event,
    cursor_range: CCursorRange,
    fold_state: Option<&FoldState>,
) -> (usize, usize, usize) {
    let total = text.char_count();
    let [min_c, max_c] = cursor_range.sorted_cursors();
    let min_idx = min_c.index.0.min(total);
    let max_idx = max_c.index.0.min(total);
    let (mut start_line, mut start_col) = text.char_to_line_col(min_idx);
    let (mut end_line, _) = text.char_to_line_col(max_idx);

    if cursor_range.is_empty() {
        match event {
            Event::Key {
                key: Key::Backspace,
                pressed: true,
                modifiers,
                ..
            } => {
                if modifiers.ctrl {
                    let prev = text.prev_word_ccursor(cursor_range.primary).index.0;
                    (start_line, start_col) = text.char_to_line_col(prev);
                } else if start_col == 0 && start_line > 0 {
                    start_line -= 1;
                    start_col = usize::MAX;
                }
            }
            Event::Key {
                key: Key::Delete,
                pressed: true,
                modifiers,
                ..
            } => {
                if modifiers.ctrl {
                    let next = text.next_word_ccursor(cursor_range.primary).index.0;
                    end_line = text.char_to_line_col(next).0;
                } else {
                    let line_end = text.line_char_range(end_line).end;
                    if max_idx >= line_end {
                        end_line = end_line.saturating_add(1);
                    }
                }
            }
            Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            } if fold_state.is_some_and(|fs| fs.is_folded(end_line)) => {
                let line_end = text.line_char_range(end_line).end;
                if max_idx >= line_end {
                    end_line = end_line.saturating_add(1);
                }
            }
            _ => {}
        }
    }
    (start_line, start_col, end_line)
}

fn finish_cursor_and_scroll(
    ui: &Ui,
    state: &mut FastEditorState,
    cursor_box: Rect,
    text_clip_rect: Rect,
    has_focus: bool,
) -> bool {
    if has_focus {
        let now = ui.input(|i| i.time);
        // Use the full visible clip rect so the caret is never cut off at the edges.
        let cursor_painter = ui.painter().with_clip_rect(ui.clip_rect());
        paint_text_cursor(
            ui,
            &cursor_painter,
            cursor_box,
            now - state.last_interaction_time,
        );
    }

    let cursor_in_view = text_clip_rect.contains(cursor_box.center());
    if cursor_in_view {
        state.scroll_to_cursor = false;
        if has_focus && !ui.ctx().will_discard() && ui.input(|i| i.focused) {
            let ppp = ui.ctx().pixels_per_point();
            let x = (cursor_box.min.x * ppp).round() as i32;
            let y = (cursor_box.min.y * ppp).round() as i32;
            let w = ((cursor_box.width() * ppp).round() as i32).max(1);
            let h = ((cursor_box.height() * ppp).round() as i32).max(1);
            let force = !state.had_focus_prev_frame || state.pending_accesskit_selection_sync;
            crate::platform::update_caret(x, y, w, h, force);
        }
    } else if state.scroll_to_cursor {
        // Only redo the pass when we've just asked the ScrollArea to scroll to the
        // caret (edit / caret move). If the user simply scrolled the caret out of view,
        // discarding every frame would re-run layout continuously.
        if !ui.ctx().will_discard() {
            ui.ctx().request_discard("scroll to cursor");
        }
        // If the pass limit is hit, keep scrolling on the next frame.
        ui.ctx().request_repaint();
    }

    cursor_in_view
}

fn store_editor_state(
    ui: &Ui,
    id: Id,
    mut state: FastEditorState,
    current_pass: u64,
    has_focus: bool,
) {
    if !ui.ctx().will_discard() {
        state.had_focus_prev_frame = has_focus && ui.input(|i| i.focused);
    }
    state.last_rendered_pass = Some(current_pass);
    ui.ctx().data_mut(|d| d.insert_temp(id, state));

    if has_focus {
        ui.memory_mut(|mem| {
            mem.set_focus_lock_filter(id, default_focus_lock_filter());
        });
    }
}

pub struct FastEditor<'a, T: EditorBuffer = String> {
    id: Id,
    text: &'a mut T,
    min_size: Vec2,
    layouter: Layouter<'a, T>,
    cache_salt: u64,
    text_revision: Option<u64>,
    virtualized: bool,
    pointer_moves_caret: bool,
    read_only: bool,
    newline: &'static str,
    fold_state: Option<&'a mut FoldState>,
}

#[derive(Clone, Copy)]
enum EditOutcome {
    Unchanged,
    Edited(CCursorRange),
    Restored(CCursorRange),
}

impl<'a, T: EditorBuffer> FastEditor<'a, T> {
    pub fn new(
        id: Id,
        text: &'a mut T,
        min_size: Vec2,
        layouter: &'a mut dyn FnMut(&Ui, &str, f32) -> Arc<Galley>,
    ) -> Self {
        Self {
            id,
            text,
            min_size,
            layouter: Layouter::Str(layouter),
            cache_salt: 0,
            text_revision: None,
            virtualized: false,
            pointer_moves_caret: true,
            read_only: false,
            newline: "\n",
            fold_state: None,
        }
    }

    pub fn new_with_buffer(
        id: Id,
        text: &'a mut T,
        min_size: Vec2,
        layouter: &'a mut dyn FnMut(&Ui, &T, f32) -> Arc<Galley>,
    ) -> Self {
        Self {
            id,
            text,
            min_size,
            layouter: Layouter::Buffer(layouter),
            cache_salt: 0,
            text_revision: None,
            virtualized: false,
            pointer_moves_caret: true,
            read_only: false,
            newline: "\n",
            fold_state: None,
        }
    }

    /// Prevents text mutations (typing, paste, cut, delete, enter, tab, undo/redo) while still
    /// allowing copying. Used for terminal views where the PTY owns buffer contents.
    pub fn read_only(mut self, enabled: bool) -> Self {
        self.read_only = enabled;
        self
    }

    /// Attaches optional code-folding state to hide collapsed line ranges and auto-update folds on edit.
    pub fn fold_state(mut self, fold_state: &'a mut FoldState) -> Self {
        self.fold_state = Some(fold_state);
        self
    }

    /// Sets the line ending inserted when `Enter` is pressed (defaults to `"\n"`).
    pub fn newline(mut self, newline: &'static str) -> Self {
        self.newline = newline;
        self
    }

    /// Enables line-windowed virtualized layout (no word-wrap, only visible lines laid out).
    pub fn virtualized(mut self, enabled: bool) -> Self {
        self.virtualized = enabled;
        self
    }

    /// Controls whether pointer clicks and drags move the text caret.
    ///
    /// Set to `false` for terminal views where the caret position is controlled by the PTY.
    pub fn pointer_moves_caret(mut self, enabled: bool) -> Self {
        self.pointer_moves_caret = enabled;
        self
    }

    /// Extra value mixed into the layout-cache key. Change it whenever something the
    /// layouter depends on (e.g. syntax language) changes.
    #[allow(dead_code)]
    pub fn cache_salt(mut self, salt: u64) -> Self {
        self.cache_salt = salt;
        self
    }

    /// Optional monotonic revision number for `text`. When provided, layout cache
    /// hits check the revision in O(1) instead of comparing the full text buffer.
    ///
    /// Callers using `text_revision` must increment their stored revision by `wrapping_add(1)`
    /// whenever `EditorOutput::text_changed` is `true` so the updated cache entry matches on the
    /// next frame.
    #[allow(dead_code)]
    pub fn text_revision(mut self, revision: u64) -> Self {
        self.text_revision = Some(revision);
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_edit_outcome(
        &mut self,
        outcome: EditOutcome,
        now: f64,
        old_line_count: usize,
        affected_lines: (usize, usize, usize),
        state: &mut FastEditorState,
        cursor_range: &mut CCursorRange,
        text_changed: &mut bool,
        should_feed_undoer: &mut bool,
    ) -> bool {
        match outcome {
            EditOutcome::Edited(new_ccursor_range) => {
                *text_changed = true;
                *should_feed_undoer = true;
                state.last_interaction_time = now;
                *cursor_range = new_ccursor_range;
                if let Some(fs) = self.fold_state.as_deref_mut() {
                    let line_delta = self.text.line_count() as isize - old_line_count as isize;
                    fs.adjust_for_edit_with_col(
                        affected_lines.0,
                        affected_lines.1,
                        affected_lines.2,
                        line_delta,
                    );
                    fs.ensure_updated_buffer(self.text, None);
                    let cur_line = self.text.char_to_line_col(cursor_range.primary.index.0).0;
                    fs.unfold_containing(cur_line);
                }
                true
            }
            EditOutcome::Restored(new_ccursor_range) => {
                *text_changed = true;
                *should_feed_undoer = false;
                state.last_interaction_time = now;
                *cursor_range = new_ccursor_range;
                if let Some(fs) = self.fold_state.as_deref_mut() {
                    fs.ensure_updated_buffer(self.text, None);
                    let cur_line = self.text.char_to_line_col(cursor_range.primary.index.0).0;
                    fs.unfold_containing(cur_line);
                }
                true
            }
            EditOutcome::Unchanged => false,
        }
    }

    fn seed_pre_edit_snapshot(&self, state: &mut FastEditorState, cursor_range: CCursorRange) {
        if state.last_undo_snapshot.is_none() {
            let snap = self.text.undo_snapshot();
            state
                .undoer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .add_undo(&(cursor_range, snap.clone()));
            state.last_undo_snapshot = Some(snap);
        }
    }

    fn handle_common_edit_event(
        &mut self,
        ui: &Ui,
        event: &Event,
        state: &mut FastEditorState,
        cursor_range: CCursorRange,
        text_changed: bool,
    ) -> EditOutcome {
        if self.read_only && !matches!(event, Event::Copy) {
            return EditOutcome::Unchanged;
        }
        match event {
            Event::Copy => {
                let copy_range = if !self.pointer_moves_caret {
                    state.mouse_selection
                } else {
                    Some(cursor_range)
                };
                if let Some(range) = copy_range.filter(|r| r.primary != r.secondary) {
                    let [min_c, max_c] = range.sorted_cursors();
                    let total = self.text.char_count();
                    ui.ctx().copy_text(
                        self.text
                            .slice_chars(min_c.index.0.min(total)..max_c.index.0.min(total)),
                    );
                }
                EditOutcome::Unchanged
            }
            Event::Cut => {
                if cursor_range.is_empty() {
                    return EditOutcome::Unchanged;
                }
                if !text_changed {
                    self.seed_pre_edit_snapshot(state, cursor_range);
                }
                let [min_c, max_c] = cursor_range.sorted_cursors();
                let total = self.text.char_count();
                ui.ctx().copy_text(
                    self.text
                        .slice_chars(min_c.index.0.min(total)..max_c.index.0.min(total)),
                );
                let ccursor = self.text.delete_selected(&cursor_range);
                EditOutcome::Edited(CCursorRange::one(ccursor))
            }
            Event::Paste(text_to_insert) => {
                if text_to_insert.is_empty() {
                    return EditOutcome::Unchanged;
                }
                if !text_changed {
                    self.seed_pre_edit_snapshot(state, cursor_range);
                }
                let mut ccursor = self.text.delete_selected(&cursor_range);
                self.text
                    .insert_text_at(&mut ccursor, text_to_insert, usize::MAX);
                EditOutcome::Edited(CCursorRange::one(ccursor))
            }
            Event::Text(text_to_insert) => {
                // Enter/Return is handled by Key::Enter to avoid double newlines
                if text_to_insert.is_empty() || text_to_insert == "\n" || text_to_insert == "\r" {
                    return EditOutcome::Unchanged;
                }
                if !text_changed {
                    self.seed_pre_edit_snapshot(state, cursor_range);
                }
                let mut ccursor = self.text.delete_selected(&cursor_range);
                self.text
                    .insert_text_at(&mut ccursor, text_to_insert, usize::MAX);
                EditOutcome::Edited(CCursorRange::one(ccursor))
            }
            Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            } => {
                if !text_changed {
                    self.seed_pre_edit_snapshot(state, cursor_range);
                }
                let mut ccursor = self.text.delete_selected(&cursor_range);
                let (line_idx, col_idx) = self.text.char_to_line_col(ccursor.index.0);
                let line_range = self.text.line_char_range(line_idx);
                let prefix_end = (line_range.start + col_idx).min(line_range.end);
                let line_prefix = self.text.slice_chars(line_range.start..prefix_end);
                let indent: String = line_prefix
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                self.text
                    .insert_text_at(&mut ccursor, self.newline, usize::MAX);
                if !indent.is_empty() {
                    self.text
                        .insert_text_at(&mut ccursor, &indent, usize::MAX);
                }
                EditOutcome::Edited(CCursorRange::one(ccursor))
            }
            Event::Key {
                key: Key::Tab,
                pressed: true,
                modifiers,
                ..
            } if !modifiers.ctrl && !modifiers.command => {
                if !modifiers.shift {
                    if !text_changed {
                        self.seed_pre_edit_snapshot(state, cursor_range);
                    }
                    let mut ccursor = self.text.delete_selected(&cursor_range);
                    self.text.insert_text_at(&mut ccursor, TAB_INDENT, usize::MAX);
                    EditOutcome::Edited(CCursorRange::one(ccursor))
                } else {
                    let [min_c, max_c] = cursor_range.sorted_cursors();
                    let total = self.text.char_count();
                    let min_idx = min_c.index.0.min(total);
                    let max_idx = max_c.index.0.min(total);
                    let (start_line, _) = self.text.char_to_line_col(min_idx);
                    let (mut end_line, end_col) = self.text.char_to_line_col(max_idx);
                    if end_line > start_line && end_col == 0 {
                        end_line -= 1;
                    }

                    let mut new_primary = cursor_range.primary.index.0.min(total);
                    let mut new_secondary = cursor_range.secondary.index.0.min(total);
                    let mut any_deleted = false;

                    for line_idx in (start_line..=end_line).rev() {
                        let line_range = self.text.line_char_range(line_idx);
                        let check_end = (line_range.start + 4).min(line_range.end);
                        let prefix = self.text.slice_chars(line_range.start..check_end);
                        let remove_count = if prefix.starts_with('\t') {
                            1
                        } else {
                            prefix.chars().take(4).take_while(|c| *c == ' ').count()
                        };
                        if remove_count > 0 {
                            if !any_deleted && !text_changed {
                                self.seed_pre_edit_snapshot(state, cursor_range);
                            }
                            any_deleted = true;
                            self.text.delete_char_range(
                                line_range.start..(line_range.start + remove_count),
                            );
                            let del_start = line_range.start;
                            if new_primary > del_start {
                                new_primary = new_primary
                                    .saturating_sub(remove_count.min(new_primary - del_start));
                            }
                            if new_secondary > del_start {
                                new_secondary = new_secondary
                                    .saturating_sub(remove_count.min(new_secondary - del_start));
                            }
                        }
                    }

                    if any_deleted {
                        EditOutcome::Edited(CCursorRange::two(
                            CCursor::new(new_secondary),
                            CCursor::new(new_primary),
                        ))
                    } else {
                        EditOutcome::Unchanged
                    }
                }
            }
            Event::Key {
                key: Key::Backspace,
                pressed: true,
                modifiers,
                ..
            } => {
                if !text_changed {
                    self.seed_pre_edit_snapshot(state, cursor_range);
                }
                let ccursor = if let Some(cursor) = cursor_range.single() {
                    if modifiers.ctrl {
                        self.text.delete_prev_word(cursor)
                    } else {
                        let idx = cursor.index.0.min(self.text.char_count());
                        if idx >= 2 && self.text.slice_chars(idx - 2..idx) == "\r\n" {
                            self.text
                                .delete_char_range((idx - 2)..idx);
                            CCursor::new(idx - 2)
                        } else {
                            self.text.delete_previous_char(cursor)
                        }
                    }
                } else {
                    self.text.delete_selected(&cursor_range)
                };
                EditOutcome::Edited(CCursorRange::one(ccursor))
            }
            Event::Key {
                key: Key::Delete,
                pressed: true,
                modifiers,
                ..
            } => {
                if !text_changed {
                    self.seed_pre_edit_snapshot(state, cursor_range);
                }
                let ccursor = if let Some(cursor) = cursor_range.single() {
                    if modifiers.ctrl {
                        self.text.delete_nxt_word(cursor)
                    } else {
                        let idx = cursor.index.0;
                        let total = self.text.char_count();
                        if idx + 2 <= total && self.text.slice_chars(idx..idx + 2) == "\r\n" {
                            self.text
                                .delete_char_range(idx..(idx + 2));
                            CCursor::new(idx)
                        } else {
                            self.text.delete_next_char(cursor)
                        }
                    }
                } else {
                    self.text.delete_selected(&cursor_range)
                };
                EditOutcome::Edited(CCursorRange::one(ccursor))
            }
            Event::Key {
                key: Key::Z,
                pressed: true,
                modifiers,
                ..
            } if modifiers.command && !modifiers.shift => {
                let current_snap = match &state.last_undo_snapshot {
                    Some(s) if !text_changed => s.clone(),
                    _ => self.text.undo_snapshot(),
                };
                let undo_target = state
                    .undoer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .undo(&(cursor_range, current_snap))
                    .cloned();
                if let Some((undo_range, undo_snap)) = undo_target {
                    self.text.restore_undo_snapshot(&undo_snap);
                    state.last_undo_snapshot = Some(undo_snap);
                    EditOutcome::Restored(undo_range)
                } else {
                    EditOutcome::Unchanged
                }
            }
            Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if (*key == Key::Z && modifiers.command && modifiers.shift)
                || (*key == Key::Y && modifiers.command) =>
            {
                let current_snap = match &state.last_undo_snapshot {
                    Some(s) if !text_changed => s.clone(),
                    _ => self.text.undo_snapshot(),
                };
                let redo_target = state
                    .undoer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .redo(&(cursor_range, current_snap))
                    .cloned();
                if let Some((redo_range, redo_snap)) = redo_target {
                    self.text.restore_undo_snapshot(&redo_snap);
                    state.last_undo_snapshot = Some(redo_snap);
                    EditOutcome::Restored(redo_range)
                } else {
                    EditOutcome::Unchanged
                }
            }
            _ => EditOutcome::Unchanged,
        }
    }

    /// Moves the primary cursor `rows` visual rows up/down via `galley`, keeping
    /// the secondary cursor when `shift` is held (selection extension).
    fn page_move_cursor(
        galley: &Galley,
        cursor_range: &mut CCursorRange,
        rows: usize,
        down: bool,
        shift: bool,
    ) {
        let mut h_pos = None;
        for _ in 0..rows {
            if down {
                (cursor_range.primary, h_pos) =
                    galley.cursor_down_one_row(&cursor_range.primary, h_pos);
            } else {
                (cursor_range.primary, h_pos) =
                    galley.cursor_up_one_row(&cursor_range.primary, h_pos);
            }
        }
        if !shift {
            cursor_range.secondary = cursor_range.primary;
        }
    }

    fn layout_with_folds(
        &mut self,
        ui: &Ui,
        wrap_width: f32,
    ) -> (Arc<Galley>, Option<Arc<FoldView>>) {
        let has_folds = self
            .fold_state
            .as_deref()
            .is_some_and(FoldState::has_active_folds);
        if !has_folds {
            return (self.layouter.layout(ui, self.text, wrap_width), None);
        }
        let fold_state = self.fold_state.as_deref().unwrap();
        let (mut view, disp_text) = self
            .text
            .with_str(|s| FoldView::build_full(s, fold_state));
        let galley = self.layouter.layout_str(ui, &disp_text, wrap_width);
        view.populate_row_markers(&galley);
        (galley, Some(Arc::new(view)))
    }

    /// Standard pointer interaction (click & drag moves the text caret).
    #[allow(clippy::too_many_arguments)]
    fn handle_caret_pointer(
        &mut self,
        ui: &Ui,
        response: &Response,
        galley: &Galley,
        galley_pos: Pos2,
        state: &mut FastEditorState,
        cursor_range: &mut CCursorRange,
        fold_view: Option<&FoldView>,
    ) {
        if let Some(pointer_pos) = ui.ctx().pointer_interact_pos() {
            if response.hovered() || response.dragged() || response.clicked() {
                let is_being_dragged = ui.ctx().is_being_dragged(self.id);
                let cursor_at_pointer = galley.cursor_from_pos(pointer_pos - galley_pos);
                if let Some(fv) = fold_view {
                    let mut local_cursor_state = state.cursor;
                    let mapped_local = CCursorRange::two(
                        CCursor::new(fv.doc_to_disp(cursor_range.secondary.index.0)),
                        CCursor::new(fv.doc_to_disp(cursor_range.primary.index.0)),
                    );
                    let prev_local_sec = mapped_local.secondary.index.0;
                    local_cursor_state.set_char_range(Some(mapped_local));
                    let did_interact = local_cursor_state.pointer_interaction(
                        ui,
                        response,
                        cursor_at_pointer,
                        galley,
                        is_being_dragged,
                    );
                    if did_interact || response.clicked() {
                        response.request_focus();
                        state.last_interaction_time = ui.input(|i| i.time);
                        if let Some(r) = local_cursor_state.range(galley) {
                            let secondary = if is_being_dragged
                                && !response.clicked()
                                && r.secondary.index.0 == prev_local_sec
                            {
                                cursor_range.secondary
                            } else {
                                CCursor::new(fv.disp_to_doc(r.secondary.index.0))
                            };
                            *cursor_range = CCursorRange::two(
                                secondary,
                                CCursor::new(fv.disp_to_doc(r.primary.index.0)),
                            );
                        }
                        state.cursor = local_cursor_state;
                    }
                } else {
                    let did_interact = state.cursor.pointer_interaction(
                        ui,
                        response,
                        cursor_at_pointer,
                        galley,
                        is_being_dragged,
                    );
                    if did_interact || response.clicked() {
                        response.request_focus();
                        state.last_interaction_time = ui.input(|i| i.time);
                        if let Some(r) = state.cursor.range(galley) {
                            *cursor_range = r;
                        }
                    }
                }
            }
        }
    }

    /// Terminal-mode pointer interaction (selection without moving the PTY caret).
    /// Left-drag selects; copy is deferred to `Event::Copy`/secondary click.
    fn handle_selection_pointer(
        &mut self,
        ui: &Ui,
        response: &Response,
        galley: &Galley,
        galley_pos: Pos2,
        state: &mut FastEditorState,
    ) {
        if response.clicked() || response.secondary_clicked() {
            response.request_focus();
            state.last_interaction_time = ui.input(|i| i.time);
        }
        if response.secondary_clicked() {
            if let Some(sel) = state
                .mouse_selection
                .take()
                .filter(|sel| sel.primary != sel.secondary)
            {
                let [min_c, max_c] = sel.sorted_cursors();
                let total = self.text.char_count();
                let start = min_c.index.0.min(total);
                let end = max_c.index.0.min(total);
                if start < end {
                    ui.ctx().copy_text(self.text.slice_chars(start..end));
                }
            }
        } else if let Some(pointer_pos) = ui.ctx().pointer_interact_pos() {
            let primary_pressed = ui.input(|i| i.pointer.primary_pressed());
            let primary_down = ui.input(|i| i.pointer.primary_down());
            if response.hovered() && primary_pressed {
                response.request_focus();
                let c = galley.cursor_from_pos(pointer_pos - galley_pos);
                state.mouse_selection = Some(CCursorRange::one(c));
            } else if response.dragged_by(egui::PointerButton::Primary)
                || (ui.ctx().is_being_dragged(self.id) && primary_down)
            {
                if let Some(sel) = &mut state.mouse_selection {
                    let c = galley.cursor_from_pos(pointer_pos - galley_pos);
                    sel.primary = c;
                }
            }
        }
        if response.clicked()
            && state
                .mouse_selection
                .is_some_and(|sel| sel.primary == sel.secondary)
        {
            state.mouse_selection = None;
        }
    }

    pub fn show(mut self, ui: &mut Ui) -> EditorOutput {
        if self.virtualized {
            return self.show_virtualized(ui);
        }
        const MARGIN: Vec2 = EDITOR_MARGIN;
        let wrap_width = (ui.available_width() - MARGIN.x * 2.0).max(MIN_WRAP_WIDTH);
        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let char_width = ui.fonts_mut(|f| f.glyph_width(&font_id, ' ')).max(1.0);
        let badge_width = char_width * 3.0 + 10.0;

        let (mut state, current_pass) = load_editor_state(ui, self.id);

        if let Some(fs) = self.fold_state.as_deref_mut() {
            fs.ensure_updated_buffer(self.text, self.text_revision);
        }
        let fold_version = self
            .fold_state
            .as_deref()
            .map_or(0, FoldState::fold_version);

        // Reuse the previous layout when nothing it depends on has changed.
        // When `text_revision` is supplied, this is an O(1) integer comparison;
        // otherwise it falls back to comparing against the cached galley's text.
        let cache_key = (
            wrap_width.to_bits(),
            ui.ctx().pixels_per_point().to_bits(),
            ui.visuals().dark_mode,
            self.cache_salt,
        );
        let text_unchanged = match &state.layout_cache {
            Some(c) => match (self.text_revision, c.text_revision) {
                (Some(r1), Some(r2)) => r1 == r2,
                _ => {
                    if c.fold_view.is_some() {
                        false
                    } else {
                        self.text.with_str(|s| c.galley.text() == s)
                    }
                }
            },
            None => false,
        };
        if !text_unchanged {
            state.last_undo_snapshot = None;
        }
        let mut layout_dirty = false;
        let (mut galley, mut fold_view) = match &state.layout_cache {
            Some(c)
                if c.key == cache_key
                    && text_unchanged
                    && c.fold_version == fold_version =>
            {
                (Arc::clone(&c.galley), c.fold_view.as_ref().map(Arc::clone))
            }
            _ => {
                layout_dirty = true;
                self.layout_with_folds(ui, wrap_width)
            }
        };
        let desired_size = (galley.size() + MARGIN * 2.0).max(self.min_size);
        let (rect, _) = ui.allocate_exact_size(desired_size, Sense::hover());
        let response = ui.interact(rect, self.id, Sense::click_and_drag());
        let galley_pos = rect.min + MARGIN;

        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }

        let prev_cursor_range = state.cursor.char_range();
        let mut cursor_range = state
            .cursor
            .char_range()
            .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));

        // Check if the user clicked an inline `...` fold badge to unfold that region.
        let mut clicked_fold_badge = false;
        if response.clicked() {
            if let (Some(fv), Some(pointer_pos)) =
                (fold_view.as_deref(), ui.ctx().pointer_interact_pos())
            {
                let text_clip = ui.clip_rect().intersect(rect);
                if let Some(doc_line) = hit_test_fold_placeholder(
                    &galley,
                    galley_pos,
                    text_clip,
                    fv,
                    pointer_pos,
                    badge_width,
                ) {
                    if let Some(fs) = self.fold_state.as_deref_mut() {
                        if fs.unfold_line(doc_line) {
                            clicked_fold_badge = true;
                            response.request_focus();
                            let (g, fv_new) = self.layout_with_folds(ui, wrap_width);
                            galley = g;
                            fold_view = fv_new;
                            layout_dirty = true;
                            ui.ctx().request_repaint();
                        }
                    }
                }
            }
        }

        // Pointer interaction (mouse click & drag)
        if !clicked_fold_badge {
            if self.pointer_moves_caret {
                self.handle_caret_pointer(
                    ui,
                    &response,
                    &galley,
                    galley_pos,
                    &mut state,
                    &mut cursor_range,
                    fold_view.as_deref(),
                );
            } else {
                self.handle_selection_pointer(ui, &response, &galley, galley_pos, &mut state);
            }
        }

        let os = ui.ctx().os();
        let mut text_changed = false;
        let mut should_feed_undoer = false;
        // Keep editor focus if Escape was pressed while editor had focus on the previous frame
        let has_focus = restore_focus_on_escape(ui, &response, self.id);

        if has_focus {
            let now = ui.input(|i| i.time);

            // Feed state to undoer if in flux so undo points can stabilize.
            // Reuse the cached UndoSnapshot when available to avoid snapshotting on idle frames.
            feed_undoer_if_in_flux(&mut state, self.text, now, cursor_range);

            let events = collect_relevant_events(ui);
            let mut pending_relayout = false;

            for event in &events {
                // If an earlier event in this same frame mutated the text and this event
                // might depend on visual row layout, flush the pending relayout first.
                if pending_relayout
                    && matches!(
                        event,
                        Event::Key {
                            key: Key::ArrowUp
                                | Key::ArrowDown
                                | Key::ArrowLeft
                                | Key::ArrowRight
                                | Key::Home
                                | Key::End
                                | Key::PageUp
                                | Key::PageDown,
                            pressed: true,
                            ..
                        }
                    )
                {
                    let (g, fv_new) = self.layout_with_folds(ui, wrap_width);
                    galley = g;
                    fold_view = fv_new;
                    layout_dirty = true;
                    pending_relayout = false;
                }

                // In read-only terminal mode (`read_only && !pointer_moves_caret`), only Copy is
                // handled by FastEditor; caret movement is owned by the PTY.
                if self.read_only && !self.pointer_moves_caret {
                    let _ = self.handle_common_edit_event(
                        ui,
                        event,
                        &mut state,
                        cursor_range,
                        text_changed,
                    );
                    continue;
                }

                // Select All should always select the full document, even if trailing lines are folded.
                if matches!(
                    event,
                    Event::Key {
                        key: Key::A,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command
                ) {
                    let total = self.text.char_count();
                    cursor_range = CCursorRange::two(CCursor::new(0), CCursor::new(total));
                    state.last_interaction_time = now;
                    continue;
                }

                // Navigation & selection events (arrows, home, end, etc.)
                if let Some(fv) = fold_view.as_deref() {
                    let old_disp_sec = fv.doc_to_disp(cursor_range.secondary.index.0);
                    let mut disp_range = CCursorRange::two(
                        CCursor::new(old_disp_sec),
                        CCursor::new(fv.doc_to_disp(cursor_range.primary.index.0)),
                    );
                    if disp_range.on_event(os, event, &galley, self.id) {
                        let new_primary = CCursor::new(fv.disp_to_doc(disp_range.primary.index.0));
                        let new_secondary = if disp_range.secondary.index.0 == old_disp_sec {
                            cursor_range.secondary
                        } else {
                            CCursor::new(fv.disp_to_doc(disp_range.secondary.index.0))
                        };
                        cursor_range = CCursorRange::two(new_secondary, new_primary);
                        state.last_interaction_time = now;
                        continue;
                    }
                } else if cursor_range.on_event(os, event, &galley, self.id) {
                    state.last_interaction_time = now;
                    continue;
                }

                let old_line_count = if self.fold_state.is_some() {
                    self.text.line_count()
                } else {
                    0
                };
                let affected_lines = if self.fold_state.is_some() {
                    edit_affected_lines(self.text, event, cursor_range, self.fold_state.as_deref())
                } else {
                    (0, usize::MAX, 0)
                };

                let outcome = match event {
                    Event::Key {
                        key: Key::PageUp,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if let Some(fv) = fold_view.as_deref() {
                            let old_disp_sec = fv.doc_to_disp(cursor_range.secondary.index.0);
                            let mut disp_range = CCursorRange::two(
                                CCursor::new(old_disp_sec),
                                CCursor::new(fv.doc_to_disp(cursor_range.primary.index.0)),
                            );
                            Self::page_move_cursor(
                                &galley,
                                &mut disp_range,
                                PAGE_JUMP_ROWS,
                                false,
                                modifiers.shift,
                            );
                            let new_primary =
                                CCursor::new(fv.disp_to_doc(disp_range.primary.index.0));
                            let new_secondary = if modifiers.shift {
                                cursor_range.secondary
                            } else {
                                new_primary
                            };
                            cursor_range = CCursorRange::two(new_secondary, new_primary);
                        } else {
                            Self::page_move_cursor(
                                &galley,
                                &mut cursor_range,
                                PAGE_JUMP_ROWS,
                                false,
                                modifiers.shift,
                            );
                        }
                        state.last_interaction_time = now;
                        continue;
                    }
                    Event::Key {
                        key: Key::PageDown,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if let Some(fv) = fold_view.as_deref() {
                            let old_disp_sec = fv.doc_to_disp(cursor_range.secondary.index.0);
                            let mut disp_range = CCursorRange::two(
                                CCursor::new(old_disp_sec),
                                CCursor::new(fv.doc_to_disp(cursor_range.primary.index.0)),
                            );
                            Self::page_move_cursor(
                                &galley,
                                &mut disp_range,
                                PAGE_JUMP_ROWS,
                                true,
                                modifiers.shift,
                            );
                            let new_primary =
                                CCursor::new(fv.disp_to_doc(disp_range.primary.index.0));
                            let new_secondary = if modifiers.shift {
                                cursor_range.secondary
                            } else {
                                new_primary
                            };
                            cursor_range = CCursorRange::two(new_secondary, new_primary);
                        } else {
                            Self::page_move_cursor(
                                &galley,
                                &mut cursor_range,
                                PAGE_JUMP_ROWS,
                                true,
                                modifiers.shift,
                            );
                        }
                        state.last_interaction_time = now;
                        continue;
                    }
                    _ => self.handle_common_edit_event(
                        ui,
                        event,
                        &mut state,
                        cursor_range,
                        text_changed,
                    ),
                };

                if self.apply_edit_outcome(
                    outcome,
                    now,
                    old_line_count,
                    affected_lines,
                    &mut state,
                    &mut cursor_range,
                    &mut text_changed,
                    &mut should_feed_undoer,
                ) {
                    pending_relayout = true;
                }
            }

            if pending_relayout {
                let (g, fv_new) = self.layout_with_folds(ui, wrap_width);
                galley = g;
                fold_view = fv_new;
                layout_dirty = true;
            }
        }

        state.cursor.set_char_range(Some(cursor_range));

        if should_feed_undoer {
            let snap = self.text.undo_snapshot();
            state
                .undoer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .feed_state(
                ui.input(|i| i.time),
                &(cursor_range, snap.clone()),
            );
            state.last_undo_snapshot = Some(snap);
        }

        let text_clip_rect = ui.clip_rect().intersect(rect);
        let painter = ui.painter_at(rect);

        // Selection & cursor rendering
        // An empty galley has a zero-height row; give the caret box full line height so
        // scroll_to_rect, cursor_in_view, and caret painting use consistent bounds.
        let line_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let disp_primary = match fold_view.as_deref() {
            Some(fv) => CCursor::new(fv.doc_to_disp(cursor_range.primary.index.0)),
            None => cursor_range.primary,
        };
        let mut cursor_box = galley
            .pos_from_cursor(disp_primary)
            .translate(galley_pos.to_vec2());
        if cursor_box.height() < line_height {
            cursor_box.max.y = cursor_box.min.y + line_height;
        }
        let cursor_moved = state.cursor.char_range() != prev_cursor_range;

        if has_focus && (text_changed || cursor_moved) {
            state.scroll_to_cursor = true;
        }

        if state.scroll_to_cursor {
            ui.scroll_to_rect(cursor_box, None);
        }

        let selection_range = if !self.pointer_moves_caret {
            state
                .mouse_selection
                .filter(|r| r.primary != r.secondary)
                .map(|r| {
                    let total = self.text.char_count();
                    CCursorRange::two(
                        CCursor::new(r.secondary.index.0.min(total)),
                        CCursor::new(r.primary.index.0.min(total)),
                    )
                })
        } else if !cursor_range.is_empty() {
            Some(cursor_range)
        } else {
            None
        };

        if let Some(sel_range) = selection_range {
            // Paint selection rects directly for visible rows instead of calling
            // `paint_text_selection` on `galley`, which would deep-clone the entire
            // Galley via `Arc::make_mut` every frame.
            paint_selection_rows(
                &painter,
                &galley,
                galley_pos,
                text_clip_rect,
                ui.visuals().selection.bg_fill,
                sel_range,
                0..usize::MAX,
                fold_view.as_deref(),
            );
        }

        painter.galley(galley_pos, Arc::clone(&galley), Color32::WHITE);

        if let Some(fv) = fold_view.as_deref() {
            paint_fold_placeholders(
                ui,
                &painter,
                &galley,
                galley_pos,
                text_clip_rect,
                fv,
                &font_id,
                badge_width,
            );
        }

        let cursor_in_view =
            finish_cursor_and_scroll(ui, &mut state, cursor_box, text_clip_rect, has_focus);

        let focus = galley.layout_from_cursor(disp_primary);
        update_accesskit(
            ui,
            self.id,
            &mut state,
            &galley,
            galley_pos,
            rect,
            text_clip_rect,
            has_focus,
            cursor_in_view,
            focus.row,
            focus.row,
            focus.column.0,
        );

        let final_fold_version = self
            .fold_state
            .as_deref()
            .map_or(0, FoldState::fold_version);
        if text_changed {
            if let (Some(fs), Some(rev)) = (self.fold_state.as_deref_mut(), self.text_revision) {
                fs.set_cached_revision(rev.wrapping_add(1));
            }
        }
        if layout_dirty {
            let cached_rev = if text_changed {
                self.text_revision.map(|r| r.wrapping_add(1))
            } else {
                self.text_revision
            };
            state.layout_cache = Some(Arc::new(LayoutCache {
                key: cache_key,
                text_revision: cached_rev,
                fold_version: final_fold_version,
                galley: Arc::clone(&galley),
                fold_view,
            }));
        }
        store_editor_state(ui, self.id, state, current_pass, has_focus);

        EditorOutput {
            response,
            galley,
            galley_pos,
            text_clip_rect,
            cursor_range: Some(cursor_range),
            text_changed,
            first_line: 0,
            virtualized: false,
            active_galley_row: Some(focus.row),
        }
    }

    fn layout_virtual_window(
        &mut self,
        ui: &Ui,
        font_id: &egui::FontId,
        text_color: Color32,
        first_line: usize,
        last_line: usize,
        window_buf: &mut String,
    ) -> (Arc<Galley>, usize, usize, Option<Arc<FoldView>>) {
        let doc_total_lines = self.text.line_count().max(1);
        let total_chars = self.text.char_count();
        let has_folds = self
            .fold_state
            .as_deref()
            .is_some_and(FoldState::has_active_folds);

        if has_folds {
            let fs = self.fold_state.as_deref_mut().unwrap();
            let vis_lines = fs.visible_lines(doc_total_lines).to_vec();
            let (mut fv, disp_text) = FoldView::build_window(
                self.text,
                &vis_lines,
                first_line,
                last_line,
                fs,
            );
            let ws = fv.doc_span().start;
            let we = fv.doc_span().end;
            let job = LayoutJob::simple(disp_text, font_id.clone(), text_color, f32::INFINITY);
            let g = ui.fonts_mut(|f| f.layout_job(job));
            fv.populate_row_markers(&g);
            (g, ws, we, Some(Arc::new(fv)))
        } else {
            let ws = self.text.line_to_char(first_line);
            let we = if last_line >= doc_total_lines {
                total_chars
            } else {
                self.text.line_char_range(last_line - 1).end
            };
            self.text.extract_char_range(ws..we, window_buf);
            let job = LayoutJob::simple(
                std::mem::take(window_buf),
                font_id.clone(),
                text_color,
                f32::INFINITY,
            );
            let g = ui.fonts_mut(|f| f.layout_job(job));
            (g, ws, we, None)
        }
    }

    fn show_virtualized(mut self, ui: &mut Ui) -> EditorOutput {
        const MARGIN: Vec2 = EDITOR_MARGIN;
        const OVERSCAN: usize = VIRTUALIZED_OVERSCAN;

        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let text_color = ui.visuals().widgets.inactive.text_color();
        let line_height = ui.fonts_mut(|f| f.row_height(&font_id)).max(1.0);
        let char_width = ui.fonts_mut(|f| f.glyph_width(&font_id, ' ')).max(1.0);
        let badge_width = char_width * 3.0 + 10.0;

        let (mut state, current_pass) = load_editor_state(ui, self.id);

        if let Some(fs) = self.fold_state.as_deref_mut() {
            fs.ensure_updated_buffer(self.text, self.text_revision);
        }
        let fold_version = self
            .fold_state
            .as_deref()
            .map_or(0, FoldState::fold_version);

        let doc_total_lines = self.text.line_count().max(1);
        let total_lines = match self.fold_state.as_deref_mut() {
            Some(fs) if fs.has_active_folds() => fs.visible_lines(doc_total_lines).len().max(1),
            _ => doc_total_lines,
        };
        let total_chars = self.text.char_count();
        let total_height =
            (total_lines as f32 * line_height + MARGIN.y * 2.0).max(self.min_size.y);
        let alloc_width = ui.available_width().max(self.min_size.x);
        let desired_size = Vec2::new(alloc_width, total_height);

        let (rect, _) = ui.allocate_exact_size(desired_size, Sense::hover());
        let response = ui.interact(rect, self.id, Sense::click_and_drag());
        let base_pos = rect.min + MARGIN;

        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }

        let clip_rect = ui.clip_rect();
        let vis_top = (clip_rect.min.y - base_pos.y).max(0.0);
        let vis_bottom = (clip_rect.max.y - base_pos.y).max(0.0);
        let mut first_line = ((vis_top / line_height).floor() as usize)
            .saturating_sub(OVERSCAN)
            .min(total_lines - 1);
        let mut last_line = (((vis_bottom / line_height).ceil() as usize) + 1 + OVERSCAN)
            .min(total_lines)
            .max(first_line + 1);
        let window_galley_pos = base_pos + Vec2::new(0.0, first_line as f32 * line_height);

        let cache_key = (
            ui.ctx().pixels_per_point().to_bits(),
            ui.visuals().dark_mode,
            self.cache_salt,
        );
        let rev = self.text_revision.unwrap_or(0);
        let text_unchanged = match &state.virtual_layout_cache {
            Some(c) => self.text_revision.is_some() && c.text_revision == rev,
            None => false,
        };
        if !text_unchanged {
            state.last_undo_snapshot = None;
        }
        let mut window_buf = String::new();

        let (mut galley, mut window_start_char, mut window_end_char, mut fold_view, mut layout_dirty) =
            match &state.virtual_layout_cache {
                Some(c)
                    if c.key == cache_key
                        && text_unchanged
                        && c.fold_version == fold_version
                        && c.first_line == first_line
                        && c.last_line == last_line =>
                {
                    (
                        Arc::clone(&c.galley),
                        c.window_start_char,
                        c.window_end_char,
                        c.fold_view.as_ref().map(Arc::clone),
                        false,
                    )
                }
                _ => {
                    let (g, ws, we, fv) = self.layout_virtual_window(
                        ui,
                        &font_id,
                        text_color,
                        first_line,
                        last_line,
                        &mut window_buf,
                    );
                    (g, ws, we, fv, true)
                }
            };

        let prev_ccursor_range = state
            .cursor
            .char_range()
            .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
        let mut ccursor_range = CCursorRange::two(
            CCursor::new(prev_ccursor_range.secondary.index.0.min(total_chars)),
            CCursor::new(prev_ccursor_range.primary.index.0.min(total_chars)),
        );

        // Check if user clicked an inline `...` fold placeholder badge to unfold it.
        let mut clicked_fold_badge = false;
        if response.clicked() {
            if let (Some(fv), Some(pointer_pos)) =
                (fold_view.as_deref(), ui.ctx().pointer_interact_pos())
            {
                let text_clip = ui.clip_rect().intersect(rect);
                if let Some(doc_line) = hit_test_fold_placeholder(
                    &galley,
                    window_galley_pos,
                    text_clip,
                    fv,
                    pointer_pos,
                    badge_width,
                ) {
                    if let Some(fs) = self.fold_state.as_deref_mut() {
                        if fs.unfold_line(doc_line) {
                            clicked_fold_badge = true;
                            response.request_focus();
                            let new_total_vis = if fs.has_active_folds() {
                                fs.visible_lines(doc_total_lines).len().max(1)
                            } else {
                                doc_total_lines
                            };
                            first_line = first_line.min(new_total_vis - 1);
                            last_line = last_line.min(new_total_vis).max(first_line + 1);
                            let (g, ws, we, fv_new) = self.layout_virtual_window(
                                ui,
                                &font_id,
                                text_color,
                                first_line,
                                last_line,
                                &mut window_buf,
                            );
                            galley = g;
                            window_start_char = ws;
                            window_end_char = we;
                            fold_view = fv_new;
                            layout_dirty = true;
                            ui.ctx().request_repaint();
                        }
                    }
                }
            }
        }

        // Pointer interaction inside the visible window
        if !clicked_fold_badge {
            if self.pointer_moves_caret {
                if let Some(pointer_pos) = ui.ctx().pointer_interact_pos() {
                    if response.hovered() || response.dragged() || response.clicked() {
                        let is_being_dragged = ui.ctx().is_being_dragged(self.id);
                        let cursor_at_pointer =
                            galley.cursor_from_pos(pointer_pos - window_galley_pos);

                        let mut local_cursor_state = state.cursor;
                        let clamped_local = if let Some(fv) = fold_view.as_deref() {
                            CCursorRange::two(
                                CCursor::new(fv.doc_to_disp(ccursor_range.secondary.index.0)),
                                CCursor::new(fv.doc_to_disp(ccursor_range.primary.index.0)),
                            )
                        } else {
                            CCursorRange::two(
                                CCursor::new(
                                    ccursor_range
                                        .secondary
                                        .index
                                        .0
                                        .saturating_sub(window_start_char)
                                        .min(window_end_char - window_start_char),
                                ),
                                CCursor::new(
                                    ccursor_range
                                        .primary
                                        .index
                                        .0
                                        .saturating_sub(window_start_char)
                                        .min(window_end_char - window_start_char),
                                ),
                            )
                        };
                        let prev_clamped_sec = clamped_local.secondary.index.0;
                        local_cursor_state.set_char_range(Some(clamped_local));

                        let did_interact = local_cursor_state.pointer_interaction(
                            ui,
                            &response,
                            cursor_at_pointer,
                            &galley,
                            is_being_dragged,
                        );
                        if did_interact || response.clicked() {
                            response.request_focus();
                            state.last_interaction_time = ui.input(|i| i.time);
                            if let Some(r) = local_cursor_state.char_range() {
                                let secondary = if is_being_dragged
                                    && !response.clicked()
                                    && r.secondary.index.0 == prev_clamped_sec
                                {
                                    ccursor_range.secondary
                                } else if let Some(fv) = fold_view.as_deref() {
                                    CCursor::new(fv.disp_to_doc(r.secondary.index.0).min(total_chars))
                                } else {
                                    CCursor::new(
                                        (window_start_char + r.secondary.index.0).min(total_chars),
                                    )
                                };
                                let primary = if let Some(fv) = fold_view.as_deref() {
                                    CCursor::new(fv.disp_to_doc(r.primary.index.0).min(total_chars))
                                } else {
                                    CCursor::new(
                                        (window_start_char + r.primary.index.0).min(total_chars),
                                    )
                                };
                                ccursor_range = CCursorRange::two(secondary, primary);
                            }
                            state.cursor = local_cursor_state;
                        }
                    }
                }
            } else if response.clicked() || response.secondary_clicked() {
                response.request_focus();
                state.last_interaction_time = ui.input(|i| i.time);
            }
        }

        let mut text_changed = false;
        let mut should_feed_undoer = false;
        let has_focus = restore_focus_on_escape(ui, &response, self.id);

        if has_focus {
            let now = ui.input(|i| i.time);
            feed_undoer_if_in_flux(&mut state, self.text, now, ccursor_range);

            let events = collect_relevant_events(ui);

            for event in &events {
                if self.read_only && !self.pointer_moves_caret {
                    let _ = self.handle_common_edit_event(
                        ui,
                        event,
                        &mut state,
                        ccursor_range,
                        text_changed,
                    );
                    continue;
                }
                let cur_total = self.text.char_count();

                match event {
                    Event::Key {
                        key: Key::A,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        ccursor_range =
                            CCursorRange::two(CCursor::new(0), CCursor::new(cur_total));
                        state.last_interaction_time = now;
                    }
                    Event::Key {
                        key: Key::ArrowLeft,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let next = if !modifiers.shift
                            && !(modifiers.ctrl || modifiers.command)
                            && !ccursor_range.is_empty()
                        {
                            ccursor_range.sorted_cursors()[0].index.0.min(cur_total)
                        } else if modifiers.ctrl || modifiers.command {
                            let mut target =
                                self.text.prev_word_ccursor(ccursor_range.primary).index.0;
                            if let Some(fs) = self.fold_state.as_deref() {
                                if fs.has_active_folds() {
                                    let (target_line, _) = self.text.char_to_line_col(target);
                                    if fs.is_line_hidden(target_line) {
                                        let ancestor = fs.outermost_visible_ancestor(target_line);
                                        target = self.text.line_char_range(ancestor).end;
                                    }
                                }
                            }
                            target
                        } else {
                            let cur = ccursor_range.primary.index.0.min(cur_total);
                            let mut jumped = None;
                            if let Some(fs) = self.fold_state.as_deref_mut() {
                                if fs.has_active_folds() {
                                    let (line, col) = self.text.char_to_line_col(cur);
                                    if col == 0 {
                                        let total_l = self.text.line_count().max(1);
                                        let vis = fs.visible_lines(total_l);
                                        let vis_idx = vis.partition_point(|&l| l < line);
                                        if vis_idx > 0 {
                                            let prev_vis_line = vis[vis_idx - 1];
                                            jumped = Some(self.text.line_char_range(prev_vis_line).end);
                                        }
                                    }
                                }
                            }
                            if let Some(j) = jumped {
                                j
                            } else if cur >= 2 && self.text.slice_chars(cur - 2..cur) == "\r\n" {
                                cur - 2
                            } else {
                                cur.saturating_sub(1)
                            }
                        };
                        ccursor_range.primary = CCursor::new(next);
                        if !modifiers.shift {
                            ccursor_range.secondary = ccursor_range.primary;
                        }
                        state.last_interaction_time = now;
                    }
                    Event::Key {
                        key: Key::ArrowRight,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let next = if !modifiers.shift
                            && !(modifiers.ctrl || modifiers.command)
                            && !ccursor_range.is_empty()
                        {
                            ccursor_range.sorted_cursors()[1].index.0.min(cur_total)
                        } else if modifiers.ctrl || modifiers.command {
                            let mut target =
                                self.text.next_word_ccursor(ccursor_range.primary).index.0;
                            if let Some(fs) = self.fold_state.as_deref_mut() {
                                if fs.has_active_folds() {
                                    let (target_line, _) = self.text.char_to_line_col(target);
                                    if fs.is_line_hidden(target_line) {
                                        let total_l = self.text.line_count().max(1);
                                        let vis = fs.visible_lines(total_l);
                                        let vis_idx = vis.partition_point(|&l| l < target_line);
                                        if let Some(&next_vis) = vis.get(vis_idx) {
                                            target = self.text.line_char_range(next_vis).start;
                                        } else {
                                            let ancestor =
                                                fs.outermost_visible_ancestor(target_line);
                                            target = self.text.line_char_range(ancestor).end;
                                        }
                                    }
                                }
                            }
                            target
                        } else {
                            let cur = ccursor_range.primary.index.0.min(cur_total);
                            let mut jumped = None;
                            if let Some(fs) = self.fold_state.as_deref_mut() {
                                if fs.has_active_folds() {
                                    let (line, _) = self.text.char_to_line_col(cur);
                                    let line_end = self.text.line_char_range(line).end;
                                    if cur >= line_end {
                                        let total_l = self.text.line_count().max(1);
                                        let vis = fs.visible_lines(total_l);
                                        let vis_idx = vis.partition_point(|&l| l < line);
                                        if vis_idx + 1 < vis.len() {
                                            let next_vis_line = vis[vis_idx + 1];
                                            jumped = Some(self.text.line_char_range(next_vis_line).start);
                                        } else {
                                            jumped = Some(line_end);
                                        }
                                    }
                                }
                            }
                            if let Some(j) = jumped {
                                j
                            } else if cur + 2 <= cur_total
                                && self.text.slice_chars(cur..cur + 2) == "\r\n"
                            {
                                cur + 2
                            } else {
                                (cur + 1).min(cur_total)
                            }
                        };
                        ccursor_range.primary = CCursor::new(next);
                        if !modifiers.shift {
                            ccursor_range.secondary = ccursor_range.primary;
                        }
                        state.last_interaction_time = now;
                    }
                    Event::Key {
                        key: key @ (Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown),
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let (line, col) = self.text.char_to_line_col(ccursor_range.primary.index.0);
                        let lines_total = self.text.line_count().max(1);
                        let target_line = if let Some(fs) = self.fold_state.as_deref_mut() {
                            if fs.has_active_folds() {
                                let vis = fs.visible_lines(lines_total);
                                let cur_vis = vis
                                    .partition_point(|&l| l < line)
                                    .min(vis.len().saturating_sub(1));
                                let target_vis = match key {
                                    Key::ArrowUp => cur_vis.saturating_sub(1),
                                    Key::ArrowDown => (cur_vis + 1).min(vis.len() - 1),
                                    Key::PageUp => cur_vis.saturating_sub(PAGE_JUMP_ROWS),
                                    Key::PageDown => {
                                        (cur_vis + PAGE_JUMP_ROWS).min(vis.len() - 1)
                                    }
                                    _ => cur_vis,
                                };
                                vis[target_vis]
                            } else {
                                match key {
                                    Key::ArrowUp => line.saturating_sub(1),
                                    Key::ArrowDown => (line + 1).min(lines_total - 1),
                                    Key::PageUp => line.saturating_sub(PAGE_JUMP_ROWS),
                                    Key::PageDown => {
                                        (line + PAGE_JUMP_ROWS).min(lines_total - 1)
                                    }
                                    _ => line,
                                }
                            }
                        } else {
                            match key {
                                Key::ArrowUp => line.saturating_sub(1),
                                Key::ArrowDown => (line + 1).min(lines_total - 1),
                                Key::PageUp => line.saturating_sub(PAGE_JUMP_ROWS),
                                Key::PageDown => (line + PAGE_JUMP_ROWS).min(lines_total - 1),
                                _ => line,
                            }
                        };
                        let range = self.text.line_char_range(target_line);
                        let next = (range.start + col).min(range.end);
                        ccursor_range.primary = CCursor::new(next);
                        if !modifiers.shift {
                            ccursor_range.secondary = ccursor_range.primary;
                        }
                        state.last_interaction_time = now;
                    }
                    Event::Key {
                        key: Key::Home,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let next = if modifiers.ctrl || modifiers.command {
                            0
                        } else {
                            let (line, _) = self.text.char_to_line_col(ccursor_range.primary.index.0);
                            self.text.line_char_range(line).start
                        };
                        ccursor_range.primary = CCursor::new(next);
                        if !modifiers.shift {
                            ccursor_range.secondary = ccursor_range.primary;
                        }
                        state.last_interaction_time = now;
                    }
                    Event::Key {
                        key: Key::End,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let next = if modifiers.ctrl || modifiers.command {
                            if let Some(fs) = self.fold_state.as_deref_mut() {
                                if fs.has_active_folds() {
                                    let total_l = self.text.line_count().max(1);
                                    let last_vis =
                                        *fs.visible_lines(total_l).last().unwrap_or(&0);
                                    self.text.line_char_range(last_vis).end
                                } else {
                                    cur_total
                                }
                            } else {
                                cur_total
                            }
                        } else {
                            let (line, _) = self.text.char_to_line_col(ccursor_range.primary.index.0);
                            self.text.line_char_range(line).end
                        };
                        ccursor_range.primary = CCursor::new(next);
                        if !modifiers.shift {
                            ccursor_range.secondary = ccursor_range.primary;
                        }
                        state.last_interaction_time = now;
                    }
                    _ => {
                        let old_line_count = if self.fold_state.is_some() {
                            self.text.line_count()
                        } else {
                            0
                        };
                        let affected_lines = if self.fold_state.is_some() {
                            edit_affected_lines(
                                self.text,
                                event,
                                ccursor_range,
                                self.fold_state.as_deref(),
                            )
                        } else {
                            (0, usize::MAX, 0)
                        };
                        let outcome = self.handle_common_edit_event(
                            ui,
                            event,
                            &mut state,
                            ccursor_range,
                            text_changed,
                        );
                        self.apply_edit_outcome(
                            outcome,
                            now,
                            old_line_count,
                            affected_lines,
                            &mut state,
                            &mut ccursor_range,
                            &mut text_changed,
                            &mut should_feed_undoer,
                        );
                    }
                }
            }
        }

        let total_lines_after = self.text.line_count().max(1);
        let total_chars_after = self.text.char_count();
        ccursor_range.primary.index.0 = ccursor_range.primary.index.0.min(total_chars_after);
        ccursor_range.secondary.index.0 = ccursor_range.secondary.index.0.min(total_chars_after);

        if text_changed {
            let new_total_vis = match self.fold_state.as_deref_mut() {
                Some(fs) if fs.has_active_folds() => {
                    fs.visible_lines(total_lines_after).len().max(1)
                }
                _ => total_lines_after,
            };
            first_line = first_line.min(new_total_vis - 1);
            last_line = last_line.min(new_total_vis).max(first_line + 1);
            let (g, ws, we, fv_new) = self.layout_virtual_window(
                ui,
                &font_id,
                text_color,
                first_line,
                last_line,
                &mut window_buf,
            );
            galley = g;
            window_start_char = ws;
            window_end_char = we;
            fold_view = fv_new;
            layout_dirty = true;
        }

        state.cursor.set_char_range(Some(ccursor_range));

        if should_feed_undoer {
            let snap = self.text.undo_snapshot();
            state
                .undoer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .feed_state(
                ui.input(|i| i.time),
                &(ccursor_range, snap.clone()),
            );
            state.last_undo_snapshot = Some(snap);
        }

        let (cur_line, cur_col) = self.text.char_to_line_col(ccursor_range.primary.index.0);
        let cur_vis_line = match self.fold_state.as_deref_mut() {
            Some(fs) if fs.has_active_folds() => {
                let vis = fs.visible_lines(total_lines_after);
                vis.partition_point(|&l| l < cur_line)
                    .min(vis.len().saturating_sub(1))
            }
            _ => cur_line,
        };
        let cursor_in_window = ccursor_range.primary.index.0 >= window_start_char
            && ccursor_range.primary.index.0 <= window_end_char;

        let local_primary = if cursor_in_window {
            let disp_idx = match fold_view.as_deref() {
                Some(fv) => fv.doc_to_disp(ccursor_range.primary.index.0),
                None => ccursor_range.primary.index.0 - window_start_char,
            };
            Some(galley.layout_from_cursor(CCursor::new(disp_idx)))
        } else {
            None
        };

        let mut cursor_box = if let Some(lp) = local_primary {
            galley
                .pos_from_layout_cursor(&lp)
                .translate(window_galley_pos.to_vec2())
        } else {
            let x = base_pos.x + cur_col as f32 * char_width;
            let y = base_pos.y + cur_vis_line as f32 * line_height;
            Rect::from_min_size(Pos2::new(x, y), Vec2::new(1.0, line_height))
        };
        if cursor_box.height() < line_height {
            cursor_box.max.y = cursor_box.min.y + line_height;
        }

        let cursor_moved = ccursor_range != prev_ccursor_range;
        if has_focus && (text_changed || cursor_moved) {
            state.scroll_to_cursor = true;
        }
        if state.scroll_to_cursor {
            ui.scroll_to_rect(cursor_box, None);
        }

        let text_clip_rect = ui.clip_rect().intersect(rect);
        let painter = ui.painter_at(rect);

        // Paint selection rects for visible rows inside the window
        if ccursor_range.primary != ccursor_range.secondary {
            paint_selection_rows(
                &painter,
                &galley,
                window_galley_pos,
                text_clip_rect,
                ui.visuals().selection.bg_fill,
                ccursor_range,
                window_start_char..window_end_char,
                fold_view.as_deref(),
            );
        }

        painter.galley(window_galley_pos, Arc::clone(&galley), Color32::WHITE);

        if let Some(fv) = fold_view.as_deref() {
            paint_fold_placeholders(
                ui,
                &painter,
                &galley,
                window_galley_pos,
                text_clip_rect,
                fv,
                &font_id,
                badge_width,
            );
        }

        let cursor_in_view =
            finish_cursor_and_scroll(ui, &mut state, cursor_box, text_clip_rect, has_focus);

        let local_row = cur_vis_line.saturating_sub(first_line);
        let local_col = local_primary.map(|lp| lp.column.0).unwrap_or(cur_col);
        update_accesskit(
            ui,
            self.id,
            &mut state,
            &galley,
            window_galley_pos,
            rect,
            text_clip_rect,
            has_focus,
            cursor_in_view && cursor_in_window,
            local_row,
            cur_line,
            local_col,
        );

        let final_fold_version = self
            .fold_state
            .as_deref()
            .map_or(0, FoldState::fold_version);
        if text_changed {
            if let (Some(fs), Some(r)) = (self.fold_state.as_deref_mut(), self.text_revision) {
                fs.set_cached_revision(r.wrapping_add(1));
            }
        }
        if layout_dirty {
            let cached_rev = if text_changed {
                rev.wrapping_add(1)
            } else {
                rev
            };
            state.virtual_layout_cache = Some(Arc::new(VirtualLayoutCache {
                key: cache_key,
                text_revision: cached_rev,
                fold_version: final_fold_version,
                first_line,
                last_line,
                window_start_char,
                window_end_char,
                galley: Arc::clone(&galley),
                fold_view,
            }));
        }
        store_editor_state(ui, self.id, state, current_pass, has_focus);

        EditorOutput {
            response,
            galley,
            galley_pos: window_galley_pos,
            text_clip_rect,
            cursor_range: Some(ccursor_range),
            text_changed,
            first_line,
            virtualized: true,
            active_galley_row: if cursor_in_window {
                Some(local_row)
            } else {
                None
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_accesskit(
    ui: &mut Ui,
    id: Id,
    state: &mut FastEditorState,
    galley: &Arc<Galley>,
    galley_pos: Pos2,
    rect: Rect,
    text_clip_rect: Rect,
    has_focus: bool,
    cursor_in_view: bool,
    galley_row_idx: usize,
    global_row_idx: usize,
    col_idx: usize,
) {
    ui.ctx().accesskit_node_builder(id, |builder| {
        builder.set_role(egui::accesskit::Role::MultilineTextInput);
        builder.set_bounds(egui::accesskit::Rect {
            x0: rect.min.x.into(),
            y0: rect.min.y.into(),
            x1: rect.max.x.into(),
            y1: rect.max.y.into(),
        });
    });

    if has_focus && !cursor_in_view {
        state.accesskit_row_cache = None;
        ui.ctx().accesskit_node_builder(id, |builder| {
            builder.clear_text_selection();
        });
    } else if let Some(row) = galley.rows.get(galley_row_idx) {
        let row_id = id.with(global_row_idx);
        let row_accesskit_id: egui::accesskit::NodeId = row_id.value().into();

        if has_focus {
            if !state.had_focus_prev_frame && !ui.ctx().will_discard() {
                state.pending_accesskit_selection_sync = true;
                ui.ctx().request_repaint();
                ui.ctx().accesskit_node_builder(id, |builder| {
                    let pos = egui::accesskit::TextPosition {
                        node: row_accesskit_id,
                        character_index: 0,
                    };
                    builder.set_text_selection(egui::accesskit::TextSelection {
                        anchor: pos,
                        focus: pos,
                    });
                });
            } else {
                if !ui.ctx().will_discard() {
                    state.pending_accesskit_selection_sync = false;
                }
                ui.ctx().accesskit_node_builder(id, |builder| {
                    let target_idx = (col_idx + 1).min(row.glyphs.len() + 1);
                    let focus_pos = egui::accesskit::TextPosition {
                        node: row_accesskit_id,
                        character_index: target_idx,
                    };
                    builder.set_text_selection(egui::accesskit::TextSelection {
                        anchor: focus_pos,
                        focus: focus_pos,
                    });
                });
            }
        } else if ui.memory(|m| m.has_focus(id)) {
            // The widget keeps egui focus but the OS window is inactive. Keep exposing
            // the caret so that when Windows re-announces focus on window activation
            // (before our next frame runs) UIA clients (magnifier, screen readers) find
            // a valid text selection and keep tracking the caret.
            ui.ctx().accesskit_node_builder(id, |builder| {
                let target_idx = (col_idx + 1).min(row.glyphs.len() + 1);
                let focus_pos = egui::accesskit::TextPosition {
                    node: row_accesskit_id,
                    character_index: target_idx,
                };
                builder.set_text_selection(egui::accesskit::TextSelection {
                    anchor: focus_pos,
                    focus: focus_pos,
                });
            });
        } else {
            ui.ctx().accesskit_node_builder(id, |builder| {
                builder.clear_text_selection();
            });
        }

        let line_height = ui.text_style_height(&egui::TextStyle::Monospace);
        let mut row_min = galley_pos + row.rect().min.to_vec2();
        let mut row_max = galley_pos + row.rect().max.to_vec2();
        if row_max.y - row_min.y < line_height {
            row_max.y = row_min.y + line_height;
        }

        if text_clip_rect.min.y.is_finite()
            && text_clip_rect.max.y.is_finite()
            && text_clip_rect.min.y <= text_clip_rect.max.y
        {
            row_min.y = row_min.y.clamp(text_clip_rect.min.y, text_clip_rect.max.y);
            row_max.y = row_max.y.clamp(row_min.y, text_clip_rect.max.y);
        } else if text_clip_rect.min.y.is_finite() {
            row_min.y = text_clip_rect.min.y;
            row_max.y = text_clip_rect.min.y;
        }
        if text_clip_rect.min.x.is_finite()
            && text_clip_rect.max.x.is_finite()
            && text_clip_rect.min.x <= text_clip_rect.max.x
        {
            row_min.x = row_min.x.clamp(text_clip_rect.min.x, text_clip_rect.max.x);
            row_max.x = row_max.x.clamp(row_min.x, text_clip_rect.max.x);
        } else if text_clip_rect.min.x.is_finite() {
            row_min.x = text_clip_rect.min.x;
            row_max.x = text_clip_rect.min.x;
        }
        if row_max.x - row_min.x < 2.0 {
            row_max.x = row_min.x + 2.0;
        }
        let row_bounds = egui::accesskit::Rect {
            x0: row_min.x.into(),
            y0: row_min.y.into(),
            x1: row_max.x.into(),
            y1: row_max.y.into(),
        };

        let row_unchanged = match &state.accesskit_row_cache {
            Some(c) => {
                Arc::ptr_eq(&c.galley, galley)
                    && c.row == global_row_idx
                    && c.bounds == row_bounds
            }
            None => false,
        };

        if row_unchanged {
            ui.ctx().accesskit_node_builder(id, |builder| {
                if !builder.children().contains(&row_accesskit_id) {
                    builder.push_child(row_accesskit_id);
                }
            });
        } else if !ui.ctx().will_discard() {
            let mut built_row = false;
            let sub_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("accesskit_row", row_id))
                    .accessibility_parent(id),
            );
            sub_ui.interact(egui::Rect::NOTHING, row_id, egui::Sense::empty());
            sub_ui.ctx().accesskit_node_builder(row_id, |builder| {
                built_row = true;
                builder.set_role(egui::accesskit::Role::TextRun);
                builder.set_text_direction(egui::accesskit::TextDirection::LeftToRight);
                builder.set_bounds(row_bounds);

                let glyph_count = row.glyphs.len();
                let mut value = String::with_capacity(glyph_count + 2);
                let mut character_lengths = Vec::<u8>::with_capacity(glyph_count + 2);
                let mut character_positions = Vec::<f32>::with_capacity(glyph_count + 2);
                let mut character_widths = Vec::<f32>::with_capacity(glyph_count + 2);

                value.push(' ');
                character_lengths.push(1);
                character_positions.push(0.0);
                character_widths.push(1.0);

                for glyph in &row.glyphs {
                    let old_len = value.len();
                    value.push(glyph.chr);
                    character_lengths.push((value.len() - old_len) as u8);
                    character_positions.push(glyph.pos.x - row.rect().min.x);
                    character_widths.push(glyph.advance_width.max(1.0));
                }

                let end_x = row.rect().width();
                value.push(if row.ends_with_newline { '\n' } else { ' ' });
                character_lengths.push(1);
                character_positions.push(end_x);
                character_widths.push(1.0);

                builder.set_value(value);
                builder.set_character_lengths(character_lengths);
                builder.set_character_positions(character_positions);
                builder.set_character_widths(character_widths);
                builder.set_word_starts(vec![0]);
            });
            if built_row {
                state.accesskit_row_cache = Some(AccessKitRowCache {
                    galley: Arc::clone(galley),
                    row: global_row_idx,
                    bounds: row_bounds,
                });
            }
        }
    }
}
