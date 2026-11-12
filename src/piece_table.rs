//! Piece Table text buffer data structure (similar to VS Code's text buffer architecture).
//!
//! Stores document text across two append-only buffers (`original` and `added`)
//! referenced by lightweight piece descriptors. Edits (inserts and deletes) split or
//! extend piece descriptors without ever shifting or reallocating existing text in memory.

use std::{
    cell::{Cell, UnsafeCell},
    fmt,
    ops::{Deref, Range},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferType {
    Original,
    Added,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub buffer: BufferType,
    pub start: usize,         // Byte offset in the respective buffer
    pub length: usize,        // Byte length of this piece
    pub char_count: usize,    // Unicode character (code point) count in this piece
    pub line_feed_cnt: usize, // Number of '\n' in this piece
}

impl Piece {
    #[inline]
    pub fn get_text<'a>(&self, original: &'a str, added: &'a str) -> &'a str {
        match self.buffer {
            BufferType::Original => &original[self.start..self.start + self.length],
            BufferType::Added => &added[self.start..self.start + self.length],
        }
    }

    /// Finds the byte offset within this piece corresponding to `char_offset` (relative to the piece).
    pub fn char_to_byte_offset(&self, original: &str, added: &str, char_offset: usize) -> usize {
        if char_offset == 0 {
            return 0;
        }
        if char_offset >= self.char_count {
            return self.length;
        }
        // Fast path: if char count equals byte length, all characters are single-byte ASCII
        if self.char_count == self.length {
            return char_offset;
        }
        let text = self.get_text(original, added);
        text.char_indices()
            .nth(char_offset)
            .map(|(b, _)| b)
            .unwrap_or(self.length)
    }

    /// Splits this piece into two pieces at `char_offset` (relative to the piece).
    pub fn split_at_char(&self, original: &str, added: &str, char_offset: usize) -> (Piece, Piece) {
        let byte_offset = self.char_to_byte_offset(original, added, char_offset);
        let text = self.get_text(original, added);
        let left_text = &text[..byte_offset];
        let _right_text = &text[byte_offset..];

        let left_lf = left_text.bytes().filter(|&b| b == b'\n').count();
        let right_lf = self.line_feed_cnt - left_lf;

        let left = Piece {
            buffer: self.buffer,
            start: self.start,
            length: byte_offset,
            char_count: char_offset,
            line_feed_cnt: left_lf,
        };
        let right = Piece {
            buffer: self.buffer,
            start: self.start + byte_offset,
            length: self.length - byte_offset,
            char_count: self.char_count - char_offset,
            line_feed_cnt: right_lf,
        };
        (left, right)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceTableSnapshot {
    original: Arc<str>,
    pieces: Arc<[Piece]>,
    total_chars: usize,
    total_bytes: usize,
    total_line_feeds: usize,
}

pub struct PieceTable {
    original: Arc<str>,
    added: String,
    pieces: Vec<Piece>,
    total_chars: usize,
    total_bytes: usize,
    total_line_feeds: usize,
    cached_text: UnsafeCell<String>,
    cached_dirty: Cell<bool>,
}

// Safety: PieceTable does not share interior mutability across threads.
// Send allows transferring ownership between threads.
unsafe impl Send for PieceTable {}

impl Clone for PieceTable {
    fn clone(&self) -> Self {
        Self {
            original: Arc::clone(&self.original),
            added: self.added.clone(),
            pieces: self.pieces.clone(),
            total_chars: self.total_chars,
            total_bytes: self.total_bytes,
            total_line_feeds: self.total_line_feeds,
            cached_text: UnsafeCell::new(String::new()),
            cached_dirty: Cell::new(true),
        }
    }
}

impl Default for PieceTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PieceTable {
    pub fn new() -> Self {
        Self::from_str("")
    }

    pub fn from_string(original: String) -> Self {
        Self::from_arc_str(Arc::<str>::from(original))
    }

    pub fn from_str(text: &str) -> Self {
        Self::from_arc_str(Arc::<str>::from(text))
    }

    fn from_arc_str(original: Arc<str>) -> Self {
        let char_count = original.chars().count();
        let byte_len = original.len();
        let line_feed_cnt = original.bytes().filter(|&b| b == b'\n').count();

        let mut pieces = Vec::new();
        if byte_len > 0 {
            pieces.push(Piece {
                buffer: BufferType::Original,
                start: 0,
                length: byte_len,
                char_count,
                line_feed_cnt,
            });
        }

        Self {
            original,
            added: String::new(),
            pieces,
            total_chars: char_count,
            total_bytes: byte_len,
            total_line_feeds: line_feed_cnt,
            cached_text: UnsafeCell::new(String::new()),
            cached_dirty: Cell::new(true),
        }
    }

    /// Captures a lightweight O(P) snapshot of piece descriptors without copying text buffers.
    pub fn snapshot(&self) -> PieceTableSnapshot {
        PieceTableSnapshot {
            original: Arc::clone(&self.original),
            pieces: Arc::from(self.pieces.as_slice()),
            total_chars: self.total_chars,
            total_bytes: self.total_bytes,
            total_line_feeds: self.total_line_feeds,
        }
    }

    /// Restores piece descriptors from a previously captured `PieceTableSnapshot` in O(P) time.
    pub fn restore_snapshot(&mut self, snapshot: &PieceTableSnapshot) {
        if snapshot
            .pieces
            .iter()
            .all(|p| p.buffer != BufferType::Added || p.start + p.length <= self.added.len())
        {
            self.original = Arc::clone(&snapshot.original);
            self.pieces.clear();
            self.pieces.extend_from_slice(&snapshot.pieces);
            self.total_chars = snapshot.total_chars;
            self.total_bytes = snapshot.total_bytes;
            self.total_line_feeds = snapshot.total_line_feeds;
            self.cached_dirty.set(true);
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total_bytes == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.total_bytes
    }

    #[inline]
    pub fn char_count(&self) -> usize {
        self.total_chars
    }

    #[inline]
    pub fn line_count(&self) -> usize {
        self.total_line_feeds + 1
    }

    #[inline]
    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn clear(&mut self) {
        self.original = "".into();
        self.added.clear();
        self.pieces.clear();
        self.total_chars = 0;
        self.total_bytes = 0;
        self.total_line_feeds = 0;
        unsafe {
            (*self.cached_text.get()).clear();
        }
        self.cached_dirty.set(false);
    }

    /// Inserts `text` at the character index `char_index`.
    /// Returns the number of characters inserted.
    pub fn insert(&mut self, char_index: usize, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }

        let text_bytes = text.len();
        let text_chars = text.chars().count();
        let text_lf = text.bytes().filter(|&b| b == b'\n').count();

        let add_start = self.added.len();
        self.added.push_str(text);

        let target_char = char_index.min(self.total_chars);

        if self.pieces.is_empty() {
            self.pieces.push(Piece {
                buffer: BufferType::Added,
                start: add_start,
                length: text_bytes,
                char_count: text_chars,
                line_feed_cnt: text_lf,
            });
        } else if target_char == self.total_chars {
            // Append at the end: check VS Code consecutive insert optimization
            let last_idx = self.pieces.len() - 1;
            let last = &mut self.pieces[last_idx];
            if last.buffer == BufferType::Added && last.start + last.length == add_start {
                // Extend the existing piece without allocating a new piece descriptor
                last.length += text_bytes;
                last.char_count += text_chars;
                last.line_feed_cnt += text_lf;
            } else {
                self.pieces.push(Piece {
                    buffer: BufferType::Added,
                    start: add_start,
                    length: text_bytes,
                    char_count: text_chars,
                    line_feed_cnt: text_lf,
                });
            }
        } else if target_char == 0 {
            // Prepend at the beginning
            self.pieces.insert(
                0,
                Piece {
                    buffer: BufferType::Added,
                    start: add_start,
                    length: text_bytes,
                    char_count: text_chars,
                    line_feed_cnt: text_lf,
                },
            );
        } else {
            // Find target piece
            let mut curr_char = 0;
            let mut target_idx = 0;
            for (idx, piece) in self.pieces.iter().enumerate() {
                if target_char <= curr_char + piece.char_count {
                    target_idx = idx;
                    break;
                }
                curr_char += piece.char_count;
            }

            let offset_in_piece = target_char - curr_char;
            let piece = self.pieces[target_idx];

            if offset_in_piece == 0 {
                // Boundary between pieces: check consecutive insert optimization on previous piece
                if target_idx > 0 {
                    let prev = &mut self.pieces[target_idx - 1];
                    if prev.buffer == BufferType::Added && prev.start + prev.length == add_start {
                        prev.length += text_bytes;
                        prev.char_count += text_chars;
                        prev.line_feed_cnt += text_lf;
                        self.total_chars += text_chars;
                        self.total_bytes += text_bytes;
                        self.total_line_feeds += text_lf;
                        self.cached_dirty.set(true);
                        return text_chars;
                    }
                }
                self.pieces.insert(
                    target_idx,
                    Piece {
                        buffer: BufferType::Added,
                        start: add_start,
                        length: text_bytes,
                        char_count: text_chars,
                        line_feed_cnt: text_lf,
                    },
                );
            } else if offset_in_piece == piece.char_count {
                // At the end of target piece: check consecutive insert optimization
                if piece.buffer == BufferType::Added && piece.start + piece.length == add_start {
                    let target_piece = &mut self.pieces[target_idx];
                    target_piece.length += text_bytes;
                    target_piece.char_count += text_chars;
                    target_piece.line_feed_cnt += text_lf;
                } else {
                    self.pieces.insert(
                        target_idx + 1,
                        Piece {
                            buffer: BufferType::Added,
                            start: add_start,
                            length: text_bytes,
                            char_count: text_chars,
                            line_feed_cnt: text_lf,
                        },
                    );
                }
            } else {
                // Inside the piece: split into left, new, right
                let (left, right) =
                    piece.split_at_char(&self.original, &self.added, offset_in_piece);
                let new_piece = Piece {
                    buffer: BufferType::Added,
                    start: add_start,
                    length: text_bytes,
                    char_count: text_chars,
                    line_feed_cnt: text_lf,
                };
                self.pieces[target_idx] = left;
                self.pieces.insert(target_idx + 1, new_piece);
                self.pieces.insert(target_idx + 2, right);
            }
        }

        self.total_chars += text_chars;
        self.total_bytes += text_bytes;
        self.total_line_feeds += text_lf;
        self.cached_dirty.set(true);

        text_chars
    }

    /// Deletes characters in the range `char_range`.
    pub fn delete(&mut self, char_range: Range<usize>) {
        let start = char_range.start.min(self.total_chars);
        let end = char_range.end.min(self.total_chars);

        if start >= end || self.pieces.is_empty() {
            return;
        }

        let mut new_pieces = Vec::with_capacity(self.pieces.len());
        let mut curr_char = 0;

        for piece in &self.pieces {
            let piece_start = curr_char;
            let piece_end = curr_char + piece.char_count;
            curr_char = piece_end;

            if piece_end <= start || piece_start >= end {
                // Completely outside deleted range
                new_pieces.push(*piece);
            } else if piece_start >= start && piece_end <= end {
                // Completely inside deleted range -> drop it
            } else {
                // Overlaps at left boundary
                if piece_start < start {
                    let left_len = start - piece_start;
                    let (left, _) =
                        piece.split_at_char(&self.original, &self.added, left_len);
                    new_pieces.push(left);
                }
                // Overlaps at right boundary
                if piece_end > end {
                    let right_skip = end - piece_start;
                    let (_, right) =
                        piece.split_at_char(&self.original, &self.added, right_skip);
                    new_pieces.push(right);
                }
            }
        }

        self.pieces = new_pieces;
        self.coalesce();

        // Recalculate totals from surviving pieces
        self.total_chars = self.pieces.iter().map(|p| p.char_count).sum();
        self.total_bytes = self.pieces.iter().map(|p| p.length).sum();
        self.total_line_feeds = self.pieces.iter().map(|p| p.line_feed_cnt).sum();
        self.cached_dirty.set(true);
    }

    /// Merges contiguous pieces that point to adjacent regions in the same buffer in a single O(M) pass.
    pub fn coalesce(&mut self) {
        if self.pieces.len() <= 1 {
            return;
        }
        let mut write = 0;
        for read in 1..self.pieces.len() {
            let next = self.pieces[read];
            let curr = &mut self.pieces[write];
            if curr.buffer == next.buffer && curr.start + curr.length == next.start {
                curr.length += next.length;
                curr.char_count += next.char_count;
                curr.line_feed_cnt += next.line_feed_cnt;
            } else {
                write += 1;
                self.pieces[write] = next;
            }
        }
        self.pieces.truncate(write + 1);
    }

    /// Returns the flattened string view.
    /// Zero-copy when the document consists of 0 or 1 piece; otherwise rebuilt on demand when dirty.
    pub fn as_str(&self) -> &str {
        match self.pieces.as_slice() {
            [] => "",
            [p] => p.get_text(&self.original, &self.added),
            _ => {
                if self.cached_dirty.get() {
                    let s = unsafe { &mut *self.cached_text.get() };
                    s.clear();
                    s.reserve(self.total_bytes);
                    for p in &self.pieces {
                        s.push_str(p.get_text(&self.original, &self.added));
                    }
                    self.cached_dirty.set(false);
                }
                unsafe { (*self.cached_text.get()).as_str() }
            }
        }
    }

    /// Allocates and builds an owned `String` directly from the piece descriptors in a single pass,
    /// avoiding an intermediate copy into `cached_text`.
    pub fn build_string(&self) -> String {
        let mut s = String::with_capacity(self.total_bytes);
        for p in &self.pieces {
            s.push_str(p.get_text(&self.original, &self.added));
        }
        s
    }

    /// Converts a character index to a byte offset in the document.
    pub fn char_to_byte(&self, char_idx: usize) -> usize {
        if char_idx == 0 {
            return 0;
        }
        if char_idx >= self.total_chars {
            return self.total_bytes;
        }
        let mut curr_char = 0;
        let mut curr_byte = 0;
        for p in &self.pieces {
            if char_idx <= curr_char + p.char_count {
                let offset_in_piece = char_idx - curr_char;
                return curr_byte
                    + p.char_to_byte_offset(&self.original, &self.added, offset_in_piece);
            }
            curr_char += p.char_count;
            curr_byte += p.length;
        }
        self.total_bytes
    }

    /// Converts a byte offset to a character index in the document.
    pub fn byte_to_char(&self, byte_idx: usize) -> usize {
        if byte_idx == 0 {
            return 0;
        }
        if byte_idx >= self.total_bytes {
            return self.total_chars;
        }
        let mut curr_byte = 0;
        let mut curr_char = 0;
        for p in &self.pieces {
            if byte_idx <= curr_byte + p.length {
                let offset_in_piece = byte_idx - curr_byte;
                // Fast path: if char count equals byte length, all characters are single-byte ASCII
                if p.char_count == p.length {
                    return curr_char + offset_in_piece;
                }
                let text = p.get_text(&self.original, &self.added);
                let chars_in_offset = text[..offset_in_piece].chars().count();
                return curr_char + chars_in_offset;
            }
            curr_byte += p.length;
            curr_char += p.char_count;
        }
        self.total_chars
    }

    /// Returns the starting character index of 0-based `target_line` without flattening the buffer.
    pub fn line_to_char(&self, target_line: usize) -> usize {
        if target_line == 0 {
            return 0;
        }
        if target_line > self.total_line_feeds {
            return self.total_chars;
        }

        let mut curr_line = 0usize;
        let mut curr_char = 0usize;

        for p in &self.pieces {
            if curr_line + p.line_feed_cnt < target_line {
                curr_line += p.line_feed_cnt;
                curr_char += p.char_count;
                continue;
            }

            let text = p.get_text(&self.original, &self.added);
            let bytes = text.as_bytes();
            let mut last_pos = 0usize;
            for (i, &b) in bytes.iter().enumerate() {
                if b == b'\n' {
                    curr_line += 1;
                    if curr_line == target_line {
                        let byte_end = i + 1;
                        let chars_in_prefix = if p.char_count == p.length {
                            byte_end
                        } else {
                            text[..byte_end].chars().count()
                        };
                        return curr_char + chars_in_prefix;
                    }
                    last_pos = i + 1;
                }
            }
            let _ = last_pos;
            curr_char += p.char_count;
        }

        self.total_chars
    }

    /// Returns `(line_idx, col_chars)` (both 0-based) for `char_idx` without flattening the buffer.
    pub fn char_to_line_col(&self, char_idx: usize) -> (usize, usize) {
        let target = char_idx.min(self.total_chars);
        if target == 0 {
            return (0, 0);
        }

        let mut curr_char = 0usize;
        let mut curr_line = 0usize;
        let mut last_line_start_char = 0usize;

        for p in &self.pieces {
            if curr_char + p.char_count < target {
                if p.line_feed_cnt > 0 {
                    let text = p.get_text(&self.original, &self.added);
                    if let Some(last_nl_byte) = text.as_bytes().iter().rposition(|&b| b == b'\n') {
                        let byte_after_nl = last_nl_byte + 1;
                        let chars_before_nl_end = if p.char_count == p.length {
                            byte_after_nl
                        } else {
                            text[..byte_after_nl].chars().count()
                        };
                        last_line_start_char = curr_char + chars_before_nl_end;
                    }
                    curr_line += p.line_feed_cnt;
                }
                curr_char += p.char_count;
                continue;
            }

            let offset_chars = target - curr_char;
            if p.line_feed_cnt > 0 {
                let offset_bytes = p.char_to_byte_offset(&self.original, &self.added, offset_chars);
                let prefix = &p.get_text(&self.original, &self.added)[..offset_bytes];
                let lf_count = prefix.bytes().filter(|&b| b == b'\n').count();
                if lf_count > 0 {
                    curr_line += lf_count;
                    let last_nl_byte = prefix
                        .as_bytes()
                        .iter()
                        .rposition(|&b| b == b'\n')
                        .unwrap();
                    let byte_after_nl = last_nl_byte + 1;
                    let chars_to_nl_end = if p.char_count == p.length {
                        byte_after_nl
                    } else {
                        prefix[..byte_after_nl].chars().count()
                    };
                    last_line_start_char = curr_char + chars_to_nl_end;
                }
            }

            return (curr_line, target - last_line_start_char);
        }

        (curr_line, target - last_line_start_char)
    }

    /// Returns the character range `start..end` (excluding trailing `'\n'`) of 0-based `line_idx`.
    pub fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        let start = self.line_to_char(line_idx);
        let end = if line_idx >= self.total_line_feeds {
            self.total_chars
        } else {
            self.line_to_char(line_idx + 1).saturating_sub(1)
        };
        start..end.max(start)
    }

    /// Extracts characters in `char_range` into `out` (cleared first) without flattening the full buffer.
    pub fn extract_char_range(&self, char_range: Range<usize>, out: &mut String) {
        out.clear();
        let start = char_range.start.min(self.total_chars);
        let end = char_range.end.min(self.total_chars);
        if start >= end {
            return;
        }

        let mut curr_char = 0usize;
        for p in &self.pieces {
            let piece_start = curr_char;
            let piece_end = curr_char + p.char_count;
            curr_char = piece_end;

            if piece_end <= start {
                continue;
            }
            if piece_start >= end {
                break;
            }

            let local_start = start.saturating_sub(piece_start);
            let local_end = (end - piece_start).min(p.char_count);
            let byte_start = p.char_to_byte_offset(&self.original, &self.added, local_start);
            let byte_end = if local_end == p.char_count {
                p.length
            } else {
                p.char_to_byte_offset(&self.original, &self.added, local_end)
            };
            let text = p.get_text(&self.original, &self.added);
            out.push_str(&text[byte_start..byte_end]);
        }
    }

    /// Returns an owned `String` for `char_range` without flattening the full buffer.
    pub fn slice_chars(&self, char_range: Range<usize>) -> String {
        let mut s = String::new();
        self.extract_char_range(char_range, &mut s);
        s
    }

    /// Checks whether the text starts with CRLF within the first `max_bytes`
    /// without flattening the buffer into `cached_text`.
    pub fn has_crlf_in_initial_bytes(&self, max_bytes: usize) -> bool {
        if self.total_line_feeds == 0 || max_bytes == 0 || self.pieces.is_empty() {
            return false;
        }
        let mut remaining = max_bytes;
        let mut prev_byte = None;
        for p in &self.pieces {
            if remaining == 0 {
                break;
            }
            let text = p.get_text(&self.original, &self.added);
            let slice = &text.as_bytes()[..text.len().min(remaining)];
            if let Some(prev) = prev_byte {
                if prev == b'\r' && slice.first() == Some(&b'\n') {
                    return true;
                }
            }
            if slice.windows(2).any(|w| w == b"\r\n") {
                return true;
            }
            prev_byte = slice.last().copied();
            remaining -= slice.len();
        }
        false
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl Deref for PieceTable {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for PieceTable {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for PieceTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for p in &self.pieces {
            f.write_str(p.get_text(&self.original, &self.added))?;
        }
        Ok(())
    }
}

impl fmt::Debug for PieceTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PieceTable")
            .field("total_bytes", &self.total_bytes)
            .field("total_chars", &self.total_chars)
            .field("pieces_count", &self.pieces.len())
            .field("text", &self.as_str())
            .finish()
    }
}

impl PartialEq for PieceTable {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for PieceTable {}

impl PartialEq<str> for PieceTable {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for PieceTable {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for PieceTable {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<PieceTable> for str {
    fn eq(&self, other: &PieceTable) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<PieceTable> for &str {
    fn eq(&self, other: &PieceTable) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<PieceTable> for String {
    fn eq(&self, other: &PieceTable) -> bool {
        self.as_str() == other.as_str()
    }
}

impl From<&str> for PieceTable {
    fn from(s: &str) -> Self {
        PieceTable::from_str(s)
    }
}

impl From<String> for PieceTable {
    fn from(s: String) -> Self {
        PieceTable::from_string(s)
    }
}

impl From<&String> for PieceTable {
    fn from(s: &String) -> Self {
        PieceTable::from_str(s.as_str())
    }
}

impl From<PieceTable> for String {
    fn from(pt: PieceTable) -> Self {
        pt.build_string()
    }
}

impl From<&PieceTable> for String {
    fn from(pt: &PieceTable) -> Self {
        pt.build_string()
    }
}

impl eframe::egui::TextBuffer for PieceTable {
    fn is_mutable(&self) -> bool {
        true
    }

    fn as_str(&self) -> &str {
        self.as_str()
    }

    fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        self.insert(char_index, text)
    }

    fn delete_char_range(&mut self, char_range: Range<usize>) {
        self.delete(char_range);
    }

    fn clear(&mut self) {
        self.clear();
    }

    fn replace_with(&mut self, text: &str) {
        let char_count = text.chars().count();
        let byte_len = text.len();
        let line_feed_cnt = text.bytes().filter(|&b| b == b'\n').count();

        self.original = Arc::<str>::from(text);
        self.pieces.clear();
        if byte_len > 0 {
            self.pieces.push(Piece {
                buffer: BufferType::Original,
                start: 0,
                length: byte_len,
                char_count,
                line_feed_cnt,
            });
        }
        self.total_chars = char_count;
        self.total_bytes = byte_len;
        self.total_line_feeds = line_feed_cnt;
        self.cached_dirty.set(true);
    }

    fn take(&mut self) -> String {
        let s = self.build_string();
        self.clear();
        s
    }
}
