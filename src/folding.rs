//! Simple code folding by matching `{ }` braces and indentation level.
//!
//! Computes foldable line ranges in a single O(N) pass over the buffer, tracks collapsed
//! fold headers, and provides a lightweight coordinate projection (`FoldView`) mapping
//! between real document character offsets and visible display character offsets.

use crate::editor::EditorBuffer;
use eframe::egui::epaint::text::Galley;
use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
};

/// Tab stop width (in columns) used when measuring leading indentation.
const TAB_WIDTH: usize = 4;

/// A foldable region of 0-based lines in the document.
///
/// When folded, `start_line` remains visible (with an inline `...` indicator)
/// and lines `(start_line + 1)..=end_line` are hidden. Always satisfies `start_line < end_line`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRange {
    pub start_line: usize,
    pub end_line: usize,
}

/// Computes leading visual indentation for `line`, or `None` if the line is blank (whitespace-only).
fn line_indent(line: &str) -> Option<usize> {
    let mut indent = 0usize;
    for b in line.bytes() {
        match b {
            b' ' => indent += 1,
            b'\t' => indent = (indent / TAB_WIDTH + 1) * TAB_WIDTH,
            b'\r' => {}
            _ => return Some(indent),
        }
    }
    None
}

/// Returns `true` for file extensions where `#` starts a line comment.
pub fn extension_uses_hash_comments(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "py" | "toml" | "yaml" | "yml" | "sh" | "bash" | "zsh" | "rb" | "r"
    )
}

/// Streaming fold-range builder that scans lines sequentially without requiring a flattened buffer.
struct FoldScanner {
    line_idx: usize,
    block_comment_depth: usize,
    in_double_quote: bool,
    raw_string_hashes: Option<usize>,
    hash_comments: bool,
    brace_stack: Vec<usize>,
    brace_folds: HashMap<usize, usize>,
    indent_stack: Vec<(usize, usize)>,
    indent_folds: HashMap<usize, usize>,
    last_non_blank: usize,
}

impl FoldScanner {
    fn new(hash_comments: bool) -> Self {
        Self {
            line_idx: 0,
            block_comment_depth: 0,
            in_double_quote: false,
            raw_string_hashes: None,
            hash_comments,
            brace_stack: Vec::new(),
            brace_folds: HashMap::new(),
            indent_stack: Vec::new(),
            indent_folds: HashMap::new(),
            last_non_blank: 0,
        }
    }

