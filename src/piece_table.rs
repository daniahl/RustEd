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

const CHECKPOINT_STRIDE: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineCheckpoint {
    pub nl_count: usize,
    pub byte_offset: usize,
    pub char_offset: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub buffer: BufferType,
    pub start: usize,         // Byte offset in the respective buffer
    pub length: usize,        // Byte length of this piece
    pub char_count: usize,    // Unicode character (code point) count in this piece
    pub line_feed_cnt: usize, // Number of '\n' in this piece
    pub char_start: usize,    // Starting character offset in the underlying buffer
    pub nl_start: usize,      // Number of '\n' before `start` in the underlying buffer
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
        self.char_to_byte_offset_indexed(original, added, &[], char_offset)
    }

    pub fn char_to_byte_offset_indexed(
        &self,
        original: &str,
        added: &str,
        orig_checkpoints: &[LineCheckpoint],
        char_offset: usize,
    ) -> usize {
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
        if self.buffer == BufferType::Original && !orig_checkpoints.is_empty() {
            let abs_char = self.char_start + char_offset;
            let idx = orig_checkpoints.partition_point(|cp| cp.char_offset <= abs_char);
            if idx > 0 {
                let cp = &orig_checkpoints[idx - 1];
                if cp.char_offset >= self.char_start && cp.byte_offset >= self.start {
                    let rem_chars = abs_char - cp.char_offset;
                    let local_byte = cp.byte_offset - self.start;
                    if rem_chars == 0 {
                        return local_byte;
                    }
                    let tail = &original[cp.byte_offset..self.start + self.length];
                    return tail
                        .char_indices()
                        .nth(rem_chars)
                        .map(|(b, _)| local_byte + b)
                        .unwrap_or(self.length);
                }
            }
        }
        let text = self.get_text(original, added);
        text.char_indices()
            .nth(char_offset)
            .map(|(b, _)| b)
            .unwrap_or(self.length)
    }

    /// Splits this piece into two pieces at `char_offset` (relative to the piece).
    pub fn split_at_char(&self, original: &str, added: &str, char_offset: usize) -> (Piece, Piece) {
        self.split_at_char_indexed(original, added, &[], char_offset)
    }

    pub fn split_at_char_indexed(
        &self,
        original: &str,
        added: &str,
        orig_checkpoints: &[LineCheckpoint],
        char_offset: usize,
    ) -> (Piece, Piece) {
        let byte_offset =
            self.char_to_byte_offset_indexed(original, added, orig_checkpoints, char_offset);
        let left_lf = if self.line_feed_cnt == 0 {
            0
        } else if self.buffer == BufferType::Original && !orig_checkpoints.is_empty() {
            let abs_byte = self.start + byte_offset;
            let idx = orig_checkpoints.partition_point(|cp| cp.byte_offset <= abs_byte);
            if idx > 0 && orig_checkpoints[idx - 1].byte_offset >= self.start {
                let cp = &orig_checkpoints[idx - 1];
                let rem_lf = original.as_bytes()[cp.byte_offset..abs_byte]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count();
                (cp.nl_count - self.nl_start) + rem_lf
            } else {
                let text = self.get_text(original, added);
                text.as_bytes()[..byte_offset]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count()
            }
        } else {
            let text = self.get_text(original, added);
            if byte_offset <= self.length / 2 {
                text.as_bytes()[..byte_offset]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count()
            } else {
                let right_lf = text.as_bytes()[byte_offset..]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count();
                self.line_feed_cnt - right_lf
            }
        };
        let right_lf = self.line_feed_cnt - left_lf;

        let left = Piece {
            buffer: self.buffer,
            start: self.start,
            length: byte_offset,
            char_count: char_offset,
            line_feed_cnt: left_lf,
            char_start: self.char_start,
            nl_start: self.nl_start,
        };
        let right = Piece {
            buffer: self.buffer,
            start: self.start + byte_offset,
            length: self.length - byte_offset,
            char_count: self.char_count - char_offset,
            line_feed_cnt: right_lf,
            char_start: self.char_start + char_offset,
            nl_start: self.nl_start + left_lf,
        };
        (left, right)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceTableSnapshot {
    original: Arc<str>,
    orig_checkpoints: Arc<[LineCheckpoint]>,
    pieces: Arc<Vec<Piece>>,
    total_chars: usize,
    total_bytes: usize,
    total_line_feeds: usize,
}

pub struct PieceTable {
    original: Arc<str>,
    orig_checkpoints: Arc<[LineCheckpoint]>,
    added: String,
    added_chars: usize,
    added_lfs: usize,
    /// Copy-on-write piece list: snapshots share it in O(1); edits call Arc::make_mut.
    pieces: Arc<Vec<Piece>>,
    /// Locality hint (piece_idx, char_start_of_piece) for the last lookup/edit.
    hint: Cell<(usize, usize)>,
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
            orig_checkpoints: Arc::clone(&self.orig_checkpoints),
            added: self.added.clone(),
            added_chars: self.added_chars,
            added_lfs: self.added_lfs,
            pieces: Arc::clone(&self.pieces),
            hint: Cell::new((0, 0)),
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

fn scan_str_with_checkpoints(text: &str) -> (usize, usize, Vec<LineCheckpoint>) {
    let bytes = text.as_bytes();
    let mut char_count = 0usize;
    let mut line_feed_cnt = 0usize;
    let mut checkpoints = Vec::new();

    for (i, &b) in bytes.iter().enumerate() {
        if (b & 0xC0) != 0x80 {
            char_count += 1;
        }
        if b == b'\n' {
            line_feed_cnt += 1;
            if line_feed_cnt % CHECKPOINT_STRIDE == 0 {
                checkpoints.push(LineCheckpoint {
                    nl_count: line_feed_cnt,
                    byte_offset: i + 1,
                    char_offset: char_count,
                });
            }
        }
    }

    (char_count, line_feed_cnt, checkpoints)
}

#[inline]
fn scan_str_counts(text: &str) -> (usize, usize) {
    let mut char_count = 0usize;
    let mut line_feed_cnt = 0usize;
    for &b in text.as_bytes() {
        if (b & 0xC0) != 0x80 {
            char_count += 1;
        }
        if b == b'\n' {
            line_feed_cnt += 1;
        }
    }
    (char_count, line_feed_cnt)
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
        let byte_len = original.len();
        let (char_count, line_feed_cnt, checkpoints) = scan_str_with_checkpoints(&original);

        let mut pieces = Vec::new();
        if byte_len > 0 {
            pieces.push(Piece {
                buffer: BufferType::Original,
                start: 0,
                length: byte_len,
                char_count,
                line_feed_cnt,
                char_start: 0,
                nl_start: 0,
            });
        }

        Self {
            original,
            orig_checkpoints: Arc::from(checkpoints),
            added: String::new(),
            added_chars: 0,
            added_lfs: 0,
            pieces: Arc::new(pieces),
            hint: Cell::new((0, 0)),
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
            orig_checkpoints: Arc::clone(&self.orig_checkpoints),
            pieces: Arc::clone(&self.pieces),
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
            self.orig_checkpoints = Arc::clone(&snapshot.orig_checkpoints);
            self.pieces = Arc::clone(&snapshot.pieces);
            self.hint.set((0, 0));
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
        self.orig_checkpoints = Arc::from(&[][..]);
        self.added.clear();
        self.added_chars = 0;
        self.added_lfs = 0;
        self.pieces = Arc::new(Vec::new());
        self.hint.set((0, 0));
        self.total_chars = 0;
        self.total_bytes = 0;
        self.total_line_feeds = 0;
        unsafe {
            (*self.cached_text.get()).clear();
        }
        self.cached_dirty.set(false);
    }

    /// Finds the first piece whose end is >= `target` (char index, `<= total_chars`).
    /// Returns `(piece_idx, char_start_of_piece)`. Resumes from the last lookup so that
    /// localized editing is O(1) amortized instead of O(P).
    fn locate_char(&self, target: usize) -> (usize, usize) {
        let pieces = &self.pieces[..];
        let (mut idx, mut start) = self.hint.get();
        if idx >= pieces.len() || start > self.total_chars {
            idx = 0;
            start = 0;
        }
        while idx > 0 && target <= start {
            idx -= 1;
            start -= pieces[idx].char_count;
        }
        while idx + 1 < pieces.len() && target > start + pieces[idx].char_count {
            start += pieces[idx].char_count;
            idx += 1;
        }
        self.hint.set((idx, start));
        (idx, start)
    }

    /// Inserts `text` at the character index `char_index`.
    /// Returns the number of characters inserted.
    pub fn insert(&mut self, char_index: usize, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }

        let text_bytes = text.len();
        let (text_chars, text_lf) = scan_str_counts(text);

        let add_start = self.added.len();
        let add_char_start = self.added_chars;
        let add_nl_start = self.added_lfs;
        self.added.push_str(text);
        self.added_chars += text_chars;
        self.added_lfs += text_lf;

        let target_char = char_index.min(self.total_chars);
        let new_piece = Piece {
            buffer: BufferType::Added,
            start: add_start,
            length: text_bytes,
            char_count: text_chars,
            line_feed_cnt: text_lf,
            char_start: add_char_start,
            nl_start: add_nl_start,
        };

        let (target_idx, curr_char) = if self.pieces.is_empty() || target_char == 0 {
            (0, 0)
        } else {
            self.locate_char(target_char)
        };
        let pieces = Arc::make_mut(&mut self.pieces);
        let mut new_hint = (target_idx, curr_char);

        if pieces.is_empty() {
            pieces.push(new_piece);
            new_hint = (0, 0);
        } else if target_char == self.total_chars {
            // Append at the end: extend the previous Added piece when contiguous.
            let last_idx = pieces.len() - 1;
            let last = &mut pieces[last_idx];
            if last.buffer == BufferType::Added && last.start + last.length == add_start {
                last.length += text_bytes;
                last.char_count += text_chars;
                last.line_feed_cnt += text_lf;
                new_hint = (last_idx, self.total_chars + text_chars - last.char_count);
            } else {
                pieces.push(new_piece);
                new_hint = (last_idx + 1, self.total_chars);
            }
        } else if target_char == 0 {
            pieces.insert(0, new_piece);
            new_hint = (0, 0);
        } else {
            let offset_in_piece = target_char - curr_char;
            let piece = pieces[target_idx];

            if offset_in_piece == 0 {
                let mut merged = false;
                if target_idx > 0 {
                    let prev = &mut pieces[target_idx - 1];
                    if prev.buffer == BufferType::Added && prev.start + prev.length == add_start {
                        prev.length += text_bytes;
                        prev.char_count += text_chars;
                        prev.line_feed_cnt += text_lf;
                        new_hint = (target_idx - 1, curr_char - (prev.char_count - text_chars));
                        merged = true;
                    }
                }
                if !merged {
                    pieces.insert(target_idx, new_piece);
                }
            } else if offset_in_piece == piece.char_count {
                if piece.buffer == BufferType::Added && piece.start + piece.length == add_start {
                    let target_piece = &mut pieces[target_idx];
                    target_piece.length += text_bytes;
                    target_piece.char_count += text_chars;
                    target_piece.line_feed_cnt += text_lf;
                } else {
                    pieces.insert(target_idx + 1, new_piece);
                }
            } else {
                let (left, right) = piece.split_at_char_indexed(
                    &self.original,
                    &self.added,
                    &self.orig_checkpoints,
                    offset_in_piece,
                );
                pieces[target_idx] = left;
                pieces.insert(target_idx + 1, new_piece);
                pieces.insert(target_idx + 2, right);
            }
        }

        self.hint.set(new_hint);
        self.total_chars += text_chars;
        self.total_bytes += text_bytes;
        self.total_line_feeds += text_lf;
        self.cached_dirty.set(true);

        text_chars
    }

    /// Deletes characters in the range `char_range`.
    ///
    /// Only the pieces overlapping the range are touched (via a single `splice`), the totals
    /// are adjusted incrementally and only the seam is coalesced.
    pub fn delete(&mut self, char_range: Range<usize>) {
        let start = char_range.start.min(self.total_chars);
        let end = char_range.end.min(self.total_chars);

        if start >= end || self.pieces.is_empty() {
            return;
        }

        // First piece that overlaps [start, end): the first with end > start.
        let (mut first, mut first_start) = self.locate_char(start);
        while first < self.pieces.len() && first_start + self.pieces[first].char_count <= start {
            first_start += self.pieces[first].char_count;
            first += 1;
        }

        // Find the exclusive upper bound of overlapping pieces.
        let mut last = first;
        let mut last_start = first_start;
        while last < self.pieces.len() && last_start < end {
            last_start += self.pieces[last].char_count;
            last += 1;
        }
        // `last_start` is now the char end of piece `last - 1`.
        let last_piece_start = last_start - self.pieces[last - 1].char_count;

        let mut replacement: [Option<Piece>; 2] = [None, None];
        if first_start < start {
            let (left, _) = self.pieces[first].split_at_char_indexed(
                &self.original,
                &self.added,
                &self.orig_checkpoints,
                start - first_start,
            );
            replacement[0] = Some(left);
        }
        if last_start > end {
            let (_, right) = self.pieces[last - 1].split_at_char_indexed(
                &self.original,
                &self.added,
                &self.orig_checkpoints,
                end - last_piece_start,
            );
            replacement[1] = Some(right);
        }

        let (mut old_bytes, mut old_lfs) = (0usize, 0usize);
        for p in &self.pieces[first..last] {
            old_bytes += p.length;
            old_lfs += p.line_feed_cnt;
        }
        let (mut new_bytes, mut new_lfs) = (0usize, 0usize);
        for p in replacement.iter().flatten() {
            new_bytes += p.length;
            new_lfs += p.line_feed_cnt;
        }

        let repl_len = replacement.iter().flatten().count();
        let pieces = Arc::make_mut(&mut self.pieces);
        pieces.splice(first..last, replacement.into_iter().flatten());

        // Coalesce only around the seam.
        let mut merged = false;
        let mut i = first.saturating_sub(1);
        let mut upper = first + repl_len;
        while i + 1 < pieces.len() && i < upper {
            let next = pieces[i + 1];
            let curr = &mut pieces[i];
            if curr.buffer == next.buffer && curr.start + curr.length == next.start {
                curr.length += next.length;
                curr.char_count += next.char_count;
                curr.line_feed_cnt += next.line_feed_cnt;
                pieces.remove(i + 1);
                upper = upper.saturating_sub(1);
                merged = true;
            } else {
                i += 1;
            }
        }

        self.hint.set(if merged { (0, 0) } else { (first, first_start) });
        self.total_chars -= end - start;
        self.total_bytes = self.total_bytes - old_bytes + new_bytes;
        self.total_line_feeds = self.total_line_feeds - old_lfs + new_lfs;
        self.cached_dirty.set(true);
    }

    /// Merges contiguous pieces that point to adjacent regions in the same buffer in a single O(M) pass.
    pub fn coalesce(&mut self) {
        if self.pieces.len() <= 1 {
            return;
        }
        self.hint.set((0, 0));
        let pieces = Arc::make_mut(&mut self.pieces);
        let mut write = 0;
        for read in 1..pieces.len() {
            let next = pieces[read];
            let curr = &mut pieces[write];
            if curr.buffer == next.buffer && curr.start + curr.length == next.start {
                curr.length += next.length;
                curr.char_count += next.char_count;
                curr.line_feed_cnt += next.line_feed_cnt;
            } else {
                write += 1;
                pieces[write] = next;
            }
        }
        pieces.truncate(write + 1);
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
                    for p in self.pieces.iter() {
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
        for p in self.pieces.iter() {
            s.push_str(p.get_text(&self.original, &self.added));
        }
        s
    }

    /// Streams all piece slices directly to `writer` without flattening the buffer in memory.
    pub fn write_to<W: std::io::Write>(&self, mut writer: W) -> std::io::Result<()> {
        for p in self.pieces.iter() {
            writer.write_all(p.get_text(&self.original, &self.added).as_bytes())?;
        }
        writer.flush()
    }

    /// Compares the piece table contents against `other` without allocating a flattened string.
    pub fn eq_str(&self, other: &str) -> bool {
        if self.total_bytes != other.len() {
            return false;
        }
        let mut offset = 0usize;
        for p in self.pieces.iter() {
            let chunk = p.get_text(&self.original, &self.added);
            let end = offset + chunk.len();
            if &other.as_bytes()[offset..end] != chunk.as_bytes() {
                return false;
            }
            offset = end;
        }
        true
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
        for p in self.pieces.iter() {
            if char_idx <= curr_char + p.char_count {
                let offset_in_piece = char_idx - curr_char;
                return curr_byte
                    + p.char_to_byte_offset_indexed(
                        &self.original,
                        &self.added,
                        &self.orig_checkpoints,
                        offset_in_piece,
                    );
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
        for p in self.pieces.iter() {
            if byte_idx <= curr_byte + p.length {
                let offset_in_piece = byte_idx - curr_byte;
                // Fast path: if char count equals byte length, all characters are single-byte ASCII
                if p.char_count == p.length {
                    return curr_char + offset_in_piece;
                }
                if p.buffer == BufferType::Original && !self.orig_checkpoints.is_empty() {
                    let abs_byte = p.start + offset_in_piece;
                    let idx = self
                        .orig_checkpoints
                        .partition_point(|cp| cp.byte_offset <= abs_byte);
                    if idx > 0 {
                        let cp = &self.orig_checkpoints[idx - 1];
                        if cp.byte_offset >= p.start && cp.char_offset >= p.char_start {
                            let rem_chars = self.original[cp.byte_offset..abs_byte].chars().count();
                            return curr_char + (cp.char_offset - p.char_start) + rem_chars;
                        }
                    }
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

    /// Internal helper: returns `(char_idx, piece_idx, byte_offset_in_piece)` for the start of `target_line`.
    fn find_line_start_pos(&self, target_line: usize) -> (usize, usize, usize) {
        if target_line == 0 {
            return (0, 0, 0);
        }
        if target_line > self.total_line_feeds {
            return (self.total_chars, self.pieces.len(), 0);
        }

        let mut curr_line = 0usize;
        let mut curr_char = 0usize;

        for (p_idx, p) in self.pieces.iter().enumerate() {
            if curr_line + p.line_feed_cnt < target_line {
                curr_line += p.line_feed_cnt;
                curr_char += p.char_count;
                continue;
            }

            let rel_line = target_line - curr_line;
            let text = p.get_text(&self.original, &self.added);
            let bytes = text.as_bytes();

            let (scan_byte_start, mut scan_char_offset, mut rem_lines) =
                if p.buffer == BufferType::Original && !self.orig_checkpoints.is_empty() {
                    let abs_nl = p.nl_start + rel_line;
                    let cp_idx = abs_nl / CHECKPOINT_STRIDE;
                    if cp_idx > 0 {
                        let cp = &self.orig_checkpoints[cp_idx - 1];
                        if cp.nl_count > p.nl_start && cp.byte_offset >= p.start {
                            (
                                cp.byte_offset - p.start,
                                cp.char_offset - p.char_start,
                                abs_nl - cp.nl_count,
                            )
                        } else {
                            (0, 0, rel_line)
                        }
                    } else {
                        (0, 0, rel_line)
                    }
                } else {
                    (0, 0, rel_line)
                };

            if rem_lines == 0 {
                return (curr_char + scan_char_offset, p_idx, scan_byte_start);
            }

            for (i, &b) in bytes[scan_byte_start..].iter().enumerate() {
                if b == b'\n' {
                    rem_lines -= 1;
                    if rem_lines == 0 {
                        let byte_end = scan_byte_start + i + 1;
                        let chars_in_slice = if p.char_count == p.length {
                            byte_end - scan_byte_start
                        } else {
                            text[scan_byte_start..byte_end].chars().count()
                        };
                        scan_char_offset += chars_in_slice;
                        return (curr_char + scan_char_offset, p_idx, byte_end);
                    }
                }
            }
            curr_char += p.char_count;
            curr_line += p.line_feed_cnt;
        }

        (self.total_chars, self.pieces.len(), 0)
    }

    /// Returns the starting character index of 0-based `target_line` without flattening the buffer.
    pub fn line_to_char(&self, target_line: usize) -> usize {
        self.find_line_start_pos(target_line).0
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

        for p in self.pieces.iter() {
            if curr_char + p.char_count < target {
                if p.line_feed_cnt > 0 {
                    let text = p.get_text(&self.original, &self.added);
                    if let Some(last_nl_byte) = text.as_bytes().iter().rposition(|&b| b == b'\n') {
                        let byte_after_nl = last_nl_byte + 1;
                        let chars_before_nl_end = if p.char_count == p.length {
                            byte_after_nl
                        } else {
                            p.char_count - text[byte_after_nl..].chars().count()
                        };
                        last_line_start_char = curr_char + chars_before_nl_end;
                    }
                    curr_line += p.line_feed_cnt;
                }
                curr_char += p.char_count;
                continue;
            }

            let offset_chars = target - curr_char;
            if p.line_feed_cnt > 0 && offset_chars > 0 {
                let offset_bytes = p.char_to_byte_offset_indexed(
                    &self.original,
                    &self.added,
                    &self.orig_checkpoints,
                    offset_chars,
                );
                let text = p.get_text(&self.original, &self.added);
                let prefix = &text[..offset_bytes];

                let (base_lf, scan_start_byte, base_char_offset) =
                    if p.buffer == BufferType::Original && !self.orig_checkpoints.is_empty() {
                        let abs_byte = p.start + offset_bytes;
                        let idx = self
                            .orig_checkpoints
                            .partition_point(|cp| cp.byte_offset <= abs_byte);
                        if idx > 0 {
                            let cp = &self.orig_checkpoints[idx - 1];
                            if cp.byte_offset > p.start && cp.nl_count > p.nl_start {
                                (
                                    cp.nl_count - p.nl_start,
                                    cp.byte_offset - p.start,
                                    cp.char_offset - p.char_start,
                                )
                            } else {
                                (0, 0, 0)
                            }
                        } else {
                            (0, 0, 0)
                        }
                    } else {
                        (0, 0, 0)
                    };

                let tail_bytes = &prefix.as_bytes()[scan_start_byte..];
                let tail_lf = tail_bytes.iter().filter(|&&b| b == b'\n').count();
                let total_lf = base_lf + tail_lf;
                if total_lf > 0 {
                    curr_line += total_lf;
                    if tail_lf > 0 {
                        let rel_nl = tail_bytes.iter().rposition(|&b| b == b'\n').unwrap() + 1;
                        let chars_in_tail = if p.char_count == p.length {
                            rel_nl
                        } else {
                            prefix[scan_start_byte..scan_start_byte + rel_nl]
                                .chars()
                                .count()
                        };
                        last_line_start_char = curr_char + base_char_offset + chars_in_tail;
                    } else {
                        last_line_start_char = curr_char + base_char_offset;
                    }
                }
            }

            return (curr_line, target - last_line_start_char);
        }

        (curr_line, target - last_line_start_char)
    }

    /// Returns the character range `start..end` (excluding trailing `'\n'`) of 0-based `line_idx`.
    pub fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        if line_idx > self.total_line_feeds {
            return self.total_chars..self.total_chars;
        }
        let (start, start_piece_idx, start_byte_in_piece) = self.find_line_start_pos(line_idx);
        if line_idx == self.total_line_feeds {
            return start..self.total_chars.max(start);
        }

        let mut curr_char = start;
        for (idx, p) in self.pieces[start_piece_idx..].iter().enumerate() {
            let byte_offset = if idx == 0 { start_byte_in_piece } else { 0 };
            if byte_offset >= p.length {
                continue;
            }
            if idx > 0 && p.line_feed_cnt == 0 {
                curr_char += p.char_count;
                continue;
            }
            let text = p.get_text(&self.original, &self.added);
            let slice = &text[byte_offset..];
            if let Some(nl_pos) = slice.as_bytes().iter().position(|&b| b == b'\n') {
                let chars_in_line = if p.char_count == p.length {
                    nl_pos
                } else {
                    slice[..nl_pos].chars().count()
                };
                let end = curr_char + chars_in_line;
                return start..end.max(start);
            } else {
                let rem_chars = if idx == 0 {
                    if p.char_count == p.length {
                        slice.len()
                    } else {
                        slice.chars().count()
                    }
                } else {
                    p.char_count
                };
                curr_char += rem_chars;
            }
        }

        start..self.total_chars.max(start)
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
        for p in self.pieces.iter() {
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
            let byte_start = p.char_to_byte_offset_indexed(
                &self.original,
                &self.added,
                &self.orig_checkpoints,
                local_start,
            );
            let byte_end = if local_end == p.char_count {
                p.length
            } else {
                p.char_to_byte_offset_indexed(
                    &self.original,
                    &self.added,
                    &self.orig_checkpoints,
                    local_end,
                )
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
        for p in self.pieces.iter() {
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
        for p in self.pieces.iter() {
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
        self.total_bytes == other.total_bytes
            && self.total_chars == other.total_chars
            && self.as_str() == other.as_str()
    }
}

impl Eq for PieceTable {}

impl PartialEq<str> for PieceTable {
    fn eq(&self, other: &str) -> bool {
        self.total_bytes == other.len() && self.as_str() == other
    }
}

impl PartialEq<&str> for PieceTable {
    fn eq(&self, other: &&str) -> bool {
        self.total_bytes == other.len() && self.as_str() == *other
    }
}

impl PartialEq<String> for PieceTable {
    fn eq(&self, other: &String) -> bool {
        self.total_bytes == other.len() && self.as_str() == other.as_str()
    }
}

impl PartialEq<PieceTable> for str {
    fn eq(&self, other: &PieceTable) -> bool {
        self.len() == other.total_bytes && self == other.as_str()
    }
}

impl PartialEq<PieceTable> for &str {
    fn eq(&self, other: &PieceTable) -> bool {
        self.len() == other.total_bytes && *self == other.as_str()
    }
}

impl PartialEq<PieceTable> for String {
    fn eq(&self, other: &PieceTable) -> bool {
        self.len() == other.total_bytes && self.as_str() == other.as_str()
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

    fn insert_text(&mut self, text: &str, char_index: eframe::egui::text::CharIndex) -> usize {
        self.insert(char_index.0, text)
    }

    fn delete_char_range(&mut self, char_range: Range<eframe::egui::text::CharIndex>) {
        self.delete(char_range.start.0..char_range.end.0);
    }

    fn type_id(&self) -> std::any::TypeId {
        std::any::TypeId::of::<Self>()
    }

    fn clear(&mut self) {
        self.clear();
    }

    fn replace_with(&mut self, text: &str) {
        let byte_len = text.len();
        let (char_count, line_feed_cnt, checkpoints) = scan_str_with_checkpoints(text);

        self.original = Arc::<str>::from(text);
        self.orig_checkpoints = Arc::from(checkpoints);
        self.hint.set((0, 0));
        let pieces = Arc::make_mut(&mut self.pieces);
        pieces.clear();
        if byte_len > 0 {
            pieces.push(Piece {
                buffer: BufferType::Original,
                start: 0,
                length: byte_len,
                char_count,
                line_feed_cnt,
                char_start: 0,
                nl_start: 0,
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
