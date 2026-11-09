//! High-performance text editor widget (`FastEditor`).
//!
//! Custom multiline text editing component optimized for large files, with syntax layout caching,
//! undo/redo history, scroll management, and screen-magnifier/screen-reader accessibility (AccessKit).

use eframe::egui::{
    self,
    epaint::text::Galley,
    text_selection::{
        visuals::paint_text_cursor,
        CCursorRange, CursorRange, TextCursorState,
    },
    Color32, Event, Id, Key, Pos2, Rect, Response, Sense, TextBuffer, Ui, Vec2,
};
use std::sync::{Arc, Mutex};

type Undoer = egui::util::undoer::Undoer<(CCursorRange, Arc<String>)>;

#[derive(Clone, Default)]
pub struct FastEditorState {
    pub cursor: TextCursorState,
    pub undoer: Arc<Mutex<Undoer>>,
    pub last_interaction_time: f64,
    /// Set after an edit / caret move until the caret has actually been scrolled into view.
    pub scroll_to_cursor: bool,
    /// Last layout result, reused on frames where text, width and theme are unchanged.
    pub layout_cache: Option<Arc<LayoutCache>>,
    /// Tracks whether the editor had focus on the previous frame so we can force a
    /// `UIA_Text_TextSelectionChangedEventId` transition on the frame after focus is gained.
    pub had_focus_prev_frame: bool,
    /// Set when the editor just gained focus; causes one repaint where the selection is updated
    /// while `old.is_text_pattern_supported()` is already true.
    pub pending_accesskit_selection_sync: bool,
}

/// Cached galley for a given text + layout parameters.
pub struct LayoutCache {
    key: (u32, u32, bool, u64),
    text_revision: Option<u64>,
    text: Arc<String>,
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
}

pub struct FastEditor<'a> {
    id: Id,
    text: &'a mut String,
    min_size: Vec2,
    layouter: &'a mut dyn FnMut(&Ui, &str, f32) -> Arc<Galley>,
    cache_salt: u64,
    text_revision: Option<u64>,
}