    fn scan_line(&mut self, raw_line: &str) {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let line_idx = self.line_idx;
        self.line_idx += 1;

        // --- Indentation-level tracking (monotonic stack) ---
        if let Some(ind) = line_indent(line) {
            while let Some(&(top_line, top_ind)) = self.indent_stack.last() {
                if top_ind >= ind {
                    self.indent_stack.pop();
                    if self.last_non_blank > top_line {
                        self.indent_folds.insert(top_line, self.last_non_blank);
                    }
                } else {
                    break;
                }
            }
            self.indent_stack.push((line_idx, ind));
            self.last_non_blank = line_idx;
        }

        // --- Matching `{ }` brace tracking ---
        let bytes = line.as_bytes();
        let trimmed_starts_with_close = line.trim_start().starts_with('}');
        let mut i = 0usize;

        while i < bytes.len() {
            if self.block_comment_depth > 0 {
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    self.block_comment_depth += 1;
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    self.block_comment_depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            if let Some(hashes) = self.raw_string_hashes {
                if bytes[i] == b'"'
                    && i + hashes < bytes.len()
                    && bytes[i + 1..=i + hashes].iter().all(|&b| b == b'#')
                {
                    self.raw_string_hashes = None;
                    i += 1 + hashes;
                } else {
                    i += 1;
                }
                continue;
            }

            if self.in_double_quote {
                if bytes[i] == b'\\' {
                    i += 2;
                } else if bytes[i] == b'"' {
                    self.in_double_quote = false;
                    i += 1;
                } else {
                    i += 1;
                }
                continue;
            }

            match bytes[i] {
                b'#' if self.hash_comments => {
                    // Hash line comment (Python, TOML, YAML, Shell, Ruby, R)
                    break;
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                    // Line comment: ignore rest of line
                    break;
                }
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                    self.block_comment_depth += 1;
                    i += 2;
                }
                b'r' if !self.hash_comments => {
                    // Detect Rust raw string literals: `r"..."`, `r#"..."#`, `br#"..."#`, etc.
                    let valid_prefix = if i == 0 {
                        true
                    } else {
                        let prev = bytes[i - 1];
                        if prev == b'b' || prev == b'c' {
                            i == 1
                                || !(bytes[i - 2].is_ascii_alphanumeric() || bytes[i - 2] == b'_')
                        } else {
                            !(prev.is_ascii_alphanumeric() || prev == b'_')
                        }
                    };
                    if valid_prefix {
                        let mut j = i + 1;
                        while j < bytes.len() && bytes[j] == b'#' {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b'"' {
                            self.raw_string_hashes = Some(j - (i + 1));
                            i = j + 1;
                            continue;
                        }
                    }
                    i += 1;
                }
                b'"' => {
                    self.in_double_quote = true;
                    i += 1;
                }
                b'\'' => {
                    // Skip simple char literals like '{', '}', '\'', '\n' without breaking on
                    // Rust lifetimes such as `'a` or `'static`.
                    if i + 3 < bytes.len() && bytes[i + 1] == b'\\' && bytes[i + 3] == b'\'' {
                        i += 4;
                    } else if i + 2 < bytes.len()
                        && bytes[i + 1] != b'\''
                        && bytes[i + 1] != b'\\'
                        && bytes[i + 2] == b'\''
                    {
                        i += 3;
                    } else {
                        i += 1;
                    }
                }
                b'{' => {
                    self.brace_stack.push(line_idx);
                    i += 1;
                }
                b'}' => {
                    if let Some(start_line) = self.brace_stack.pop() {
                        if line_idx > start_line {
                            let last_hidden = if trimmed_starts_with_close {
                                line_idx - 1
                            } else {
                                line_idx
                            };
                            if last_hidden > start_line {
                                self.brace_folds
                                    .entry(start_line)
                                    .and_modify(|e| *e = (*e).max(last_hidden))
                                    .or_insert(last_hidden);
                            }
                        }
                    }
                    i += 1;
                }
                _ => {
                    i += 1;
                }
            }
        }
    }

    fn finish(mut self) -> Vec<FoldRange> {
        while let Some((top_line, _)) = self.indent_stack.pop() {
            if self.last_non_blank > top_line {
                self.indent_folds.insert(top_line, self.last_non_blank);
            }
        }

        // Merge brace folds and indentation folds; brace folds take precedence on the same start_line.
        let mut merged = self.indent_folds;
        for (start_line, end_line) in self.brace_folds {
            merged.insert(start_line, end_line);
        }

        let mut ranges: Vec<FoldRange> = merged
            .into_iter()
            .filter(|&(start, end)| end > start)
            .map(|(start_line, end_line)| FoldRange {
                start_line,
                end_line,
            })
            .collect();
        ranges.sort_unstable_by_key(|r| r.start_line);

        // Enforce laminar (non-crossing) nesting so `visible_lines`, `is_line_hidden`, and
        // `outermost_visible_ancestor` always agree even when indentation and brace folds diverge.
        let mut outer_ends: Vec<usize> = Vec::new();
        let mut normalized: Vec<FoldRange> = Vec::with_capacity(ranges.len());
        for mut r in ranges {
            while let Some(&outer_end) = outer_ends.last() {
                if r.start_line > outer_end {
                    outer_ends.pop();
                } else {
                    break;
                }
            }
            if let Some(&outer_end) = outer_ends.last() {
                if r.end_line > outer_end {
                    r.end_line = outer_end;
                }
            }
            if r.end_line > r.start_line {
                outer_ends.push(r.end_line);
                normalized.push(r);
            }
        }
        normalized
    }
}

/// Computes all foldable line ranges in `text` by matching `{ }` braces and indentation levels.
///
/// Returned ranges are sorted by `start_line` with unique `start_line` values.
pub fn compute_fold_ranges(text: &str) -> Vec<FoldRange> {
    compute_fold_ranges_with_options(text, false)
}

/// Like [`compute_fold_ranges`], optionally ignoring `#` line comments (for Python, TOML, YAML, Shell, etc.).
pub fn compute_fold_ranges_with_options(text: &str, hash_comments: bool) -> Vec<FoldRange> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut scanner = FoldScanner::new(hash_comments);
    for line in text.split('\n') {
        scanner.scan_line(line);
    }
    scanner.finish()
}

