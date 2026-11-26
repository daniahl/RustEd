//! Piece Table text buffer data structure (similar to VS Code's text buffer architecture).
//!
//! Stores document text across two append-only buffers (`original` and `added`)
//! referenced by lightweight piece descriptors. Edits (inserts and deletes) split or
//! extend piece descriptors without ever shifting or reallocating existing text in memory.

use std::{
    cell::{Cell, RefCell},
    fmt,
    ops::Range,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

#[inline]
fn next_table_id() -> u64 {
    NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed)
}

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
    pub(crate) buffer: BufferType,
    pub(crate) start: usize,         // Byte offset in the respective buffer
    pub(crate) length: usize,        // Byte length of this piece
    pub(crate) char_count: usize,    // Unicode character (code point) count in this piece
    pub(crate) line_feed_cnt: usize, // Number of '\n' in this piece
    pub(crate) char_start: usize,    // Starting character offset in the underlying buffer
    pub(crate) nl_start: usize,      // Number of '\n' before `start` in the underlying buffer
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
    table_id: u64,
    original: Arc<str>,
    orig_checkpoints: Arc<[LineCheckpoint]>,
    pieces: Arc<Vec<Piece>>,
    added_epoch: u64,
    added_piece_bytes: usize,
    added_len_at_snapshot: usize,
    total_chars: usize,
    total_bytes: usize,
    total_line_feeds: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PieceHint {
    idx: usize,
    char_start: usize,
    byte_start: usize,
    nl_start: usize,
}

pub struct PieceTable {
    table_id: u64,
    original: Arc<str>,
    orig_checkpoints: Arc<[LineCheckpoint]>,
    added: String,
    added_epoch: u64,
    added_chars: usize,
    added_lfs: usize,
    added_piece_bytes: usize,
    /// Copy-on-write piece list: snapshots share it in O(1); edits call Arc::make_mut.
    pieces: Arc<Vec<Piece>>,
    /// Locality hint `(idx, char_start, byte_start, nl_start)` for the last lookup/edit.
    hint: Cell<PieceHint>,
    total_chars: usize,
    total_bytes: usize,
    total_line_feeds: usize,
    /// Lazily-flattened text cache. `None` means dirty (needs rebuild).
    /// Uses `RefCell` for safe single-threaded interior mutability.
    cached_text: RefCell<Option<String>>,
}

