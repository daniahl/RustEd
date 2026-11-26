//! Windowed virtualized rendering and editing mode for large files in `FastEditor`.

use super::{
    accesskit::update_accesskit,
    collect_relevant_events, edit_affected_lines, feed_undoer_after_edit, feed_undoer_if_in_flux,
    finish_cursor_and_scroll, load_editor_state, paint_editor_contents, resolve_selection_range,
    restore_focus_on_escape, store_editor_state, try_unfold_clicked_badge, EditorBuffer,
    EditorOutput, FastEditor, VirtualLayoutCache, EDITOR_MARGIN, PAGE_JUMP_ROWS,
    VIRTUALIZED_OVERSCAN,
};
use crate::folding::{FoldState, FoldView};
use eframe::egui::{
    self,
    epaint::text::Galley,
    text::{CCursor, LayoutJob},
    text_selection::CCursorRange,
    Color32, Event, Key, Pos2, Rect, Sense, Ui, Vec2,
};
use std::sync::Arc;

impl<'a, T: EditorBuffer> FastEditor<'a, T> {
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
            fs.ensure_visible_lines(doc_total_lines);
            let fs_ref: &FoldState = fs;
            let (mut fv, disp_text) = FoldView::build_window(
                self.text,
                fs_ref.cached_visible_lines(),
                first_line,
                last_line,
                fs_ref,
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

    pub(super) fn show_virtualized(mut self, ui: &mut Ui) -> EditorOutput {
        const MARGIN: Vec2 = EDITOR_MARGIN;
        const OVERSCAN: usize = VIRTUALIZED_OVERSCAN;

        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let text_color = ui.visuals().widgets.inactive.text_color();
        let line_height = ui.fonts_mut(|f| f.row_height(&font_id)).max(1.0);
        let char_width = ui.fonts_mut(|f| f.glyph_width(&font_id, ' ')).max(1.0);
        let badge_width = char_width * 3.0 + 10.0;

        let (mut state, current_pass) = load_editor_state(ui, self.id);
        state.layout_cache = None;

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
        let prev_max_width = state
            .virtual_layout_cache
            .as_ref()
            .filter(|c| c.key == cache_key && text_unchanged)
            .map(|c| c.max_content_width)
            .unwrap_or(0.0);

        let alloc_width = ui
            .available_width()
            .max(self.min_size.x)
            .max(prev_max_width);
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
        let clicked_fold_badge = try_unfold_clicked_badge(
            ui,
            &response,
            rect,
            &galley,
            window_galley_pos,
            fold_view.as_deref(),
            self.fold_state.as_deref_mut(),
            badge_width,
        );
        if clicked_fold_badge {
            let new_total_vis = match self.fold_state.as_deref_mut() {
                Some(fs) if fs.has_active_folds() => {
                    fs.visible_lines(doc_total_lines).len().max(1)
                }
                _ => doc_total_lines,
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
                            state.preferred_col = None;
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
            } else {
                self.handle_selection_pointer_windowed(
                    ui,
                    &response,
                    &galley,
                    window_galley_pos,
                    &mut state,
                    window_start_char,
                    fold_view.as_deref(),
                );
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
                        state.preferred_col = None;
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
                        state.preferred_col = None;
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
                        state.preferred_col = None;
                        state.last_interaction_time = now;
                    }
                    Event::Key {
                        key: key @ (Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown),
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        let (line, col) = self.text.char_to_line_col(ccursor_range.primary.index.0);
                        let desired_col = *state.preferred_col.get_or_insert(col);
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
                        let next = (range.start + desired_col).min(range.end);
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
                        state.preferred_col = None;
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
                        state.preferred_col = None;
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
                        if self.apply_edit_outcome(
                            outcome,
                            now,
                            old_line_count,
                            affected_lines,
                            &mut state,
                            &mut ccursor_range,
                            &mut text_changed,
                            &mut should_feed_undoer,
                        ) {
                            state.preferred_col = None;
                        }
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

        let needed_width = galley.size().x + MARGIN.x * 2.0;
        let max_content_width = if text_changed {
            needed_width
        } else {
            prev_max_width.max(needed_width)
        };
        if needed_width > alloc_width + 1.0 {
            ui.ctx().request_repaint();
        }

        state.cursor.set_char_range(Some(ccursor_range));

        if should_feed_undoer {
            feed_undoer_after_edit(ui, &mut state, self.text, ccursor_range);
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

        let selection_range = resolve_selection_range(
            self.pointer_moves_caret,
            state.mouse_selection,
            ccursor_range,
            total_chars_after,
        );

        paint_editor_contents(
            ui,
            &painter,
            &galley,
            window_galley_pos,
            text_clip_rect,
            selection_range,
            window_start_char..window_end_char,
            fold_view.as_deref(),
            &font_id,
            badge_width,
        );

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
        if layout_dirty || max_content_width > prev_max_width {
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
                max_content_width,
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
