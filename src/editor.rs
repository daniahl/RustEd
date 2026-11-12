//! High-performance text editor widget (`FastEditor`).
//!
//! Custom multiline text editing component optimized for large files, with syntax layout caching,
//! undo/redo history, scroll management, and screen-magnifier/screen-reader accessibility (AccessKit).

use crate::piece_table::{PieceTable, PieceTableSnapshot};
use eframe::egui::{
    self,
    epaint::text::{
        cursor::{Cursor, PCursor, RCursor},
        Galley,
    },
    text::{CCursor, LayoutJob},
    text_selection::{
        visuals::paint_text_cursor,
        CCursorRange, CursorRange, TextCursorState,
    },
    Color32, Event, Id, Key, Pos2, Rect, Response, Sense, Ui, Vec2,
};
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoSnapshot {
    PieceTable(PieceTableSnapshot),
    FullText(Arc<str>),
}

pub trait EditorBuffer: egui::TextBuffer {
    fn undo_snapshot(&self) -> UndoSnapshot {
        UndoSnapshot::FullText(Arc::from(self.as_str()))
    }

    fn restore_undo_snapshot(&mut self, snapshot: &UndoSnapshot) {
        if let UndoSnapshot::FullText(text) = snapshot {
            self.replace_with(text);
        }
    }

    fn char_count(&self) -> usize {
        self.as_str().chars().count()
    }

    fn line_count(&self) -> usize {
        self.as_str().bytes().filter(|&b| b == b'\n').count() + 1
    }

    fn line_to_char(&self, line_idx: usize) -> usize {
        if line_idx == 0 {
            return 0;
        }
        let s = self.as_str();
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
    }

    fn char_to_line_col(&self, char_idx: usize) -> (usize, usize) {
        let s = self.as_str();
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
    }

    fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        let start = self.line_to_char(line_idx);
        let total_lines = self.line_count();
        let end = if line_idx + 1 >= total_lines {
            self.char_count()
        } else {
            self.line_to_char(line_idx + 1).saturating_sub(1)
        };
        start..end.max(start)
    }

    fn extract_char_range(&self, char_range: Range<usize>, out: &mut String) {
        out.clear();
        let s = self.as_str();
        for c in s.chars().skip(char_range.start).take(char_range.end.saturating_sub(char_range.start)) {
            out.push(c);
        }
    }

    fn slice_chars(&self, char_range: Range<usize>) -> String {
        let mut s = String::new();
        self.extract_char_range(char_range, &mut s);
        s
    }
}

impl EditorBuffer for String {}

impl EditorBuffer for PieceTable {
    fn undo_snapshot(&self) -> UndoSnapshot {
        UndoSnapshot::PieceTable(self.snapshot())
    }