/// Computes foldable line ranges by streaming chunks from an `EditorBuffer` without flattening it.
pub fn compute_fold_ranges_buffer<T: EditorBuffer>(buffer: &T) -> Vec<FoldRange> {
    compute_fold_ranges_buffer_with_options(buffer, false)
}

/// Like [`compute_fold_ranges_buffer`], optionally ignoring `#` line comments.
pub fn compute_fold_ranges_buffer_with_options<T: EditorBuffer>(
    buffer: &T,
    hash_comments: bool,
) -> Vec<FoldRange> {
    let mut scanner = FoldScanner::new(hash_comments);
    let mut carry = String::new();
    let mut saw_any = false;

    buffer.for_each_chunk(|chunk| {
        if chunk.is_empty() {
            return;
        }
        saw_any = true;
        let mut rest = chunk;
        while let Some(nl_pos) = rest.find('\n') {
            let piece = &rest[..nl_pos];
            if carry.is_empty() {
                scanner.scan_line(piece);
            } else {
                carry.push_str(piece);
                scanner.scan_line(&carry);
                carry.clear();
            }
            rest = &rest[nl_pos + 1..];
        }
        if !rest.is_empty() {
            carry.push_str(rest);
        }
    });

    if !saw_any {
        return Vec::new();
    }
    // Scan the final line (even if empty after a trailing newline).
    scanner.scan_line(&carry);
    scanner.finish()
}

/// Persistent folding state for an open document.
#[derive(Clone, Debug, Default)]
pub struct FoldState {
    cached_revision: Option<u64>,
    hash_comments: bool,
    ranges: Vec<FoldRange>,
    folded: BTreeSet<usize>,
    fold_version: u64,
    cached_visible: Option<(u64, usize, Vec<usize>)>,
}