impl<'a> FastEditor<'a> {
    pub fn new(
        id: Id,
        text: &'a mut String,
        min_size: Vec2,
        layouter: &'a mut dyn FnMut(&Ui, &str, f32) -> Arc<Galley>,
    ) -> Self {
        Self {
            id,
            text,
            min_size,
            layouter,
            cache_salt: 0,
            text_revision: None,
        }
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

    pub fn show(self, ui: &mut Ui) -> EditorOutput {
        const MARGIN: Vec2 = Vec2::new(6.0, 2.0);
        let wrap_width = (ui.available_width() - MARGIN.x * 2.0).max(100.0);

        let mut state = ui
            .ctx()
            .data_mut(|d| d.get_temp::<FastEditorState>(self.id))
            .unwrap_or_default();

        // Reuse the previous layout when nothing it depends on has changed.
        // When `text_revision` is supplied, this is an O(1) integer comparison;
        // otherwise it falls back to a string comparison.
        let cache_key = (
            wrap_width.to_bits(),
            ui.ctx().pixels_per_point().to_bits(),
            ui.visuals().dark_mode,
            self.cache_salt,
        );
        let mut layout_dirty = false;
        let mut galley = match &state.layout_cache {
            Some(c)
                if c.key == cache_key
                    && match (self.text_revision, c.text_revision) {
                        (Some(r1), Some(r2)) => r1 == r2 && c.text.len() == self.text.len(),
                        _ => c.text.as_str() == self.text.as_str(),
                    } =>
            {
                Arc::clone(&c.galley)
            }
            _ => {
                layout_dirty = true;
                (self.layouter)(ui, self.text.as_str(), wrap_width)
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

            // Feed state to undoer if in flux or initially empty so undo points can stabilize.
            // Reuse the cached Arc<String> when available to avoid cloning the buffer on idle frames.
            {
                let mut undoer = state.undoer.lock().unwrap();
                if undoer.is_in_flux() {
                    let text_arc = match &state.layout_cache {
                        Some(c) if !layout_dirty => Arc::clone(&c.text),
                        _ => Arc::new(self.text.clone()),
                    };
                    undoer.feed_state(now, &(cursor_range.as_ccursor_range(), text_arc));
                }
            }

            let events = ui.input(|i| {
                if i.events.is_empty() {
                    Vec::new()
                } else {
                    i.events.clone()
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
                    galley = (self.layouter)(ui, self.text.as_str(), wrap_width);
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
                            ui.ctx().copy_text(cursor_range.slice_str(self.text).to_owned());
                        }
                    }
                    Event::Cut => {
                        if !cursor_range.is_empty() {
                            ui.ctx().copy_text(cursor_range.slice_str(self.text).to_owned());
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
                        let current_arc = Arc::new(self.text.clone());
                        if let Some((undo_range, undo_txt)) = state
                            .undoer
                            .lock()
                            .unwrap()
                            .undo(&(cursor_range.as_ccursor_range(), current_arc))
                        {
                            *self.text = (**undo_txt).clone();
                            mutated = Some(*undo_range);
                        }
                    }
                    Event::Key {
                        key: Key::Y,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        let current_arc = Arc::new(self.text.clone());
                        if let Some((redo_range, redo_txt)) = state
                            .undoer
                            .lock()
                            .unwrap()
                            .redo(&(cursor_range.as_ccursor_range(), current_arc))
                        {
                            *self.text = (**redo_txt).clone();
                            mutated = Some(*redo_range);
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
                galley = (self.layouter)(ui, self.text.as_str(), wrap_width);
                layout_dirty = true;
                cursor_range = CursorRange {
                    primary: galley.from_ccursor(ccursor_range.primary),
                    secondary: galley.from_ccursor(ccursor_range.secondary),
                };
            }
        }

        state.cursor.set_range(Some(cursor_range));

        // Allocate the updated text buffer once (shared between LayoutCache and Undoer).
        let updated_text_arc = if layout_dirty || text_changed {
            Some(Arc::new(self.text.clone()))
        } else {
            None
        };

        if text_changed {
            if let Some(ref text_arc) = updated_text_arc {
                state.undoer.lock().unwrap().feed_state(
                    ui.input(|i| i.time),
                    &(cursor_range.as_ccursor_range(), Arc::clone(text_arc)),
                );
            }
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

        if state.scroll_to_cursor || (has_focus && !cursor_range.is_empty()) {
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
            let mut primary_cursor_rect = galley
                .pos_from_cursor(&cursor_range.primary)
                .translate(galley_pos.to_vec2());
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
        } else if text_clip_rect.contains(cursor_box.center()) {
            state.scroll_to_cursor = false;
        } else if state.scroll_to_cursor {
            if !ui.ctx().will_discard() {
                ui.ctx().request_discard("scroll to cursor");
            }
            ui.ctx().request_repaint();
        }

        // Accessibility (AccessKit) update
        // 1. Always register the container node with its bounds
        ui.ctx().accesskit_node_builder(self.id, |builder| {
            builder.set_role(egui::accesskit::Role::MultilineTextInput);
            builder.set_bounds(egui::accesskit::Rect {
                x0: rect.min.x.into(),
                y0: rect.min.y.into(),
                x1: rect.max.x.into(),
                y1: rect.max.y.into(),
            });
        });

        // 2. Always build the active row node inside `with_accessibility_parent(self.id, ...)`
        // so `self.id` always has a `TextRun` child (`old.is_text_pattern_supported() == true`),
        // ensuring `accesskit_windows` fires `UIA_Text_TextSelectionChangedEventId` immediately
        // even on the exact frame the editor regains focus (e.g. when closing the Find dialog).
        let focus = cursor_range.primary.rcursor;
        let cursor_rect = galley
            .pos_from_cursor(&cursor_range.primary)
            .translate(galley_pos.to_vec2());

        if text_clip_rect.contains(cursor_rect.center()) {
            state.scroll_to_cursor = false;
        }

        if has_focus && !text_clip_rect.contains(cursor_rect.center()) {
            // Only redo the pass when we've just asked the ScrollArea to scroll to the
            // caret (edit / caret move). If the user simply scrolled the caret out of view,
            // discarding every frame would re-run layout continuously.
            if state.scroll_to_cursor {
                if !ui.ctx().will_discard() {
                    ui.ctx().request_discard("scroll to cursor");
                }
                // If the pass limit is hit, keep scrolling on the next frame.
                ui.ctx().request_repaint();
            }
            ui.ctx().accesskit_node_builder(self.id, |builder| {
                builder.clear_text_selection();
            });
        } else if let Some(row) = galley.rows.get(focus.row) {
            let row_id = self.id.with(focus.row);
            let row_accesskit_id: egui::accesskit::NodeId = row_id.value().into();

            if has_focus {
                if !state.had_focus_prev_frame && !ui.ctx().will_discard() {
                    // On the frame focus returns to the editor (e.g. after Find dialog closes),
                    // schedule one immediate follow-up frame so UIA clients that first process
                    // `UIA_AutomationFocusChangedEventId` also receive a fresh
                    // `UIA_Text_TextSelectionChangedEventId` once focus has settled.
                    state.pending_accesskit_selection_sync = true;
                    ui.ctx().request_repaint();
                    ui.ctx().accesskit_node_builder(self.id, |builder| {
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
                    // Set selection on self.id
                    ui.ctx().accesskit_node_builder(self.id, |builder| {
                        let target_idx = (focus.column + 1).min(row.glyphs.len() + 1);
                        let focus_pos = egui::accesskit::TextPosition {
                            node: row_accesskit_id,
                            character_index: target_idx,
                        };
                        // Always report a collapsed caret at the moving (primary) end so
                        // Magnifier follows the caret even while a selection is extended.
                        builder.set_text_selection(egui::accesskit::TextSelection {
                            anchor: focus_pos,
                            focus: focus_pos,
                        });
                    });
                }
            } else {
                ui.ctx().accesskit_node_builder(self.id, |builder| {
                    builder.clear_text_selection();
                });
            }

            // Build active row inside with_accessibility_parent so self.id is its parent:
            ui.ctx().with_accessibility_parent(self.id, || {
                ui.ctx().accesskit_node_builder(row_id, |builder| {
                    builder.set_role(egui::accesskit::Role::TextRun);
                    builder.set_text_direction(egui::accesskit::TextDirection::LeftToRight);
                    let mut row_min = galley_pos + row.rect.min.to_vec2();
                    let mut row_max = galley_pos + row.rect.max.to_vec2();

                    // Hard-clamp row bounds to visible text area so coordinates never escape beyond the window
                    row_min.y = row_min.y.clamp(text_clip_rect.min.y, text_clip_rect.max.y);
                    row_max.y = row_max.y.clamp(row_min.y, text_clip_rect.max.y);
                    row_min.x = row_min.x.clamp(text_clip_rect.min.x, text_clip_rect.max.x);
                    row_max.x = row_max.x.clamp(row_min.x, text_clip_rect.max.x);
                    if row_max.x - row_min.x < 2.0 {
                        row_max.x = row_min.x + 2.0;
                    }
                    builder.set_bounds(egui::accesskit::Rect {
                        x0: row_min.x.into(),
                        y0: row_min.y.into(),
                        x1: row_max.x.into(),
                        y1: row_max.y.into(),
                    });

                    let glyph_count = row.glyphs.len();
                    let mut value = String::with_capacity(glyph_count + 2);
                    let mut character_lengths = Vec::<u8>::with_capacity(glyph_count + 2);
                    let mut character_positions = Vec::<f32>::with_capacity(glyph_count + 2);
                    let mut character_widths = Vec::<f32>::with_capacity(glyph_count + 2);

                    // Index 0: leading anchor at x = 0.0 on this row
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

                    // Index N+1: trailing anchor at the right end of this row
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
        }

        if !ui.ctx().will_discard() {
            state.had_focus_prev_frame = has_focus;
        }

        if layout_dirty {
            let text_arc =
                updated_text_arc.unwrap_or_else(|| Arc::new(self.text.clone()));
            let cached_rev = if text_changed {
                self.text_revision.map(|r| r.wrapping_add(1))
            } else {
                self.text_revision
            };
            state.layout_cache = Some(Arc::new(LayoutCache {
                key: cache_key,
                text_revision: cached_rev,
                text: text_arc,
                galley: Arc::clone(&galley),
            }));
        }
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
        }
    }
}