    fn restore_undo_snapshot(&mut self, snapshot: &UndoSnapshot) {
        match snapshot {
            UndoSnapshot::PieceTable(snap) => self.restore_snapshot(snap),
            UndoSnapshot::FullText(text) => egui::TextBuffer::replace_with(self, text),
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
}

type Undoer = egui::util::undoer::Undoer<(CCursorRange, UndoSnapshot)>;

#[derive(Clone, Default)]
pub struct FastEditorState {
    pub cursor: TextCursorState,
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
    galley: Arc<Galley>,
}

/// Cached windowed galley for virtualized large-file mode.
pub struct VirtualLayoutCache {
    key: (u32, bool, u64),
    text_revision: u64,
    first_line: usize,
    last_line: usize,
    window_start_char: usize,
    window_end_char: usize,
    galley: Arc<Galley>,
}

#[allow(dead_code)]
pub struct EditorOutput {
    pub response: Response,
    pub galley: Arc<Galley>,
    pub galley_pos: Pos2,
    pub text_clip_rect: Rect,
    pub cursor_range: Option<CursorRange>,
    pub text_changed: bool,
    pub first_line: usize,
    pub virtualized: bool,
}

enum Layouter<'a, T> {
    Str(&'a mut dyn FnMut(&Ui, &str, f32) -> Arc<Galley>),
    Buffer(&'a mut dyn FnMut(&Ui, &T, f32) -> Arc<Galley>),
}

impl<'a, T: egui::TextBuffer> Layouter<'a, T> {
    fn layout(&mut self, ui: &Ui, buffer: &T, wrap_width: f32) -> Arc<Galley> {
        match self {
            Layouter::Str(f) => f(ui, buffer.as_str(), wrap_width),
            Layouter::Buffer(f) => f(ui, buffer, wrap_width),
        }
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
        }
    }

    /// Enables line-windowed virtualized layout (no word-wrap, only visible lines laid out).
    pub fn virtualized(mut self, enabled: bool) -> Self {
        self.virtualized = enabled;
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
    #[allow(dead_code)]
    pub fn text_revision(mut self, revision: u64) -> Self {
        self.text_revision = Some(revision);
        self
    }

    pub fn show(mut self, ui: &mut Ui) -> EditorOutput {
        if self.virtualized {
            return self.show_virtualized(ui);
        }
        const MARGIN: Vec2 = Vec2::new(6.0, 2.0);
        let wrap_width = (ui.available_width() - MARGIN.x * 2.0).max(100.0);

        let mut state = ui
            .ctx()
            .data_mut(|d| d.get_temp::<FastEditorState>(self.id))
            .unwrap_or_default();

        let current_pass = ui.ctx().cumulative_pass_nr();
        let was_rendered_last_pass =
            state.last_rendered_pass == Some(current_pass.saturating_sub(1));
        if !was_rendered_last_pass {
            state.accesskit_row_cache = None;
            state.had_focus_prev_frame = false;
        }

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
                _ => c.galley.text() == self.text.as_str(),
            },
            None => false,
        };
        if !text_unchanged {
            state.last_undo_snapshot = None;
        }
        let mut layout_dirty = false;
        let mut galley = match &state.layout_cache {
            Some(c) if c.key == cache_key && text_unchanged => Arc::clone(&c.galley),
            _ => {
                layout_dirty = true;
                self.layouter.layout(ui, self.text, wrap_width)
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
            .range(&galley)
            .unwrap_or_else(|| CursorRange::one(galley.begin()));

        // Pointer interaction (mouse click & drag)
        if let Some(pointer_pos) = ui.ctx().pointer_interact_pos() {
            if response.hovered() || response.dragged() || response.clicked() {
                let is_being_dragged = ui.ctx().is_being_dragged(self.id);
                let cursor_at_pointer = galley.cursor_from_pos(pointer_pos - galley_pos);
                let did_interact = state.cursor.pointer_interaction(
                    ui,
                    &response,
                    cursor_at_pointer,
                    &galley,
                    is_being_dragged,
                );
                if did_interact || response.clicked() {
                    response.request_focus();
                    state.last_interaction_time = ui.input(|i| i.time);
                    if let Some(r) = state.cursor.range(&galley) {
                        cursor_range = r;
                    }
                }
            }
        }

        let has_focus = response.has_focus();
        let os = ui.ctx().os();
        let mut text_changed = false;

        if has_focus
            || (!ui.memory(|m| m.any_popup_open())
                && ui.input(|i| i.key_pressed(Key::Escape)))
        {
            // Keep editor focus if Escape was pressed while editor had focus on the previous frame
            if ui.memory(|m| m.had_focus_last_frame(self.id) && m.focused().is_none()) {
                response.request_focus();
            }
        }

        let has_focus = response.has_focus();
        if has_focus {
            let now = ui.input(|i| i.time);

            // Feed state to undoer if in flux so undo points can stabilize.
            // Reuse the cached UndoSnapshot when available to avoid snapshotting on idle frames.
            {
                let mut undoer = state.undoer.lock().unwrap();
                if undoer.is_in_flux() {
                    let snap = match &state.last_undo_snapshot {
                        Some(s) => s.clone(),
                        None => {
                            let s = self.text.undo_snapshot();
                            state.last_undo_snapshot = Some(s.clone());
                            s
                        }
                    };
                    undoer.feed_state(now, &(cursor_range.as_ccursor_range(), snap));
                }
            }

            let events = ui.input(|i| {
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
            });
            let mut pending_relayout = false;

            for event in &events {
                let mut mutated: Option<CCursorRange> = None;

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
                    let ccursor_range = cursor_range.as_ccursor_range();
                    galley = self.layouter.layout(ui, self.text, wrap_width);
                    layout_dirty = true;
                    pending_relayout = false;
                    cursor_range = CursorRange {
                        primary: galley.from_ccursor(ccursor_range.primary),
                        secondary: galley.from_ccursor(ccursor_range.secondary),
                    };
                }

                // Navigation & selection events (arrows, home, end, ctrl+a, etc.)
                if cursor_range.on_event(os, event, &galley, self.id) {
                    state.last_interaction_time = now;
                    continue;
                }

                match event {
                    Event::Key {
                        key: Key::PageUp,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let rows_to_jump = 25;
                        for _ in 0..rows_to_jump {
                            cursor_range.primary = galley.cursor_up_one_row(&cursor_range.primary);
                        }
                        if !modifiers.shift {
                            cursor_range.secondary = cursor_range.primary;
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
                        let rows_to_jump = 25;
                        for _ in 0..rows_to_jump {
                            cursor_range.primary =
                                galley.cursor_down_one_row(&cursor_range.primary);
                        }
                        if !modifiers.shift {
                            cursor_range.secondary = cursor_range.primary;
                        }
                        state.last_interaction_time = now;
                        continue;
                    }
                    Event::Copy => {
                        if !cursor_range.is_empty() {
                            let [min_c, max_c] = cursor_range.as_ccursor_range().sorted();
                            ui.ctx().copy_text(self.text.slice_chars(min_c.index..max_c.index));
                        }
                    }
                    Event::Cut => {
                        if !cursor_range.is_empty() {
                            let [min_c, max_c] = cursor_range.as_ccursor_range().sorted();
                            ui.ctx().copy_text(self.text.slice_chars(min_c.index..max_c.index));
                            let ccursor = self.text.delete_selected(&cursor_range);
                            mutated = Some(CCursorRange::one(ccursor));
                        }
                    }
                    Event::Paste(text_to_insert) => {
                        if !text_to_insert.is_empty() {
                            let mut ccursor = self.text.delete_selected(&cursor_range);
                            self.text.insert_text_at(&mut ccursor, text_to_insert, usize::MAX);
                            mutated = Some(CCursorRange::one(ccursor));
                        }
                    }
                    Event::Text(text_to_insert) => {
                        // Enter/Return is handled by Key::Enter to avoid double newlines
                        if !text_to_insert.is_empty()
                            && text_to_insert != "\n"
                            && text_to_insert != "\r"
                        {
                            let mut ccursor = self.text.delete_selected(&cursor_range);
                            self.text.insert_text_at(&mut ccursor, text_to_insert, usize::MAX);
                            mutated = Some(CCursorRange::one(ccursor));
                        }
                    }
                    Event::Key {
                        key: Key::Enter,
                        pressed: true,
                        ..
                    } => {
                        let mut ccursor = self.text.delete_selected(&cursor_range);
                        self.text.insert_text_at(&mut ccursor, "\n", usize::MAX);
                        mutated = Some(CCursorRange::one(ccursor));
                    }
                    Event::Key {
                        key: Key::Tab,
                        pressed: true,
                        ..
                    } => {
                        let mut ccursor = self.text.delete_selected(&cursor_range);
                        self.text.insert_text_at(&mut ccursor, "    ", usize::MAX);
                        mutated = Some(CCursorRange::one(ccursor));
                    }
                    Event::Key {
                        key: Key::Backspace,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let ccursor = if let Some(cursor) = cursor_range.single() {
                            if modifiers.ctrl {
                                self.text.delete_previous_word(cursor.ccursor)
                            } else {
                                self.text.delete_previous_char(cursor.ccursor)
                            }
                        } else {
                            self.text.delete_selected(&cursor_range)
                        };
                        mutated = Some(CCursorRange::one(ccursor));
                    }
                    Event::Key {
                        key: Key::Delete,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let ccursor = if let Some(cursor) = cursor_range.single() {
                            if modifiers.ctrl {
                                self.text.delete_next_word(cursor.ccursor)
                            } else {
                                self.text.delete_next_char(cursor.ccursor)
                            }
                        } else {
                            self.text.delete_selected(&cursor_range)
                        };
                        mutated = Some(CCursorRange::one(ccursor));
                    }
                    Event::Key {
                        key: Key::Z,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        let current_snap = match &state.last_undo_snapshot {
                            Some(s) if !text_changed => s.clone(),
                            _ => self.text.undo_snapshot(),
                        };
                        let undo_target = state
                            .undoer
                            .lock()
                            .unwrap()
                            .undo(&(cursor_range.as_ccursor_range(), current_snap))
                            .cloned();
                        if let Some((undo_range, undo_snap)) = undo_target {
                            self.text.restore_undo_snapshot(&undo_snap);
                            state.last_undo_snapshot = Some(undo_snap);
                            mutated = Some(undo_range);
                        }
                    }
                    Event::Key {
                        key: Key::Y,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        let current_snap = match &state.last_undo_snapshot {
                            Some(s) if !text_changed => s.clone(),
                            _ => self.text.undo_snapshot(),
                        };
                        let redo_target = state
                            .undoer
                            .lock()
                            .unwrap()
                            .redo(&(cursor_range.as_ccursor_range(), current_snap))
                            .cloned();
                        if let Some((redo_range, redo_snap)) = redo_target {
                            self.text.restore_undo_snapshot(&redo_snap);
                            state.last_undo_snapshot = Some(redo_snap);
                            mutated = Some(redo_range);
                        }
                    }
                    _ => {}
                }

                if let Some(new_ccursor_range) = mutated {
                    text_changed = true;
                    state.last_interaction_time = now;
                    cursor_range.primary.ccursor = new_ccursor_range.primary;
                    cursor_range.secondary.ccursor = new_ccursor_range.secondary;
                    pending_relayout = true;
                }
            }

            if pending_relayout {
                let ccursor_range = cursor_range.as_ccursor_range();
                galley = self.layouter.layout(ui, self.text, wrap_width);
                layout_dirty = true;
                cursor_range = CursorRange {
                    primary: galley.from_ccursor(ccursor_range.primary),
                    secondary: galley.from_ccursor(ccursor_range.secondary),
                };
            }
        }

        state.cursor.set_range(Some(cursor_range));

        if text_changed {
            let snap = self.text.undo_snapshot();
            state.undoer.lock().unwrap().feed_state(
                ui.input(|i| i.time),
                &(cursor_range.as_ccursor_range(), snap.clone()),
            );
            state.last_undo_snapshot = Some(snap);
        }

        let text_clip_rect = ui.clip_rect().intersect(rect);
        let painter = ui.painter_at(rect);

        // Selection & cursor rendering
        let cursor_box = galley
            .pos_from_cursor(&cursor_range.primary)
            .translate(galley_pos.to_vec2());
        let cursor_moved = state.cursor.char_range() != prev_cursor_range;

        if has_focus && (text_changed || cursor_moved) {
            state.scroll_to_cursor = true;
        }

        if state.scroll_to_cursor {
            ui.scroll_to_rect(cursor_box, None);
        }

        if !cursor_range.is_empty() {
            // Paint selection rects directly for visible rows instead of calling
            // `paint_text_selection` on `galley`, which would deep-clone the entire
            // Galley via `Arc::make_mut` every frame.
            let [min_cursor, max_cursor] = cursor_range.sorted_cursors();
            let min_row = min_cursor.rcursor.row;
            let max_row = max_cursor.rcursor.row.min(galley.rows.len().saturating_sub(1));
            let top = text_clip_rect.min.y - galley_pos.y;
            let first_vis = galley.rows.partition_point(|r| r.rect.max.y < top);
            let start_row = min_row.max(first_vis);
            let bg_fill = ui.visuals().selection.bg_fill;

            if start_row <= max_row {
                for (row_idx, row) in galley.rows[start_row..=max_row]
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (start_row + i, r))
                {
                    let row_top_y = galley_pos.y + row.rect.min.y;
                    if row_top_y > text_clip_rect.max.y {
                        break;
                    }
                    let left = if row_idx == min_row {
                        row.x_offset(min_cursor.rcursor.column)
                    } else {
                        0.0
                    };
                    let right = if row_idx == max_cursor.rcursor.row {
                        row.x_offset(max_cursor.rcursor.column)
                    } else {
                        let newline_extra = if row.ends_with_newline {
                            row.rect.height() * 0.5
                        } else {
                            0.0
                        };
                        row.rect.width() + newline_extra
                    };
                    let sel_rect = Rect::from_min_max(
                        galley_pos + Vec2::new(left, row.rect.min.y),
                        galley_pos + Vec2::new(right, row.rect.max.y),
                    );
                    painter.rect_filled(sel_rect, 0.0, bg_fill);
                }
            }
        }

        painter.galley(galley_pos, Arc::clone(&galley), Color32::WHITE);

        if has_focus {
            let now = ui.input(|i| i.time);
            let mut primary_cursor_rect = cursor_box;
            // An empty galley has a zero-height row, which would render only a sliver of the caret.
            let line_height = ui.text_style_height(&egui::TextStyle::Monospace);
            if primary_cursor_rect.height() < line_height {
                primary_cursor_rect.max.y = primary_cursor_rect.min.y + line_height;
            }
            // Use the full visible clip rect so the caret is never cut off at the edges.
            let cursor_painter = ui.painter().with_clip_rect(ui.clip_rect());
            paint_text_cursor(
                ui,
                &cursor_painter,
                primary_cursor_rect,
                now - state.last_interaction_time,
            );
        }

        let cursor_in_view = text_clip_rect.contains(cursor_box.center());
        if cursor_in_view {
            state.scroll_to_cursor = false;
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

        let focus = cursor_range.primary.rcursor;
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
            focus.column,
        );

        if !ui.ctx().will_discard() {
            state.had_focus_prev_frame = has_focus;
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
                galley: Arc::clone(&galley),
            }));
        }
        state.last_rendered_pass = Some(current_pass);
        ui.ctx().data_mut(|d| d.insert_temp(self.id, state));