impl FoldState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures whether `#` starts a line comment when scanning `{ }` braces.
    pub fn set_hash_comments(&mut self, enabled: bool) {
        if self.hash_comments != enabled {
            self.hash_comments = enabled;
            self.cached_revision = None;
        }
    }

    /// Monotonic version counter bumped whenever active folds or fold ranges affecting visibility change.
    pub fn fold_version(&self) -> u64 {
        self.fold_version
    }

    /// Returns `true` if at least one fold is currently collapsed.
    pub fn has_active_folds(&self) -> bool {
        !self.folded.is_empty()
    }

    /// All computed foldable ranges sorted by `start_line`.
    pub fn ranges(&self) -> &[FoldRange] {
        &self.ranges
    }

    /// Set of currently collapsed 0-based `start_line`s.
    pub fn folded_lines(&self) -> &BTreeSet<usize> {
        &self.folded
    }

    /// Replaces the collapsed fold set (e.g. when restoring an unsaved file's state).
    pub fn set_folded_lines(&mut self, folded: BTreeSet<usize>) {
        self.folded = folded;
        self.cached_revision = None;
        self.cached_visible = None;
        self.fold_version = self.fold_version.wrapping_add(1);
    }

    /// Clears all fold state and cached ranges (e.g. when opening or closing a file).
    pub fn clear(&mut self) {
        self.cached_revision = None;
        self.ranges.clear();
        self.folded.clear();
        self.cached_visible = None;
        self.fold_version = self.fold_version.wrapping_add(1);
    }

    /// Recomputes foldable ranges from `text` if `text_revision` has changed.
    pub fn ensure_updated_str(&mut self, text: &str, text_revision: u64) {
        if self.cached_revision == Some(text_revision) {
            return;
        }
        let ranges = compute_fold_ranges_with_options(text, self.hash_comments);
        self.apply_ranges(ranges, Some(text_revision));
    }

    /// Recomputes foldable ranges from an `EditorBuffer` if `text_revision` has changed.
    pub fn ensure_updated_buffer<T: EditorBuffer>(&mut self, buffer: &T, text_revision: Option<u64>) {
        if let Some(rev) = text_revision {
            if self.cached_revision == Some(rev) {
                return;
            }
        }
        let ranges = compute_fold_ranges_buffer_with_options(buffer, self.hash_comments);
        self.apply_ranges(ranges, text_revision);
    }

    /// Records the cached text revision without rescanning when ranges were already computed
    /// for the current buffer contents.
    pub fn set_cached_revision(&mut self, text_revision: u64) {
        self.cached_revision = Some(text_revision);
    }

    fn apply_ranges(&mut self, new_ranges: Vec<FoldRange>, rev: Option<u64>) {
        let had_folds = !self.folded.is_empty();
        let ranges_changed = self.ranges != new_ranges;
        self.ranges = new_ranges;
        self.cached_revision = rev;

        if had_folds {
            let prev_len = self.folded.len();
            self.folded.retain(|line| {
                self.ranges
                    .binary_search_by_key(line, |r| r.start_line)
                    .is_ok()
            });
            if self.folded.len() != prev_len || ranges_changed {
                self.fold_version = self.fold_version.wrapping_add(1);
                self.cached_visible = None;
            }
        }
    }

    /// Looks up the foldable range starting on `start_line`.
    pub fn fold_range_at(&self, start_line: usize) -> Option<FoldRange> {
        let idx = self
            .ranges
            .binary_search_by_key(&start_line, |r| r.start_line)
            .ok()?;
        Some(self.ranges[idx])
    }

    /// Returns `true` if `start_line` is currently collapsed.
    pub fn is_folded(&self, start_line: usize) -> bool {
        self.folded.contains(&start_line)
    }

    /// Returns the number of lines hidden immediately after `doc_line` (0 if not folded).
    pub fn hidden_line_count_after(&self, doc_line: usize) -> usize {
        if !self.folded.contains(&doc_line) {
            return 0;
        }
        self.fold_range_at(doc_line)
            .map(|r| r.end_line.saturating_sub(r.start_line))
            .unwrap_or(0)
    }

    /// Returns `true` if `line` is hidden inside any currently collapsed fold.
    pub fn is_line_hidden(&self, line: usize) -> bool {
        self.outermost_visible_ancestor(line) != line
    }

    /// If `line` is hidden inside one or more collapsed folds, returns the outermost collapsed
    /// header line containing it; otherwise returns `line`.
    pub fn outermost_visible_ancestor(&self, line: usize) -> usize {
        if self.folded.is_empty() {
            return line;
        }
        let mut hidden_until: Option<usize> = None;
        for &start in self.folded.range(..line) {
            if let Some(end) = hidden_until {
                if start <= end {
                    continue;
                }
            }
            if let Some(r) = self.fold_range_at(start) {
                if line <= r.end_line {
                    return start;
                }
                hidden_until = Some(r.end_line);
            }
        }
        line
    }

    /// Collapses the fold starting at `start_line` if it is foldable and not already folded.
    pub fn fold_line(&mut self, start_line: usize) -> bool {
        if self.fold_range_at(start_line).is_some() && self.folded.insert(start_line) {
            self.fold_version = self.fold_version.wrapping_add(1);
            self.cached_visible = None;
            return true;
        }
        false
    }

    /// Expands the fold starting at `start_line` if currently folded.
    pub fn unfold_line(&mut self, start_line: usize) -> bool {
        if self.folded.remove(&start_line) {
            self.fold_version = self.fold_version.wrapping_add(1);
            self.cached_visible = None;
            return true;
        }
        false
    }

    /// Toggles the fold starting at `start_line`. Returns `true` if state changed.
    pub fn toggle_fold(&mut self, start_line: usize) -> bool {
        if self.folded.contains(&start_line) {
            self.unfold_line(start_line)
        } else {
            self.fold_line(start_line)
        }
    }

    /// Folds the region starting on `line`, or the innermost unfolded enclosing region
    /// containing `line` (stepping outward if `line` is already folded).
    /// Returns the folded `start_line` if a new fold was collapsed.
    pub fn fold_at_or_enclosing(&mut self, line: usize) -> Option<usize> {
        if self.fold_range_at(line).is_some() && !self.folded.contains(&line) {
            self.fold_line(line);
            return Some(line);
        }

        // Prefer innermost unfolded range strictly covering `line` (`start <= line <= end`),
        // falling back to one whose closing line is `line` (`line == end + 1`).
        let mut strict_match = None;
        let mut closing_match = None;
        for r in &self.ranges {
            if r.start_line > line {
                break;
            }
            if self.folded.contains(&r.start_line) {
                continue;
            }
            if line <= r.end_line {
                strict_match = Some(r.start_line);
            } else if line == r.end_line + 1 {
                closing_match = Some(r.start_line);
            }
        }

        let target = strict_match.or(closing_match)?;
        if self.fold_line(target) {
            Some(target)
        } else {
            None
        }
    }

    /// Unfolds the fold starting on `line`, or the innermost folded region containing `line`
    /// (or immediately preceding `line` when `line` is the closing brace line).
    pub fn unfold_at_or_enclosing(&mut self, line: usize) -> Option<usize> {
        if self.folded.contains(&line) {
            self.unfold_line(line);
            return Some(line);
        }

        let mut strict_match = None;
        let mut closing_match = None;
        for &start in self.folded.range(..=line) {
            if let Some(r) = self.fold_range_at(start) {
                if line <= r.end_line {
                    strict_match = Some(start);
                } else if line == r.end_line + 1 {
                    closing_match = Some(start);
                }
            }
        }

        let target = strict_match.or(closing_match)?;
        if self.unfold_line(target) {
            Some(target)
        } else {
            None
        }
    }

    /// Collapses all foldable regions in the document. Returns `true` if any fold changed.
    pub fn fold_all(&mut self) -> bool {
        let mut changed = false;
        for r in &self.ranges {
            if self.folded.insert(r.start_line) {
                changed = true;
            }
        }
        if changed {
            self.fold_version = self.fold_version.wrapping_add(1);
            self.cached_visible = None;
        }
        changed
    }

    /// Expands all collapsed folds. Returns `true` if any fold changed.
    pub fn unfold_all(&mut self) -> bool {
        if self.folded.is_empty() {
            return false;
        }
        self.folded.clear();
        self.fold_version = self.fold_version.wrapping_add(1);
        self.cached_visible = None;
        true
    }

    /// Unfolds every currently collapsed fold whose hidden lines include `line`.
    /// Returns `true` if any fold was expanded.
    pub fn unfold_containing(&mut self, line: usize) -> bool {
        if self.folded.is_empty() {
            return false;
        }
        let to_unfold: Vec<usize> = self
            .folded
            .range(..line)
            .copied()
            .filter(|&start| {
                self.fold_range_at(start)
                    .is_some_and(|r| line <= r.end_line)
            })
            .collect();
        if to_unfold.is_empty() {
            return false;
        }
        for start in to_unfold {
            self.folded.remove(&start);
        }
        self.fold_version = self.fold_version.wrapping_add(1);
        self.cached_visible = None;
        true
    }

    /// Updates collapsed fold start lines after an edit touching `edit_start_line..=edit_end_line`
    /// that changed the document line count by `line_delta`.
    pub fn adjust_for_edit(
        &mut self,
        edit_start_line: usize,
        edit_end_line: usize,
        line_delta: isize,
    ) {
        self.adjust_for_edit_with_col(edit_start_line, usize::MAX, edit_end_line, line_delta);
    }

    /// Like [`Self::adjust_for_edit`], also taking the 0-based column where the edit began on
    /// `edit_start_line` so inserting newlines before column 0 of a folded header shifts it down
    /// and merging a folded header upward shifts it with its line.
    pub fn adjust_for_edit_with_col(
        &mut self,
        edit_start_line: usize,
        edit_start_col: usize,
        edit_end_line: usize,
        line_delta: isize,
    ) {
        self.cached_revision = None;
        if self.folded.is_empty() {
            return;
        }

        let mut next_folded = BTreeSet::new();
        for &start in &self.folded {
            if let Some(r) = self.fold_range_at(start) {
                // Hidden range is (r.start_line + 1)..=r.end_line.
                // If the edit overlaps the hidden lines, unfold this region.
                let hidden_start = r.start_line + 1;
                let hidden_end = r.end_line;
                if edit_start_line <= hidden_end && edit_end_line >= hidden_start {
                    continue;
                }
            }

            let should_shift = start > edit_end_line
                || (start == edit_start_line
                    && start == edit_end_line
                    && line_delta > 0
                    && edit_start_col == 0)
                || (start == edit_end_line && edit_start_line < start && line_delta < 0);

            let shifted = if should_shift {
                let s = start as isize + line_delta;
                if s < 0 {
                    continue;
                }
                s as usize
            } else {
                start
            };
            next_folded.insert(shifted);
        }

        self.folded = next_folded;
        self.cached_visible = None;
        self.fold_version = self.fold_version.wrapping_add(1);
    }

    /// Ensures the cached visible-lines vector is up to date for `total_lines`.
    pub fn ensure_visible_lines(&mut self, total_lines: usize) {
        let total = total_lines.max(1);
        let stale = match &self.cached_visible {
            Some((ver, cached_total, _)) => *ver != self.fold_version || *cached_total != total,
            None => true,
        };
        if stale {
            let mut vis = Vec::with_capacity(total);
            let mut line = 0usize;
            while line < total {
                vis.push(line);
                if self.folded.contains(&line) {
                    if let Some(r) = self.fold_range_at(line) {
                        line = (r.end_line + 1).max(line + 1);
                        continue;
                    }
                }
                line += 1;
            }
            self.cached_visible = Some((self.fold_version, total, vis));
        }
    }

    /// Returns the cached sorted visible 0-based document lines without mutating `self`.
    /// Call [`Self::ensure_visible_lines`] first when `fold_version` or line count may have changed.
    pub fn cached_visible_lines(&self) -> &[usize] {
        self.cached_visible
            .as_ref()
            .map(|(_, _, v)| v.as_slice())
            .unwrap_or(&[])
    }

    /// Returns the sorted list of visible 0-based document lines for a document with `total_lines` lines.
    pub fn visible_lines(&mut self, total_lines: usize) -> &[usize] {
        self.ensure_visible_lines(total_lines);
        self.cached_visible_lines()
    }
}

