//! AccessKit `MultilineTextInput` and active-row `TextRun` synchronization for `FastEditor`.

use super::FastEditorState;
use eframe::egui::{self, epaint::text::Galley, Id, Pos2, Rect, Ui};
use std::sync::Arc;

/// Cached identity of the active AccessKit `TextRun` row node.
#[derive(Clone)]
pub struct AccessKitRowCache {
    pub galley: Arc<Galley>,
    pub row: usize,
    pub bounds: egui::accesskit::Rect,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn update_accesskit(
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