        let focus_filter = egui::EventFilter {
            horizontal_arrows: true,
            vertical_arrows: true,
            tab: true,
            escape: true,
        };
        if has_focus {
            ui.memory_mut(|mem| {
                mem.set_focus_lock_filter(self.id, focus_filter);
            });
        }

        EditorOutput {
            response,
            galley,
            galley_pos,
            text_clip_rect,
            cursor_range: Some(cursor_range),
            text_changed,
            first_line: 0,
            virtualized: false,
        }
    }

    fn show_virtualized(self, ui: &mut Ui) -> EditorOutput {
        const MARGIN: Vec2 = Vec2::new(6.0, 2.0);
        const OVERSCAN: usize = 4;

        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let text_color = ui.visuals().widgets.inactive.text_color();
        let line_height = ui.fonts(|f| f.row_height(&font_id)).max(1.0);
        let char_width = ui.fonts(|f| f.glyph_width(&font_id, ' ')).max(1.0);

        let mut state = ui
            .ctx()
            .data_mut(|d| d.get_temp::<FastEditorState>(self.id))
            .unwrap_or_default();

        let current_pass = ui.ctx().cumulative_pass_nr();
        let was_rendered_last_pass =
            state.last_rendered_pass == Some(current_pass.saturating_sub(1));
        if !was_rendered_last_pass {
            state.accesskit_row_cache = None;
            state.had_focus_prev_frame = false;
        }

        let total_lines = self.text.line_count().max(1);
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
        let first_line = ((vis_top / line_height).floor() as usize)
            .saturating_sub(OVERSCAN)
            .min(total_lines - 1);
        let last_line = (((vis_bottom / line_height).ceil() as usize) + 1 + OVERSCAN)
            .min(total_lines)
            .max(first_line + 1);
        let window_galley_pos = base_pos + Vec2::new(0.0, first_line as f32 * line_height);

        let cache_key = (
            ui.ctx().pixels_per_point().to_bits(),
            ui.visuals().dark_mode,
            self.cache_salt,
        );
        let rev = self.text_revision.unwrap_or(0);
        let mut window_buf = String::new();

        let (mut galley, mut window_start_char, mut window_end_char, mut layout_dirty) =
            match &state.virtual_layout_cache {
                Some(c)
                    if c.key == cache_key
                        && self.text_revision.is_some()
                        && c.text_revision == rev
                        && c.first_line == first_line
                        && c.last_line == last_line =>
                {
                    (
                        Arc::clone(&c.galley),
                        c.window_start_char,
                        c.window_end_char,
                        false,
                    )
                }
                _ => {
                    let ws = self.text.line_to_char(first_line);
                    let we = if last_line >= total_lines {
                        total_chars
                    } else {
                        self.text.line_to_char(last_line).saturating_sub(1)
                    };
                    self.text.extract_char_range(ws..we, &mut window_buf);
                    let job = LayoutJob::simple(
                        std::mem::take(&mut window_buf),
                        font_id.clone(),
                        text_color,
                        f32::INFINITY,
                    );
                    let g = ui.fonts(|f| f.layout_job(job));
                    (g, ws, we, true)
                }
            };

        if layout_dirty {
            state.last_undo_snapshot = None;
        }

        let prev_ccursor_range = state
            .cursor
            .char_range()
            .unwrap_or_else(|| CCursorRange::one(CCursor::new(0)));
        let mut ccursor_range = CCursorRange::two(
            CCursor::new(prev_ccursor_range.secondary.index.min(total_chars)),
            CCursor::new(prev_ccursor_range.primary.index.min(total_chars)),
        );

        // Pointer interaction inside the visible window
        if let Some(pointer_pos) = ui.ctx().pointer_interact_pos() {
            if response.hovered() || response.dragged() || response.clicked() {
                let is_being_dragged = ui.ctx().is_being_dragged(self.id);
                let cursor_at_pointer = galley.cursor_from_pos(pointer_pos - window_galley_pos);

                let mut local_cursor_state = state.cursor.clone();
                let clamped_local = CCursorRange::two(
                    CCursor::new(
                        ccursor_range
                            .secondary
                            .index
                            .saturating_sub(window_start_char)
                            .min(window_end_char - window_start_char),
                    ),
                    CCursor::new(
                        ccursor_range
                            .primary
                            .index
                            .saturating_sub(window_start_char)
                            .min(window_end_char - window_start_char),
                    ),
                );
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
                        ccursor_range = CCursorRange::two(
                            CCursor::new((window_start_char + r.secondary.index).min(total_chars)),
                            CCursor::new((window_start_char + r.primary.index).min(total_chars)),
                        );
                    }
                    state.cursor = local_cursor_state;
                }
            }
        }

        let has_focus = response.has_focus();
        let mut text_changed = false;

        if has_focus
            || (!ui.memory(|m| m.any_popup_open())
                && ui.input(|i| i.key_pressed(Key::Escape)))
        {
            if ui.memory(|m| m.had_focus_last_frame(self.id) && m.focused().is_none()) {
                response.request_focus();
            }
        }

        let has_focus = response.has_focus();
        if has_focus {
            let now = ui.input(|i| i.time);
            {
                let mut undoer = state.undoer.lock().unwrap();
                if undoer.is_in_flux() {
                    let snap = match &state.last_undo_snapshot {
                        Some(s) => s.clone(),
                        None => {
                            let s = self.text.undo_snapshot();
                            state.last_undo_snapshot = Some(s.clone());
                            s
                        }
                    };
                    undoer.feed_state(now, &(ccursor_range, snap));
                }
            }

            let events = ui.input(|i| {
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
            });

            for event in &events {
                let cur_total = self.text.char_count();
                let mut mutated: Option<CCursorRange> = None;

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
                        let next = ccursor_range.primary.index.saturating_sub(1);
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
                        let next = (ccursor_range.primary.index + 1).min(cur_total);
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
                        let (line, col) = self.text.char_to_line_col(ccursor_range.primary.index);
                        let lines_total = self.text.line_count().max(1);
                        let target_line = match key {
                            Key::ArrowUp => line.saturating_sub(1),
                            Key::ArrowDown => (line + 1).min(lines_total - 1),
                            Key::PageUp => line.saturating_sub(25),
                            Key::PageDown => (line + 25).min(lines_total - 1),
                            _ => line,
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
                        let next = if modifiers.ctrl {
                            0
                        } else {
                            let (line, _) = self.text.char_to_line_col(ccursor_range.primary.index);
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
                        let next = if modifiers.ctrl {
                            cur_total
                        } else {
                            let (line, _) = self.text.char_to_line_col(ccursor_range.primary.index);
                            self.text.line_char_range(line).end
                        };
                        ccursor_range.primary = CCursor::new(next);
                        if !modifiers.shift {
                            ccursor_range.secondary = ccursor_range.primary;
                        }
                        state.last_interaction_time = now;
                    }
                    Event::Copy => {
                        if ccursor_range.primary != ccursor_range.secondary {
                            let [min_c, max_c] = ccursor_range.sorted();
                            ui.ctx().copy_text(self.text.slice_chars(min_c.index..max_c.index));
                        }
                    }
                    Event::Cut => {
                        if ccursor_range.primary != ccursor_range.secondary {
                            let [min_c, max_c] = ccursor_range.sorted();
                            ui.ctx().copy_text(self.text.slice_chars(min_c.index..max_c.index));
                            self.text.delete_char_range(min_c.index..max_c.index);
                            mutated = Some(CCursorRange::one(min_c));
                        }
                    }
                    Event::Paste(text_to_insert) => {
                        if !text_to_insert.is_empty() {
                            let [min_c, max_c] = ccursor_range.sorted();
                            if min_c.index < max_c.index {
                                self.text.delete_char_range(min_c.index..max_c.index);
                            }
                            let mut c = min_c;
                            self.text.insert_text_at(&mut c, text_to_insert, usize::MAX);
                            mutated = Some(CCursorRange::one(c));
                        }
                    }
                    Event::Text(text_to_insert) => {
                        if !text_to_insert.is_empty()
                            && text_to_insert != "\n"
                            && text_to_insert != "\r"
                        {
                            let [min_c, max_c] = ccursor_range.sorted();
                            if min_c.index < max_c.index {
                                self.text.delete_char_range(min_c.index..max_c.index);
                            }
                            let mut c = min_c;
                            self.text.insert_text_at(&mut c, text_to_insert, usize::MAX);
                            mutated = Some(CCursorRange::one(c));
                        }
                    }
                    Event::Key {
                        key: Key::Enter,
                        pressed: true,
                        ..
                    } => {
                        let [min_c, max_c] = ccursor_range.sorted();
                        if min_c.index < max_c.index {
                            self.text.delete_char_range(min_c.index..max_c.index);
                        }
                        let mut c = min_c;
                        self.text.insert_text_at(&mut c, "\n", usize::MAX);
                        mutated = Some(CCursorRange::one(c));
                    }
                    Event::Key {
                        key: Key::Tab,
                        pressed: true,
                        ..
                    } => {
                        let [min_c, max_c] = ccursor_range.sorted();
                        if min_c.index < max_c.index {
                            self.text.delete_char_range(min_c.index..max_c.index);
                        }
                        let mut c = min_c;
                        self.text.insert_text_at(&mut c, "    ", usize::MAX);
                        mutated = Some(CCursorRange::one(c));
                    }
                    Event::Key {
                        key: Key::Backspace,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let [min_c, max_c] = ccursor_range.sorted();
                        let c = if min_c.index < max_c.index {
                            self.text.delete_char_range(min_c.index..max_c.index);
                            min_c
                        } else if modifiers.ctrl {
                            self.text.delete_previous_word(min_c)
                        } else {
                            self.text.delete_previous_char(min_c)
                        };
                        mutated = Some(CCursorRange::one(c));
                    }
                    Event::Key {
                        key: Key::Delete,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let [min_c, max_c] = ccursor_range.sorted();
                        let c = if min_c.index < max_c.index {
                            self.text.delete_char_range(min_c.index..max_c.index);
                            min_c
                        } else if modifiers.ctrl {
                            self.text.delete_next_word(min_c)
                        } else {
                            self.text.delete_next_char(min_c)
                        };
                        mutated = Some(CCursorRange::one(c));
                    }
                    Event::Key {
                        key: Key::Z,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        let current_snap = match &state.last_undo_snapshot {
                            Some(s) if !text_changed => s.clone(),
                            _ => self.text.undo_snapshot(),
                        };
                        let undo_target = state
                            .undoer
                            .lock()
                            .unwrap()
                            .undo(&(ccursor_range, current_snap))
                            .cloned();
                        if let Some((undo_range, undo_snap)) = undo_target {
                            self.text.restore_undo_snapshot(&undo_snap);
                            state.last_undo_snapshot = Some(undo_snap);
                            mutated = Some(undo_range);
                        }
                    }
                    Event::Key {
                        key: Key::Y,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        let current_snap = match &state.last_undo_snapshot {
                            Some(s) if !text_changed => s.clone(),
                            _ => self.text.undo_snapshot(),
                        };
                        let redo_target = state
                            .undoer
                            .lock()
                            .unwrap()
                            .redo(&(ccursor_range, current_snap))
                            .cloned();
                        if let Some((redo_range, redo_snap)) = redo_target {
                            self.text.restore_undo_snapshot(&redo_snap);
                            state.last_undo_snapshot = Some(redo_snap);
                            mutated = Some(redo_range);
                        }
                    }
                    _ => {}
                }

                if let Some(new_ccursor_range) = mutated {
                    text_changed = true;
                    state.last_interaction_time = now;
                    ccursor_range = new_ccursor_range;
                }
            }
        }

        let total_lines_after = self.text.line_count().max(1);
        let total_chars_after = self.text.char_count();
        ccursor_range.primary.index = ccursor_range.primary.index.min(total_chars_after);
        ccursor_range.secondary.index = ccursor_range.secondary.index.min(total_chars_after);

        if text_changed {
            let ws = self.text.line_to_char(first_line);
            let we = if last_line >= total_lines_after {
                total_chars_after
            } else {
                self.text
                    .line_to_char(last_line.min(total_lines_after))
                    .saturating_sub(1)
            };
            self.text.extract_char_range(ws..we, &mut window_buf);
            let job = LayoutJob::simple(
                std::mem::take(&mut window_buf),
                font_id.clone(),
                text_color,
                f32::INFINITY,
            );
            galley = ui.fonts(|f| f.layout_job(job));
            window_start_char = ws;
            window_end_char = we;
            layout_dirty = true;
        }

        state.cursor.set_char_range(Some(ccursor_range));

        if text_changed {
            let snap = self.text.undo_snapshot();
            state.undoer.lock().unwrap().feed_state(
                ui.input(|i| i.time),
                &(ccursor_range, snap.clone()),
            );
            state.last_undo_snapshot = Some(snap);
        }

        let (cur_line, cur_col) = self.text.char_to_line_col(ccursor_range.primary.index);
        let cursor_in_window = ccursor_range.primary.index >= window_start_char
            && ccursor_range.primary.index <= window_end_char;

        let local_primary = if cursor_in_window {
            Some(galley.from_ccursor(CCursor::new(ccursor_range.primary.index - window_start_char)))
        } else {
            None
        };

        let cursor_box = if let Some(lp) = local_primary {
            galley
                .pos_from_cursor(&lp)
                .translate(window_galley_pos.to_vec2())
        } else {
            let x = base_pos.x + cur_col as f32 * char_width;
            let y = base_pos.y + cur_line as f32 * line_height;
            Rect::from_min_size(Pos2::new(x, y), Vec2::new(1.0, line_height))
        };

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
            let [min_c, max_c] = ccursor_range.sorted();
            if max_c.index > window_start_char && min_c.index < window_end_char {
                let clamped_min = min_c.index.max(window_start_char) - window_start_char;
                let clamped_max = max_c.index.min(window_end_char) - window_start_char;
                let min_cur = galley.from_ccursor(CCursor::new(clamped_min));
                let max_cur = galley.from_ccursor(CCursor::new(clamped_max));
                let min_row = min_cur.rcursor.row;
                let max_row = max_cur.rcursor.row.min(galley.rows.len().saturating_sub(1));
                let bg_fill = ui.visuals().selection.bg_fill;

                for (row_idx, row) in galley.rows[min_row..=max_row]
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (min_row + i, r))
                {
                    let row_top_y = window_galley_pos.y + row.rect.min.y;
                    if row_top_y > text_clip_rect.max.y {
                        break;
                    }
                    if window_galley_pos.y + row.rect.max.y < text_clip_rect.min.y {
                        continue;
                    }
                    let left = if row_idx == min_row && min_c.index >= window_start_char {
                        row.x_offset(min_cur.rcursor.column)
                    } else {
                        0.0
                    };
                    let right = if row_idx == max_row && max_c.index <= window_end_char {
                        row.x_offset(max_cur.rcursor.column)
                    } else {
                        let newline_extra = if row.ends_with_newline {
                            row.rect.height() * 0.5
                        } else {
                            0.0
                        };
                        row.rect.width() + newline_extra
                    };
                    let sel_rect = Rect::from_min_max(
                        window_galley_pos + Vec2::new(left, row.rect.min.y),
                        window_galley_pos + Vec2::new(right, row.rect.max.y),
                    );
                    painter.rect_filled(sel_rect, 0.0, bg_fill);
                }
            }
        }

        painter.galley(window_galley_pos, Arc::clone(&galley), Color32::WHITE);

        if has_focus {
            let now = ui.input(|i| i.time);
            let mut primary_cursor_rect = cursor_box;
            if primary_cursor_rect.height() < line_height {
                primary_cursor_rect.max.y = primary_cursor_rect.min.y + line_height;
            }
            let cursor_painter = ui.painter().with_clip_rect(ui.clip_rect());
            paint_text_cursor(
                ui,
                &cursor_painter,
                primary_cursor_rect,
                now - state.last_interaction_time,
            );
        }

        let cursor_in_view = text_clip_rect.contains(cursor_box.center());
        if cursor_in_view {
            state.scroll_to_cursor = false;
        } else if state.scroll_to_cursor {
            if !ui.ctx().will_discard() {
                ui.ctx().request_discard("scroll to cursor");
            }
            ui.ctx().request_repaint();
        }

        let local_row = cur_line.saturating_sub(first_line);
        let local_col = local_primary.map(|lp| lp.rcursor.column).unwrap_or(cur_col);
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

        if !ui.ctx().will_discard() {
            state.had_focus_prev_frame = has_focus;
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
                first_line,
                last_line,
                window_start_char,
                window_end_char,
                galley: Arc::clone(&galley),
            }));
        }
        state.last_rendered_pass = Some(current_pass);
        ui.ctx().data_mut(|d| d.insert_temp(self.id, state));

        let focus_filter = egui::EventFilter {
            horizontal_arrows: true,
            vertical_arrows: true,
            tab: true,
            escape: true,
        };
        if has_focus {
            ui.memory_mut(|mem| {
                mem.set_focus_lock_filter(self.id, focus_filter);
            });
        }

        let p_cur = Cursor {
            ccursor: ccursor_range.primary,
            rcursor: RCursor {
                row: cur_line,
                column: cur_col,
            },
            pcursor: PCursor {
                paragraph: cur_line,
                offset: cur_col,
                prefer_next_row: false,
            },
        };
        let s_cur = Cursor {
            ccursor: ccursor_range.secondary,
            rcursor: RCursor {
                row: cur_line,
                column: cur_col,
            },
            pcursor: PCursor {
                paragraph: cur_line,
                offset: cur_col,
                prefer_next_row: false,
            },
        };

        EditorOutput {
            response,
            galley,
            galley_pos: window_galley_pos,
            text_clip_rect,
            cursor_range: Some(CursorRange {
                primary: p_cur,
                secondary: s_cur,
            }),
            text_changed,
            first_line,
            virtualized: true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_accesskit(
    ui: &Ui,
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
        } else {
            ui.ctx().accesskit_node_builder(id, |builder| {
                builder.clear_text_selection();
            });
        }

        let mut row_min = galley_pos + row.rect.min.to_vec2();
        let mut row_max = galley_pos + row.rect.max.to_vec2();

        row_min.y = row_min.y.clamp(text_clip_rect.min.y, text_clip_rect.max.y);
        row_max.y = row_max.y.clamp(row_min.y, text_clip_rect.max.y);
        row_min.x = row_min.x.clamp(text_clip_rect.min.x, text_clip_rect.max.x);
        row_max.x = row_max.x.clamp(row_min.x, text_clip_rect.max.x);
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
                builder.push_child(row_accesskit_id);
            });
        } else if !ui.ctx().will_discard() {
            let mut built_row = false;
            ui.ctx().with_accessibility_parent(id, || {
                ui.ctx().accesskit_node_builder(row_id, |builder| {
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
                        character_positions.push(glyph.pos.x - row.rect.min.x);
                        character_widths.push(glyph.advance_width.max(1.0));
                    }

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