/// Mapping entry for a single visible logical line between document and display coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleLineMap {
    /// 0-based line index in the real document.
    pub doc_line: usize,
    /// Start char index of this line in the real document.
    pub doc_start: usize,
    /// End char index of this line's content (excluding `\r` and `\n`) in the real document.
    pub doc_end: usize,
    /// Start char index of this line in the display text.
    pub disp_start: usize,
    /// End char index of this line's content (excluding `\n`) in the display text.
    pub disp_end: usize,
    /// Number of document lines hidden immediately after this line (0 if not folded).
    pub hidden_after: usize,
}

/// Visual marker for the last galley row of a folded logical line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldedRowMarker {
    pub galley_row: usize,
    pub doc_line: usize,
    pub hidden_count: usize,
}

/// Coordinate projection between real document char offsets and projected visible display offsets.
#[derive(Clone, Debug, Default)]
pub struct FoldView {
    pub lines: Vec<VisibleLineMap>,
    pub folded_row_markers: Vec<FoldedRowMarker>,
}

impl FoldView {
    /// Builds a `FoldView` and projected display text for the full document in a single O(N) pass.
    pub fn build_full(text: &str, fold_state: &FoldState) -> (Self, String) {
        let mut lines = Vec::new();
        let mut display = String::with_capacity(text.len());
        let mut doc_char_pos = 0usize;
        let mut disp_char_pos = 0usize;
        let mut skip_until_line = 0usize;

        let mut line_iter = text.split('\n').enumerate().peekable();
        while let Some((doc_line, raw_line)) = line_iter.next() {
            let has_newline = line_iter.peek().is_some();
            let raw_chars = raw_line.chars().count();
            let has_cr = raw_line.ends_with('\r');
            let content = if has_cr {
                &raw_line[..raw_line.len() - 1]
            } else {
                raw_line
            };
            let content_chars = if has_cr { raw_chars - 1 } else { raw_chars };
            let doc_start = doc_char_pos;
            let doc_end = doc_start + content_chars;
            doc_char_pos += raw_chars + usize::from(has_newline);

            if doc_line < skip_until_line {
                continue;
            }

            if !lines.is_empty() {
                display.push('\n');
                disp_char_pos += 1;
            }

            let disp_start = disp_char_pos;
            display.push_str(content);
            disp_char_pos += content_chars;
            let disp_end = disp_char_pos;

            let hidden_after = if fold_state.is_folded(doc_line) {
                if let Some(r) = fold_state.fold_range_at(doc_line) {
                    skip_until_line = (r.end_line + 1).max(doc_line + 1);
                    r.end_line - doc_line
                } else {
                    0
                }
            } else {
                0
            };

            lines.push(VisibleLineMap {
                doc_line,
                doc_start,
                doc_end,
                disp_start,
                disp_end,
                hidden_after,
            });
        }

        (
            Self {
                lines,
                folded_row_markers: Vec::new(),
            },
            display,
        )
    }