impl Clone for PieceTable {
    fn clone(&self) -> Self {
        Self {
            table_id: next_table_id(),
            original: Arc::clone(&self.original),
            orig_checkpoints: Arc::clone(&self.orig_checkpoints),
            added: self.added.clone(),
            added_epoch: self.added_epoch,
            added_chars: self.added_chars,
            added_lfs: self.added_lfs,
            added_piece_bytes: self.added_piece_bytes,
            pieces: Arc::clone(&self.pieces),
            hint: Cell::new(PieceHint::default()),
            total_chars: self.total_chars,
            total_bytes: self.total_bytes,
            total_line_feeds: self.total_line_feeds,
            cached_text: RefCell::new(None),
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
            if line_feed_cnt.is_multiple_of(CHECKPOINT_STRIDE) {
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

    #[allow(clippy::should_implement_trait)]
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
            table_id: next_table_id(),
            original,
            orig_checkpoints: Arc::from(checkpoints),
            added: String::new(),
            added_epoch: 0,
            added_chars: 0,
            added_lfs: 0,
            added_piece_bytes: 0,
            pieces: Arc::new(pieces),
            hint: Cell::new(PieceHint::default()),
            total_chars: char_count,
            total_bytes: byte_len,
            total_line_feeds: line_feed_cnt,
            cached_text: RefCell::new(None),
        }
    }

    /// Captures a lightweight O(1) copy-on-write snapshot of piece descriptors without copying text buffers.
    pub fn snapshot(&self) -> PieceTableSnapshot {
        PieceTableSnapshot {
            table_id: self.table_id,
            original: Arc::clone(&self.original),
            orig_checkpoints: Arc::clone(&self.orig_checkpoints),
            pieces: Arc::clone(&self.pieces),
            added_epoch: self.added_epoch,
            added_piece_bytes: self.added_piece_bytes,
            added_len_at_snapshot: self.added.len(),
            total_chars: self.total_chars,
            total_bytes: self.total_bytes,
            total_line_feeds: self.total_line_feeds,
        }
    }

    #[inline]
    fn snapshot_compatible(&self, snapshot: &PieceTableSnapshot) -> bool {
        snapshot.table_id == self.table_id
            && (snapshot.added_piece_bytes == 0
                || (snapshot.added_epoch == self.added_epoch
                    && snapshot.added_len_at_snapshot <= self.added.len()))
    }

    /// Restores piece descriptors from a previously captured `PieceTableSnapshot` in O(1) time.
    /// Returns `false` if the snapshot belongs to another table or invalidated `added` epoch.
    pub fn restore_snapshot(&mut self, snapshot: &PieceTableSnapshot) -> bool {
        if !self.snapshot_compatible(snapshot) {
            return false;
        }
        self.original = Arc::clone(&snapshot.original);
        self.orig_checkpoints = Arc::clone(&snapshot.orig_checkpoints);
        self.pieces = Arc::clone(&snapshot.pieces);
        self.added_piece_bytes = snapshot.added_piece_bytes;
        self.hint.set(PieceHint::default());
        self.total_chars = snapshot.total_chars;
        self.total_bytes = snapshot.total_bytes;
        self.total_line_feeds = snapshot.total_line_feeds;
        *self.cached_text.borrow_mut() = None;
        true
    }

    /// Returns `true` if the current buffer contents match `snapshot` without flattening.
    pub fn matches_snapshot(&self, snapshot: &PieceTableSnapshot) -> bool {
        if self.total_bytes != snapshot.total_bytes
            || self.total_chars != snapshot.total_chars
            || self.total_line_feeds != snapshot.total_line_feeds
        {
            return false;
        }
        if !self.snapshot_compatible(snapshot) {
            return false;
        }
        if Arc::ptr_eq(&self.original, &snapshot.original)
            && (Arc::ptr_eq(&self.pieces, &snapshot.pieces) || *self.pieces == *snapshot.pieces)
        {
            return true;
        }
        self.eq_snapshot_content(snapshot)
    }

    fn eq_snapshot_content(&self, snapshot: &PieceTableSnapshot) -> bool {
        if !self.snapshot_compatible(snapshot) {
            return false;
        }
        let mut a_idx = 0usize;
        let mut a_off = 0usize;
        let mut b_idx = 0usize;
        let mut b_off = 0usize;
        let a_pieces = &self.pieces[..];
        let b_pieces = &snapshot.pieces[..];

        while a_idx < a_pieces.len() && b_idx < b_pieces.len() {
            if a_off >= a_pieces[a_idx].length {
                a_idx += 1;
                a_off = 0;
                continue;
            }
            if b_off >= b_pieces[b_idx].length {
                b_idx += 1;
                b_off = 0;
                continue;
            }
            let a_slice = &a_pieces[a_idx].get_text(&self.original, &self.added).as_bytes()[a_off..];
            let b_slice =
                &b_pieces[b_idx].get_text(&snapshot.original, &self.added).as_bytes()[b_off..];
            let step = a_slice.len().min(b_slice.len());
            if a_slice[..step] != b_slice[..step] {
                return false;
            }
            a_off += step;
            b_off += step;
        }
        while a_idx < a_pieces.len() && a_off >= a_pieces[a_idx].length {
            a_idx += 1;
            a_off = 0;
        }
        while b_idx < b_pieces.len() && b_off >= b_pieces[b_idx].length {
            b_idx += 1;
            b_off = 0;
        }
        a_idx == a_pieces.len() && b_idx == b_pieces.len()
    }

    /// Releases any lazily flattened string cache without altering buffer contents.
    pub fn drop_cached_text(&mut self) {
        *self.cached_text.borrow_mut() = None;
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
        self.added_epoch = self.added_epoch.wrapping_add(1);
        self.added_chars = 0;
        self.added_lfs = 0;
        self.added_piece_bytes = 0;
        self.pieces = Arc::new(Vec::new());
        self.hint.set(PieceHint::default());
        self.total_chars = 0;
        self.total_bytes = 0;
        self.total_line_feeds = 0;
        *self.cached_text.borrow_mut() = None;
    }

    #[inline]
    fn valid_hint(&self) -> PieceHint {
        let pieces = &self.pieces[..];
        let h = self.hint.get();
        if h.idx >= pieces.len()
            || h.char_start > self.total_chars
            || h.byte_start > self.total_bytes
            || h.nl_start > self.total_line_feeds
        {
            PieceHint::default()
        } else {
            h
        }
    }

    /// Finds the first piece whose char end is >= `target` (char index, `<= total_chars`).
    /// Resumes from the last lookup so localized editing/queries are O(1) amortized instead of O(P).
    fn locate_char(&self, target: usize) -> PieceHint {
        let pieces = &self.pieces[..];
        if pieces.is_empty() {
            return PieceHint::default();
        }
        let mut h = self.valid_hint();
        while h.idx > 0 && target <= h.char_start {
            h.idx -= 1;
            let p = &pieces[h.idx];
            h.char_start -= p.char_count;
            h.byte_start -= p.length;
            h.nl_start -= p.line_feed_cnt;
        }
        while h.idx + 1 < pieces.len() && target > h.char_start + pieces[h.idx].char_count {
            let p = &pieces[h.idx];
            h.char_start += p.char_count;
            h.byte_start += p.length;
            h.nl_start += p.line_feed_cnt;
            h.idx += 1;
        }
        self.hint.set(h);
        h
    }

    /// Finds the first piece whose byte end is >= `target` (byte index, `<= total_bytes`).
    fn locate_byte(&self, target: usize) -> PieceHint {
        let pieces = &self.pieces[..];
        if pieces.is_empty() {
            return PieceHint::default();
        }
        let mut h = self.valid_hint();
        while h.idx > 0 && target <= h.byte_start {
            h.idx -= 1;
            let p = &pieces[h.idx];
            h.char_start -= p.char_count;
            h.byte_start -= p.length;
            h.nl_start -= p.line_feed_cnt;
        }
        while h.idx + 1 < pieces.len() && target > h.byte_start + pieces[h.idx].length {
            let p = &pieces[h.idx];
            h.char_start += p.char_count;
            h.byte_start += p.length;
            h.nl_start += p.line_feed_cnt;
            h.idx += 1;
        }
        self.hint.set(h);
        h
    }

    /// Finds the first piece containing the `target_line`-th newline (`1 <= target_line <= total_line_feeds`).
    fn locate_line(&self, target_line: usize) -> PieceHint {
        let pieces = &self.pieces[..];
        if pieces.is_empty() {
            return PieceHint::default();
        }
        let mut h = self.valid_hint();
        while h.idx > 0 && target_line <= h.nl_start {
            h.idx -= 1;
            let p = &pieces[h.idx];
            h.char_start -= p.char_count;
            h.byte_start -= p.length;
            h.nl_start -= p.line_feed_cnt;
        }
        while h.idx + 1 < pieces.len() && target_line > h.nl_start + pieces[h.idx].line_feed_cnt {
            let p = &pieces[h.idx];
            h.char_start += p.char_count;
            h.byte_start += p.length;
            h.nl_start += p.line_feed_cnt;
            h.idx += 1;
        }
        self.hint.set(h);
        h
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

        let curr_hint = if self.pieces.is_empty() || target_char == 0 {
            PieceHint::default()
        } else {
            self.locate_char(target_char)
        };
        let target_idx = curr_hint.idx;
        let curr_char = curr_hint.char_start;
        let pieces = Arc::make_mut(&mut self.pieces);
        let mut new_hint = curr_hint;

        if pieces.is_empty() {
            pieces.push(new_piece);
            new_hint = PieceHint::default();
        } else if target_char == self.total_chars {
            // Append at the end: extend the previous Added piece when contiguous.
            let last_idx = pieces.len() - 1;
            let last = &mut pieces[last_idx];
            if last.buffer == BufferType::Added && last.start + last.length == add_start {
                last.length += text_bytes;
                last.char_count += text_chars;
                last.line_feed_cnt += text_lf;
                new_hint = PieceHint {
                    idx: last_idx,
                    char_start: self.total_chars + text_chars - last.char_count,
                    byte_start: self.total_bytes + text_bytes - last.length,
                    nl_start: self.total_line_feeds + text_lf - last.line_feed_cnt,
                };
            } else {
                pieces.push(new_piece);
                new_hint = PieceHint {
                    idx: last_idx + 1,
                    char_start: self.total_chars,
                    byte_start: self.total_bytes,
                    nl_start: self.total_line_feeds,
                };
            }
        } else if target_char == 0 {
            pieces.insert(0, new_piece);
            new_hint = PieceHint::default();
        } else {
            let offset_in_piece = target_char - curr_char;
            let piece = pieces[target_idx];

            if offset_in_piece == 0 {
                let mut merged = false;
                if target_idx > 0 {
                    let prev = &mut pieces[target_idx - 1];
                    if prev.buffer == BufferType::Added && prev.start + prev.length == add_start {
                        let prev_old_chars = prev.char_count;
                        let prev_old_bytes = prev.length;
                        let prev_old_lfs = prev.line_feed_cnt;
                        prev.length += text_bytes;
                        prev.char_count += text_chars;
                        prev.line_feed_cnt += text_lf;
                        new_hint = PieceHint {
                            idx: target_idx - 1,
                            char_start: curr_hint.char_start - prev_old_chars,
                            byte_start: curr_hint.byte_start - prev_old_bytes,
                            nl_start: curr_hint.nl_start - prev_old_lfs,
                        };
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
        self.added_piece_bytes += text_bytes;
        self.total_chars += text_chars;
        self.total_bytes += text_bytes;
        self.total_line_feeds += text_lf;
        *self.cached_text.borrow_mut() = None;

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
        let mut first_hint = self.locate_char(start);
        while first_hint.idx < self.pieces.len()
            && first_hint.char_start + self.pieces[first_hint.idx].char_count <= start
        {
            let p = &self.pieces[first_hint.idx];
            first_hint.char_start += p.char_count;
            first_hint.byte_start += p.length;
            first_hint.nl_start += p.line_feed_cnt;
            first_hint.idx += 1;
        }
        let first = first_hint.idx;
        let first_start = first_hint.char_start;

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

        let (mut old_bytes, mut old_lfs, mut old_added_bytes) = (0usize, 0usize, 0usize);
        for p in &self.pieces[first..last] {
            old_bytes += p.length;
            old_lfs += p.line_feed_cnt;
            if p.buffer == BufferType::Added {
                old_added_bytes += p.length;
            }
        }
        let (mut new_bytes, mut new_lfs, mut new_added_bytes) = (0usize, 0usize, 0usize);
        for p in replacement.iter().flatten() {
            new_bytes += p.length;
            new_lfs += p.line_feed_cnt;
            if p.buffer == BufferType::Added {
                new_added_bytes += p.length;
            }
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

        self.hint.set(if merged {
            PieceHint::default()
        } else {
            first_hint
        });
        self.added_piece_bytes = self.added_piece_bytes - old_added_bytes + new_added_bytes;
        self.total_chars -= end - start;
        self.total_bytes = self.total_bytes - old_bytes + new_bytes;
        self.total_line_feeds = self.total_line_feeds - old_lfs + new_lfs;
        *self.cached_text.borrow_mut() = None;
    }

    /// Merges contiguous pieces that point to adjacent regions in the same buffer in a single O(M) pass.
    pub fn coalesce(&mut self) {
        if self.pieces.len() <= 1 {
            return;
        }
        self.hint.set(PieceHint::default());
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

    /// Safely accesses the flattened text via a closure, using a `RefCell`-backed
    /// lazy cache for multi-piece tables. For 0 or 1 piece the closure receives a
    /// direct borrow with zero allocation.
    pub fn with_str<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        match self.pieces.as_slice() {
            [] => f(""),
            [p] => f(p.get_text(&self.original, &self.added)),
            _ => {
                if self.cached_text.borrow().is_none() {
                    let mut s = String::with_capacity(self.total_bytes);
                    for p in self.pieces.iter() {
                        s.push_str(p.get_text(&self.original, &self.added));
                    }
                    *self.cached_text.borrow_mut() = Some(s);
                }
                let cache = self.cached_text.borrow();
                f(cache.as_deref().unwrap())
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

    /// Invokes `f` with each contiguous `&str` piece in order without flattening the buffer.
    pub fn for_each_chunk(&self, mut f: impl FnMut(&str)) {
        for p in self.pieces.iter() {
            f(p.get_text(&self.original, &self.added));
        }
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
        if char_idx >= self.total_chars || self.pieces.is_empty() {
            return self.total_bytes;
        }
        let h = self.locate_char(char_idx);
        let p = &self.pieces[h.idx];
        let offset_in_piece = char_idx - h.char_start;
        h.byte_start
            + p.char_to_byte_offset_indexed(
                &self.original,
                &self.added,
                &self.orig_checkpoints,
                offset_in_piece,
            )
    }

    /// Converts a byte offset to a character index in the document.
    ///
    /// Safe even when `byte_idx` falls inside a multi-byte UTF-8 character (floors to the
    /// enclosing character's start boundary).
    pub fn byte_to_char(&self, byte_idx: usize) -> usize {
        if byte_idx == 0 {
            return 0;
        }
        if byte_idx >= self.total_bytes || self.pieces.is_empty() {
            return self.total_chars;
        }
        #[inline]
        fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
            let bytes = s.as_bytes();
            idx = idx.min(bytes.len());
            while idx > 0 && idx < bytes.len() && (bytes[idx] & 0xC0) == 0x80 {
                idx -= 1;
            }
            idx
        }

        let h = self.locate_byte(byte_idx);
        let p = &self.pieces[h.idx];
        let text = p.get_text(&self.original, &self.added);
        let offset_in_piece = floor_char_boundary(text, byte_idx - h.byte_start);
        // Fast path: if char count equals byte length, all characters are single-byte ASCII
        if p.char_count == p.length {
            return h.char_start + offset_in_piece;
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
                    return h.char_start + (cp.char_offset - p.char_start) + rem_chars;
                }
            }
        }
        let chars_in_offset = text[..offset_in_piece].chars().count();
        h.char_start + chars_in_offset
    }

    /// Internal helper: returns `(char_idx, piece_idx, byte_offset_in_piece)` for the start of `target_line`.
    fn find_line_start_pos(&self, target_line: usize) -> (usize, usize, usize) {
        if target_line == 0 {
            return (0, 0, 0);
        }
        if target_line > self.total_line_feeds || self.pieces.is_empty() {
            return (self.total_chars, self.pieces.len(), 0);
        }

        let h = self.locate_line(target_line);
        let p_idx = h.idx;
        let p = &self.pieces[p_idx];
        let rel_line = target_line - h.nl_start;
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
            return (h.char_start + scan_char_offset, p_idx, scan_byte_start);
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
                    return (h.char_start + scan_char_offset, p_idx, byte_end);
                }
            }
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
        if target == 0 || self.pieces.is_empty() {
            return (0, 0);
        }

        let h = self.locate_char(target);
        let p = &self.pieces[h.idx];
        let mut curr_line = h.nl_start;
        let offset_chars = target - h.char_start;

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
                let last_line_start_char = if tail_lf > 0 {
                    let rel_nl = tail_bytes.iter().rposition(|&b| b == b'\n').unwrap() + 1;
                    let chars_in_tail = if p.char_count == p.length {
                        rel_nl
                    } else {
                        prefix[scan_start_byte..scan_start_byte + rel_nl]
                            .chars()
                            .count()
                    };
                    h.char_start + base_char_offset + chars_in_tail
                } else {
                    h.char_start + base_char_offset
                };
                return (curr_line, target - last_line_start_char);
            }
        }

        // No '\n' before `target` inside piece `h.idx`; walk backward to the nearest piece with a '\n'.
        let mut back_idx = h.idx;
        let mut back_char_start = h.char_start;
        let mut last_line_start_char = 0usize;
        while back_idx > 0 {
            back_idx -= 1;
            let prev = &self.pieces[back_idx];
            back_char_start -= prev.char_count;
            if prev.line_feed_cnt > 0 {
                let text = prev.get_text(&self.original, &self.added);
                if let Some(last_nl_byte) = text.as_bytes().iter().rposition(|&b| b == b'\n') {
                    let byte_after_nl = last_nl_byte + 1;
                    let chars_before_nl_end = if prev.char_count == prev.length {
                        byte_after_nl
                    } else {
                        prev.char_count - text[byte_after_nl..].chars().count()
                    };
                    last_line_start_char = back_char_start + chars_before_nl_end;
                }
                break;
            }
        }

        (curr_line, target - last_line_start_char)
    }

    /// Returns the character range `start..end` (excluding trailing `'\n'` or `"\r\n"`) of 0-based `line_idx`.
    pub fn line_char_range(&self, line_idx: usize) -> Range<usize> {
        if line_idx > self.total_line_feeds {
            return self.total_chars..self.total_chars;
        }
        let (start, start_piece_idx, start_byte_in_piece) = self.find_line_start_pos(line_idx);
        if line_idx == self.total_line_feeds {
            return start..self.total_chars.max(start);
        }

        let mut curr_char = start;
        let mut prev_last_byte: Option<u8> = None;
        for (idx, p) in self.pieces[start_piece_idx..].iter().enumerate() {
            let byte_offset = if idx == 0 { start_byte_in_piece } else { 0 };
            if byte_offset >= p.length {
                continue;
            }
            let text = p.get_text(&self.original, &self.added);
            let slice = &text[byte_offset..];
            if idx > 0 && p.line_feed_cnt == 0 {
                curr_char += p.char_count;
                if !slice.is_empty() {
                    prev_last_byte = slice.as_bytes().last().copied();
                }
                continue;
            }
            if let Some(nl_pos) = slice.as_bytes().iter().position(|&b| b == b'\n') {
                let chars_in_line = if p.char_count == p.length {
                    nl_pos
                } else {
                    slice[..nl_pos].chars().count()
                };
                let mut end = curr_char + chars_in_line;
                let preceded_by_cr = if nl_pos > 0 {
                    slice.as_bytes()[nl_pos - 1] == b'\r'
                } else {
                    prev_last_byte == Some(b'\r')
                };
                if preceded_by_cr && end > start {
                    end -= 1;
                }
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
                if !slice.is_empty() {
                    prev_last_byte = slice.as_bytes().last().copied();
                }
            }
        }

        start..self.total_chars.max(start)
    }

    /// Extracts characters in `char_range` into `out` (cleared first) without flattening the full buffer.
    pub fn extract_char_range(&self, char_range: Range<usize>, out: &mut String) {
        out.clear();
        let start = char_range.start.min(self.total_chars);
        let end = char_range.end.min(self.total_chars);
        if start >= end || self.pieces.is_empty() {
            return;
        }

        let h = self.locate_char(start);
        let start_idx = h.idx;
        let mut curr_char = h.char_start;
        for p in &self.pieces[start_idx..] {
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

    /// Searches for `needle` starting at byte offset `start_byte` without flattening
    /// the piece table into `cached_text`. Returns the matching byte offset if found.
    ///
    /// Safe even when `start_byte` is not aligned to a UTF-8 character boundary (advances past
    /// any leading UTF-8 continuation bytes, since a valid UTF-8 `needle` can never begin there).
    pub fn find_from_byte(&self, needle: &str, start_byte: usize) -> Option<usize> {
        self.find_from_byte_impl(needle, start_byte, true)
    }

    fn find_from_byte_impl(
        &self,
        needle: &str,
        start_byte: usize,
        case_sensitive: bool,
    ) -> Option<usize> {
        if needle.is_empty() || needle.len() > self.total_bytes || start_byte >= self.total_bytes {
            return None;
        }
        #[inline]
        fn align_char_boundary(s: &str, mut byte_idx: usize) -> usize {
            let bytes = s.as_bytes();
            while byte_idx < bytes.len() && (bytes[byte_idx] & 0xC0) == 0x80 {
                byte_idx += 1;
            }
            byte_idx
        }
        #[inline]
        fn bytes_match(a: &[u8], b: &[u8], case_sensitive: bool) -> bool {
            if case_sensitive {
                a == b
            } else {
                a.eq_ignore_ascii_case(b)
            }
        }

        let needle_bytes = needle.as_bytes();
        let needle_len = needle_bytes.len();

        if let [p] = self.pieces.as_slice() {
            let text = p.get_text(&self.original, &self.added);
            let aligned = align_char_boundary(text, start_byte);
            return if case_sensitive {
                text[aligned..].find(needle).map(|rel| aligned + rel)
            } else {
                text.as_bytes()[aligned..]
                    .windows(needle_len)
                    .position(|w| w.eq_ignore_ascii_case(needle_bytes))
                    .map(|rel| aligned + rel)
            };
        }

        let start_hint = self.locate_byte(start_byte);
        let mut piece_byte_start = start_hint.byte_start;

        if needle_len == 1 {
            let target = needle_bytes[0];
            for p in &self.pieces[start_hint.idx..] {
                let piece_end = piece_byte_start + p.length;
                if piece_end > start_byte {
                    let local_start = start_byte.saturating_sub(piece_byte_start);
                    let bytes = &p.get_text(&self.original, &self.added).as_bytes()[local_start..];
                    let pos = if case_sensitive {
                        bytes.iter().position(|&b| b == target)
                    } else {
                        bytes.iter().position(|&b| b.eq_ignore_ascii_case(&target))
                    };
                    if let Some(rel) = pos {
                        return Some(piece_byte_start + local_start + rel);
                    }
                }
                piece_byte_start = piece_end;
            }
            return None;
        }

        // Keep at most `needle_len - 1` trailing bytes from preceding slices to catch
        // matches that straddle piece boundaries.
        let max_tail = needle_len - 1;
        let mut tail: Vec<u8> = Vec::with_capacity(max_tail * 2);

        for p in &self.pieces[start_hint.idx..] {
            let piece_end = piece_byte_start + p.length;
            if piece_end <= start_byte {
                piece_byte_start = piece_end;
                continue;
            }
            let full_text = p.get_text(&self.original, &self.added);
            let local_start =
                align_char_boundary(full_text, start_byte.saturating_sub(piece_byte_start));
            let slice_start_byte = piece_byte_start + local_start;
            let slice = &full_text[local_start..];
            let slice_bytes = slice.as_bytes();

            if !tail.is_empty() {
                let tail_len = tail.len();
                let take_prefix = slice_bytes.len().min(max_tail);
                tail.extend_from_slice(&slice_bytes[..take_prefix]);
                if let Some(rel) = tail
                    .windows(needle_len)
                    .position(|w| bytes_match(w, needle_bytes, case_sensitive))
                {
                    return Some(slice_start_byte - tail_len + rel);
                }
                tail.truncate(tail_len);
            }

            let in_slice = if case_sensitive {
                slice.find(needle)
            } else {
                slice_bytes
                    .windows(needle_len)
                    .position(|w| w.eq_ignore_ascii_case(needle_bytes))
            };
            if let Some(rel) = in_slice {
                return Some(slice_start_byte + rel);
            }

            if slice_bytes.len() >= max_tail {
                tail.clear();
                tail.extend_from_slice(&slice_bytes[slice_bytes.len() - max_tail..]);
            } else {
                let combined_len = tail.len() + slice_bytes.len();
                if combined_len > max_tail {
                    tail.drain(..combined_len - max_tail);
                }
                tail.extend_from_slice(slice_bytes);
            }
            piece_byte_start = piece_end;
        }

        None
    }

    /// Searches backwards for the last occurrence of `needle` whose match starts strictly before
    /// `limit_byte`, without flattening the piece table into `cached_text`.
    pub fn find_prev_byte(&self, needle: &str, limit_byte: usize) -> Option<usize> {
        self.find_prev_byte_impl(needle, limit_byte, true)
    }

    fn find_prev_byte_impl(
        &self,
        needle: &str,
        limit_byte: usize,
        case_sensitive: bool,
    ) -> Option<usize> {
        if needle.is_empty() || needle.len() > self.total_bytes || limit_byte == 0 {
            return None;
        }
        #[inline]
        fn bytes_match(a: &[u8], b: &[u8], case_sensitive: bool) -> bool {
            if case_sensitive {
                a == b
            } else {
                a.eq_ignore_ascii_case(b)
            }
        }

        let needle_bytes = needle.as_bytes();
        let needle_len = needle_bytes.len();
        let clamped_limit = limit_byte.min(self.total_bytes);
        let search_end = (clamped_limit - 1)
            .saturating_add(needle_len)
            .min(self.total_bytes);
        if search_end < needle_len {
            return None;
        }

        if let [p] = self.pieces.as_slice() {
            let text = p.get_text(&self.original, &self.added);
            let bytes = &text.as_bytes()[..search_end];
            return if case_sensitive {
                if text.is_char_boundary(search_end) {
                    text[..search_end].rfind(needle)
                } else {
                    bytes.windows(needle_len).rposition(|w| w == needle_bytes)
                }
            } else {
                bytes
                    .windows(needle_len)
                    .rposition(|w| w.eq_ignore_ascii_case(needle_bytes))
            };
        }

        let end_hint = self.locate_byte(search_end);
        let mut idx = end_hint.idx;
        let mut piece_byte_start = end_hint.byte_start;
        let max_head = needle_len - 1;
        let mut head: Vec<u8> = Vec::with_capacity(max_head * 2);
        let mut combined: Vec<u8> = Vec::with_capacity(max_head * 2);

        loop {
            let p = &self.pieces[idx];
            let full_text = p.get_text(&self.original, &self.added);
            let local_end = search_end.saturating_sub(piece_byte_start).min(p.length);
            let slice_bytes = &full_text.as_bytes()[..local_end];
            let slice_end_byte = piece_byte_start + local_end;

            if !slice_bytes.is_empty() {
                if needle_len == 1 {
                    let target = needle_bytes[0];
                    let pos = if case_sensitive {
                        slice_bytes.iter().rposition(|&b| b == target)
                    } else {
                        slice_bytes
                            .iter()
                            .rposition(|&b| b.eq_ignore_ascii_case(&target))
                    };
                    if let Some(rel) = pos {
                        return Some(piece_byte_start + rel);
                    }
                } else {
                    if !head.is_empty() {
                        let suffix_len = slice_bytes.len().min(max_head);
                        combined.clear();
                        combined
                            .extend_from_slice(&slice_bytes[slice_bytes.len() - suffix_len..]);
                        combined.extend_from_slice(&head);
                        if let Some(rel) = combined
                            .windows(needle_len)
                            .rposition(|w| bytes_match(w, needle_bytes, case_sensitive))
                        {
                            return Some(slice_end_byte - suffix_len + rel);
                        }
                    }

                    let in_slice = if case_sensitive && full_text.is_char_boundary(local_end) {
                        full_text[..local_end].rfind(needle)
                    } else {
                        slice_bytes
                            .windows(needle_len)
                            .rposition(|w| bytes_match(w, needle_bytes, case_sensitive))
                    };
                    if let Some(rel) = in_slice {
                        return Some(piece_byte_start + rel);
                    }

                    if slice_bytes.len() >= max_head {
                        head.clear();
                        head.extend_from_slice(&slice_bytes[..max_head]);
                    } else {
                        let keep_old = head.len().min(max_head - slice_bytes.len());
                        head.truncate(keep_old);
                        head.splice(0..0, slice_bytes.iter().copied());
                    }
                }
            }

            if idx == 0 {
                break;
            }
            idx -= 1;
            piece_byte_start -= self.pieces[idx].length;
        }

        None
    }

    /// Searches for `needle` starting from `start_char` (forward when `!backwards`, or finding the
    /// last match starting strictly before `start_char` when `backwards`), supporting both
    /// case-sensitive and case-insensitive matching. Returns the matched character range.
    pub fn find_char_range(
        &self,
        needle: &str,
        start_char: usize,
        case_sensitive: bool,
        backwards: bool,
    ) -> Option<Range<usize>> {
        if needle.is_empty() || self.is_empty() {
            return None;
        }
        if case_sensitive || needle.is_ascii() {
            let needle_chars = needle.chars().count();
            let byte_pos = if backwards {
                let limit_byte = self.char_to_byte(start_char.min(self.total_chars));
                self.find_prev_byte_impl(needle, limit_byte, case_sensitive)?
            } else {
                let start_byte = self.char_to_byte(start_char.min(self.total_chars));
                self.find_from_byte_impl(needle, start_byte, case_sensitive)?
            };
            let match_start = self.byte_to_char(byte_pos);
            return Some(match_start..match_start + needle_chars);
        }

        // Non-ASCII case-insensitive search: compare Unicode lowercased char sequences so
        // multi-codepoint case folds (e.g. 'İ') produce accurate original char ranges.
        let needle_lower: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
        if needle_lower.is_empty() {
            return None;
        }
        self.with_str(|s| {
            let mut lower_chars: Vec<char> = Vec::with_capacity(s.len());
            let mut orig_char_idx: Vec<usize> = Vec::with_capacity(s.len());
            for (ci, ch) in s.chars().enumerate() {
                for lc in ch.to_lowercase() {
                    lower_chars.push(lc);
                    orig_char_idx.push(ci);
                }
            }
            let nlen = needle_lower.len();
            if lower_chars.len() < nlen {
                return None;
            }
            if backwards {
                let limit = start_char.min(self.total_chars);
                let mut pos = lower_chars.len() - nlen;
                loop {
                    let orig_start = orig_char_idx[pos];
                    let at_orig_boundary = pos == 0 || orig_char_idx[pos - 1] != orig_start;
                    let end_at_orig_boundary = pos + nlen == lower_chars.len()
                        || orig_char_idx[pos + nlen] != orig_char_idx[pos + nlen - 1];
                    if orig_start < limit
                        && at_orig_boundary
                        && end_at_orig_boundary
                        && lower_chars[pos..pos + nlen] == needle_lower[..]
                    {
                        let orig_end = orig_char_idx[pos + nlen - 1] + 1;
                        return Some(orig_start..orig_end);
                    }
                    if pos == 0 {
                        break;
                    }
                    pos -= 1;
                }
                None
            } else {
                for pos in 0..=lower_chars.len() - nlen {
                    let orig_start = orig_char_idx[pos];
                    if orig_start < start_char {
                        continue;
                    }
                    let at_orig_boundary = pos == 0 || orig_char_idx[pos - 1] != orig_start;
                    let end_at_orig_boundary = pos + nlen == lower_chars.len()
                        || orig_char_idx[pos + nlen] != orig_char_idx[pos + nlen - 1];
                    if at_orig_boundary
                        && end_at_orig_boundary
                        && lower_chars[pos..pos + nlen] == needle_lower[..]
                    {
                        let orig_end = orig_char_idx[pos + nlen - 1] + 1;
                        return Some(orig_start..orig_end);
                    }
                }
                None
            }
        })
    }
}

impl fmt::Debug for PieceTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PieceTable")
            .field("total_bytes", &self.total_bytes)
            .field("total_chars", &self.total_chars)
            .field("pieces_count", &self.pieces.len())
            .field("text", &self.build_string())
            .finish()
    }
}

impl PartialEq for PieceTable {
    fn eq(&self, other: &Self) -> bool {
        if self.total_bytes != other.total_bytes || self.total_chars != other.total_chars {
            return false;
        }
        let mut iter_a = self
            .pieces
            .iter()
            .map(|p| p.get_text(&self.original, &self.added).as_bytes());
        let mut iter_b = other
            .pieces
            .iter()
            .map(|p| p.get_text(&other.original, &other.added).as_bytes());
        let mut slice_a: &[u8] = &[];
        let mut slice_b: &[u8] = &[];
        loop {
            while slice_a.is_empty() {
                match iter_a.next() {
                    Some(next) => slice_a = next,
                    None => return iter_b.all(|s| s.is_empty()) && slice_b.is_empty(),
                }
            }
            while slice_b.is_empty() {
                match iter_b.next() {
                    Some(next) => slice_b = next,
                    None => return false,
                }
            }
            let n = slice_a.len().min(slice_b.len());
            if slice_a[..n] != slice_b[..n] {
                return false;
            }
            slice_a = &slice_a[n..];
            slice_b = &slice_b[n..];
        }
    }
}

impl Eq for PieceTable {}

impl PartialEq<str> for PieceTable {
    fn eq(&self, other: &str) -> bool {
        self.total_bytes == other.len() && self.eq_str(other)
    }
}

impl PartialEq<&str> for PieceTable {
    fn eq(&self, other: &&str) -> bool {
        self.total_bytes == other.len() && self.eq_str(other)
    }
}

impl PartialEq<String> for PieceTable {
    fn eq(&self, other: &String) -> bool {
        self.total_bytes == other.len() && self.eq_str(other.as_str())
    }
}

impl PartialEq<PieceTable> for str {
    fn eq(&self, other: &PieceTable) -> bool {
        self.len() == other.total_bytes && other.eq_str(self)
    }
}

impl PartialEq<PieceTable> for &str {
    fn eq(&self, other: &PieceTable) -> bool {
        self.len() == other.total_bytes && other.eq_str(self)
    }
}

impl PartialEq<PieceTable> for String {
    fn eq(&self, other: &PieceTable) -> bool {
        self.len() == other.total_bytes && other.eq_str(self.as_str())
    }
}

impl std::str::FromStr for PieceTable {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_str(s))
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

// Inherent methods replacing the former `egui::TextBuffer` impl.
// `EditorBuffer` in editor.rs provides the trait abstraction.

impl PieceTable {
    /// Inserts `text` at `char_index`, returning the number of characters inserted.
    pub fn insert_text(&mut self, text: &str, char_index: usize) -> usize {
        self.insert(char_index, text)
    }

    /// Deletes the character range `start..end`.
    pub fn delete_char_range(&mut self, range: Range<usize>) {
        self.delete(range);
    }

    /// Replaces the entire buffer contents with `text`.
    pub fn replace_with(&mut self, text: &str) {
        let byte_len = text.len();
        let (char_count, line_feed_cnt, checkpoints) = scan_str_with_checkpoints(text);

        self.original = Arc::<str>::from(text);
        self.orig_checkpoints = Arc::from(checkpoints);
        self.added.clear();
        self.added_epoch = self.added_epoch.wrapping_add(1);
        self.added_chars = 0;
        self.added_lfs = 0;
        self.added_piece_bytes = 0;
        self.hint.set(PieceHint::default());
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
        *self.cached_text.borrow_mut() = None;
    }

    /// Takes the buffer contents as an owned `String`, leaving the table empty.
    pub fn take(&mut self) -> String {
        let s = self.build_string();
        self.clear();
        s
    }
}
