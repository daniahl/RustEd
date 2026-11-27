//! Text buffer abstraction (`EditorBuffer`) and undo snapshot types.

use crate::piece_table::{PieceTable, PieceTableSnapshot};
use eframe::egui::{
    self,
    text::CCursor,
    text_selection::CCursorRange,
};
use std::{ops::Range, sync::Arc};

/// Initial window size (in chars) for word-boundary scanning.
pub const WORD_SCAN_WINDOW: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoSnapshot {
    PieceTable(PieceTableSnapshot),
    FullText(Arc<str>),
}

/// Abstraction over editable text buffers (`String`, `PieceTable`).
///
/// Unlike `egui::TextBuffer`, this trait uses a closure-based `with_str` API
/// so that buffers with interior mutability (e.g. `PieceTable`'s lazy cache)
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

    /// Restores the buffer from `snapshot`. Returns `true` if restoration succeeded,
    /// or `false` if the snapshot is incompatible with this buffer.
    fn restore_undo_snapshot(&mut self, snapshot: &UndoSnapshot) -> bool {
        if let UndoSnapshot::FullText(text) = snapshot {
            self.replace_with(text);
            true
        } else {
            false
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
            for c in s
                .chars()
                .skip(char_range.start)
                .take(char_range.end.saturating_sub(char_range.start))
            {
                out.push(c);
            }
        });
    }

    fn slice_chars(&self, char_range: Range<usize>) -> String {
        let mut s = String::new();
        self.extract_char_range(char_range, &mut s);
        s
    }

    /// Returns `true` if `char_idx` falls strictly between `'\r'` and `'\n'` of a CRLF pair.
    fn is_mid_crlf(&self, char_idx: usize) -> bool {
        if char_idx == 0 || char_idx >= self.char_count() {
            return false;
        }
        self.slice_chars((char_idx - 1)..(char_idx + 1)) == "\r\n"
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

    fn restore_undo_snapshot(&mut self, snapshot: &UndoSnapshot) -> bool {
        match snapshot {
            UndoSnapshot::PieceTable(snap) => self.restore_snapshot(snap),
            UndoSnapshot::FullText(text) => {
                self.restore_full_text(text);
                true
            }
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