    /// Builds a `FoldView` and projected display text for a virtualized window of visible lines.
    pub fn build_window<T: EditorBuffer>(
        buffer: &T,
        visible_lines: &[usize],
        first_vis: usize,
        last_vis: usize,
        fold_state: &FoldState,
    ) -> (Self, String) {
        let end_vis = last_vis.min(visible_lines.len());
        let start_vis = first_vis.min(end_vis);
        let count = end_vis - start_vis;
        let mut lines = Vec::with_capacity(count);
        let mut display = String::new();
        let mut line_buf = String::new();
        let mut disp_char_pos = 0usize;

        for &doc_line in &visible_lines[start_vis..end_vis] {
            if !lines.is_empty() {
                display.push('\n');
                disp_char_pos += 1;
            }
            let range = buffer.line_char_range(doc_line);
            buffer.extract_char_range(range.clone(), &mut line_buf);
            let content_chars = range.end.saturating_sub(range.start);
            let disp_start = disp_char_pos;
            display.push_str(&line_buf);
            disp_char_pos += content_chars;
            let disp_end = disp_char_pos;

            let hidden_after = fold_state.hidden_line_count_after(doc_line);
            lines.push(VisibleLineMap {
                doc_line,
                doc_start: range.start,
                doc_end: range.end,
                disp_start,
                disp_end,
                hidden_after,
            });
        }

        (
            Self {
                lines,
                folded_row_markers: Vec::new(),
            },
            display,
        )
    }

