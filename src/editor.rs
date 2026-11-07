use eframe::egui::{
    self,
    epaint::text::Galley,
    text_selection::{
        visuals::{paint_text_cursor, paint_text_selection},
        CCursorRange, CursorRange, TextCursorState,
    },
    Color32, Event, Id, Key, Pos2, Rect, Response, Sense, TextBuffer, Ui, Vec2,
};
use std::sync::{Arc, Mutex};

type Undoer = egui::util::undoer::Undoer<(CCursorRange, String)>;

#[derive(Clone, Default)]
pub struct FastEditorState {
    pub cursor: TextCursorState,
    pub undoer: Arc<Mutex<Undoer>>,
    pub last_interaction_time: f64,
    /// Set after an edit / caret move until the caret has actually been scrolled into view.
    pub scroll_to_cursor: bool,
    /// Last layout result, reused on frames where text, width and theme are unchanged.
    pub layout_cache: Option<Arc<LayoutCache>>,
}

/// Cached galley for a given text + layout parameters.
pub struct LayoutCache {
    key: (u32, u32, bool, u64),
    text: String,
    galley: Arc<Galley>,
}

#[allow(dead_code)]
pub struct EditorOutput {
    pub response: Response,
    pub galley: Arc<Galley>,
    pub galley_pos: Pos2,
    pub text_clip_rect: Rect,
    pub cursor_range: Option<CursorRange>,
}

pub struct FastEditor<'a> {
    id: Id,
    text: &'a mut String,
    min_size: Vec2,
    layouter: &'a mut dyn FnMut(&Ui, &str, f32) -> Arc<Galley>,
    cache_salt: u64,
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
        }
    }

    /// Extra value mixed into the layout-cache key. Change it whenever something the
    /// layouter depends on (e.g. syntax language) changes.
    #[allow(dead_code)]
    pub fn cache_salt(mut self, salt: u64) -> Self {
        self.cache_salt = salt;
        self
    }

    pub fn show(self, ui: &mut Ui) -> EditorOutput {
        const MARGIN: Vec2 = Vec2::new(6.0, 2.0);
        let wrap_width = (ui.available_width() - MARGIN.x * 2.0).max(100.0);

        let mut state = ui
            .ctx()
            .data_mut(|d| d.get_temp::<FastEditorState>(self.id))
            .unwrap_or_default();

        // Reuse the previous layout when nothing it depends on has changed. A plain string
        // comparison (memcmp) is far cheaper than egui hashing the text + layout job each frame.
        let cache_key = (
            wrap_width.to_bits(),
            ui.ctx().pixels_per_point().to_bits(),
            ui.visuals().dark_mode,
            self.cache_salt,
        );
        let mut layout_dirty = false;
        let mut galley = match &state.layout_cache {
            Some(c) if c.key == cache_key && c.text == *self.text => Arc::clone(&c.galley),
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
                    ui.memory_mut(|mem| {
                        mem.request_focus(self.id);
                        mem.set_focus_lock_filter(
                            self.id,
                            egui::EventFilter {
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                tab: true,
                                ..Default::default()
                            },
                        );
                    });
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

        if has_focus {
            ui.memory_mut(|mem| {
                mem.set_focus_lock_filter(
                    self.id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        tab: true,
                        ..Default::default()
                    },
                );
            });

            let events = ui.input(|i| i.events.clone());
            let now = ui.input(|i| i.time);

            // Feed state to undoer if in flux or initially empty so undo points can stabilize
            {
                let mut undoer = state.undoer.lock().unwrap();
                if undoer.is_in_flux() {
                    undoer.feed_state(
                        now,
                        &(cursor_range.as_ccursor_range(), self.text.clone()),
                    );
                }
            }

            for event in &events {
                let mut mutated: Option<CCursorRange> = None;

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
                        if let Some((undo_range, undo_txt)) = state
                            .undoer
                            .lock()
                            .unwrap()
                            .undo(&(cursor_range.as_ccursor_range(), self.text.clone()))
                        {
                            *self.text = undo_txt.to_string();
                            mutated = Some(*undo_range);
                        }
                    }
                    Event::Key {
                        key: Key::Y,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command => {
                        if let Some((redo_range, redo_txt)) = state
                            .undoer
                            .lock()
                            .unwrap()
                            .redo(&(cursor_range.as_ccursor_range(), self.text.clone()))
                        {
                            *self.text = redo_txt.to_string();
                            mutated = Some(*redo_range);
                        }
                    }
                    _ => {}
                }

                if let Some(new_ccursor_range) = mutated {
                    text_changed = true;
                    state.last_interaction_time = now;
                    galley = (self.layouter)(ui, self.text.as_str(), wrap_width);
                    layout_dirty = true;
                    cursor_range = CursorRange {
                        primary: galley.from_ccursor(new_ccursor_range.primary),
                        secondary: galley.from_ccursor(new_ccursor_range.secondary),
                    };
                }
            }
        }

        state.cursor.set_range(Some(cursor_range));
        if text_changed {
            state.undoer.lock().unwrap().feed_state(
                ui.input(|i| i.time),
                &(cursor_range.as_ccursor_range(), self.text.clone()),
            );
        }

        let text_clip_rect = ui.clip_rect().intersect(rect);
        let painter = ui.painter_at(rect);

        // Selection & cursor rendering
        if has_focus {
            let cursor_box = galley
                .pos_from_cursor(&cursor_range.primary)
                .translate(galley_pos.to_vec2());
            let cursor_moved = state.cursor.char_range() != prev_cursor_range;

            if text_changed || cursor_moved {
                state.scroll_to_cursor = true;
            }

            if state.scroll_to_cursor || !cursor_range.is_empty() {
                ui.scroll_to_rect(cursor_box, None);
            }

            let mut galley_to_paint = Arc::clone(&galley);
            paint_text_selection(&mut galley_to_paint, ui.visuals(), &cursor_range, None);
            painter.galley(galley_pos, galley_to_paint, Color32::WHITE);

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
        } else {
            painter.galley(galley_pos, galley.clone(), Color32::WHITE);
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

        // 2. When focused, set the text selection and build the active row node
        // inside `with_accessibility_parent(self.id, ...)` so that the row is a child of self.id ONLY.
        if has_focus {
            let focus = cursor_range.primary.rcursor;
            let cursor_rect = galley
                .pos_from_cursor(&cursor_range.primary)
                .translate(galley_pos.to_vec2());

            if text_clip_rect.contains(cursor_rect.center()) {
                state.scroll_to_cursor = false;
            }

            if !text_clip_rect.contains(cursor_rect.center()) {
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

                        let total_chars = character_lengths.len() as u8;
                        builder.set_value(value);
                        builder.set_character_lengths(character_lengths);
                        builder.set_character_positions(character_positions);
                        builder.set_character_widths(character_widths);
                        builder.set_word_lengths(vec![total_chars]);
                    });
                });
            }
        }

        if layout_dirty {
            state.layout_cache = Some(Arc::new(LayoutCache {
                key: cache_key,
                text: self.text.clone(),
                galley: Arc::clone(&galley),
            }));
        }
        ui.ctx().data_mut(|d| d.insert_temp(self.id, state));

        EditorOutput {
            response,
            galley,
            galley_pos,
            text_clip_rect,
            cursor_range: Some(cursor_range),
        }
    }
}