    /// Populates `folded_row_markers` matching the last visual row in `galley` of each folded line.
    pub fn populate_row_markers(&mut self, galley: &Galley) {
        self.folded_row_markers.clear();
        let mut vis_line_idx = 0usize;
        let row_count = galley.rows.len();
        for (row_idx, row) in galley.rows.iter().enumerate() {
            let is_last_row_of_line = row.ends_with_newline || row_idx + 1 == row_count;
            if is_last_row_of_line {
                if let Some(entry) = self.lines.get(vis_line_idx) {
                    if entry.hidden_after > 0 {
                        self.folded_row_markers.push(FoldedRowMarker {
                            galley_row: row_idx,
                            doc_line: entry.doc_line,
                            hidden_count: entry.hidden_after,
                        });
                    }
                }
                vis_line_idx += 1;
            }
        }
    }

    /// Total number of characters in the projected display text.
    pub fn disp_len(&self) -> usize {
        self.lines.last().map(|l| l.disp_end).unwrap_or(0)
    }

    /// Document character span covered by this `FoldView` (`first.doc_start..last.doc_end`).
    pub fn doc_span(&self) -> Range<usize> {
        match (self.lines.first(), self.lines.last()) {
            (Some(first), Some(last)) => first.doc_start..last.doc_end,
            _ => 0..0,
        }
    }

    /// Maps a display char index to its corresponding real document char index.
    pub fn disp_to_doc(&self, disp_idx: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let idx = self
            .lines
            .partition_point(|l| l.disp_start <= disp_idx)
            .saturating_sub(1);
        let entry = &self.lines[idx];
        let col = disp_idx
            .saturating_sub(entry.disp_start)
            .min(entry.doc_end.saturating_sub(entry.doc_start));
        entry.doc_start + col
    }

    /// Maps a real document char index to its display char index in this view.
    ///
    /// Positions inside hidden lines clamp to the end of the preceding visible folded header line.
    pub fn doc_to_disp(&self, doc_idx: usize) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        if doc_idx <= self.lines[0].doc_start {
            return 0;
        }
        let idx = self
            .lines
            .partition_point(|l| l.doc_start <= doc_idx)
            .saturating_sub(1);
        let entry = &self.lines[idx];
        if doc_idx <= entry.doc_end {
            entry.disp_start + (doc_idx - entry.doc_start)
        } else {
            entry.disp_end
        }
    }
}
