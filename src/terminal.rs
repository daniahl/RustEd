//! Integrated terminal pane using Windows ConPTY.
//!
//! Manages pseudo-terminal (PTY) child process execution, streaming I/O, VT100 escape sequence parsing,
//! screen buffer rendering, and terminal keyboard input forwarding.

use eframe::egui;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
};

pub const TERM_ROWS: u16 = 30;
pub const TERM_COLS: u16 = 120;

pub struct TerminalState {
    pub parser: vt100::Parser,
    pub pty_updated: bool,
}

pub struct TerminalPane {
    pub term: Arc<Mutex<TerminalState>>,
    pub term_screen_text: String,
    pub term_cursor_idx: usize,
    pub pty_writer: Option<Box<dyn Write + Send>>,
    pub _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub _pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pub focus_terminal: bool,
    pub selecting_in_terminal: bool,
}

impl TerminalPane {
    pub fn new(ctx: egui::Context) -> Self {
        let term = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(TERM_ROWS, TERM_COLS, 0),
            pty_updated: true,
        }));
        let mut pty_writer = None;
        let mut pty_master = None;
        let mut pty_child = None;

        let pty_system = native_pty_system();
        if let Ok(pair) = pty_system.openpty(PtySize {
            rows: TERM_ROWS,
            cols: TERM_COLS,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            let mut cmd = CommandBuilder::new("cmd.exe");
            if let Ok(cwd) = std::env::current_dir() {
                cmd.cwd(cwd);
            }
            if let Ok(child) = pair.slave.spawn_command(cmd) {
                pty_child = Some(child);
            }
            drop(pair.slave);

            if let Ok(writer) = pair.master.take_writer() {
                pty_writer = Some(writer);
            }

            if let Ok(mut reader) = pair.master.try_clone_reader() {
                let term_clone = Arc::clone(&term);
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = reader.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut lock = term_clone.lock().unwrap();
                        lock.parser.process(&buf[..n]);
                        lock.pty_updated = true;
                        drop(lock);
                        ctx.request_repaint();
                    }
                });
            }

            pty_master = Some(pair.master);
        } else {
            let mut lock = term.lock().unwrap();
            lock.parser.process(b"Failed to open ConPTY.\r\n");
        }

        Self {
            term,
            term_screen_text: String::new(),
            term_cursor_idx: 0,
            pty_writer,
            _pty_master: pty_master,
            _pty_child: pty_child,
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    pub fn new_headless() -> Self {
        Self {
            term: Arc::new(Mutex::new(TerminalState {
                parser: vt100::Parser::new(TERM_ROWS, TERM_COLS, 0),
                pty_updated: false,
            })),
            term_screen_text: String::new(),
            term_cursor_idx: 0,
            pty_writer: None,
            _pty_master: None,
            _pty_child: None,
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    pub fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Some(writer) = &mut self.pty_writer {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    pub fn handle_terminal_input(&mut self, ctx: &egui::Context, term_focused: bool) {
        if !term_focused {
            return;
        }

        let mut to_send: Vec<u8> = Vec::new();
        let mut exit_selection_mode = false;
        let mut entered_selection_mode = false;

        ctx.input_mut(|i| {
            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        exit_selection_mode = true;
                        false
                    }
                    egui::Event::Paste(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        exit_selection_mode = true;
                        false
                    }
                    egui::Event::Copy => {
                        if self.selecting_in_terminal {
                            true
                        } else {
                            to_send.push(0x03);
                            false
                        }
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if modifiers.shift
                            && matches!(
                                key,
                                egui::Key::ArrowLeft
                                    | egui::Key::ArrowRight
                                    | egui::Key::ArrowUp
                                    | egui::Key::ArrowDown
                                    | egui::Key::Home
                                    | egui::Key::End
                            )
                        {
                            entered_selection_mode = true;
                            return true;
                        }

                        let seq: Option<&[u8]> = match key {
                            egui::Key::Enter => Some(b"\r"),
                            egui::Key::Backspace => Some(b"\x7f"),
                            egui::Key::Tab => Some(b"\t"),
                            egui::Key::Escape => Some(b"\x1b"),
                            egui::Key::Delete => Some(b"\x1b[3~"),
                            egui::Key::ArrowUp => Some(b"\x1b[A"),
                            egui::Key::ArrowDown => Some(b"\x1b[B"),
                            egui::Key::ArrowRight => Some(b"\x1b[C"),
                            egui::Key::ArrowLeft => Some(b"\x1b[D"),
                            egui::Key::Home => Some(b"\x1b[H"),
                            egui::Key::End => Some(b"\x1b[F"),
                            egui::Key::PageUp => Some(b"\x1b[5~"),
                            egui::Key::PageDown => Some(b"\x1b[6~"),
                            egui::Key::C if modifiers.ctrl => Some(b"\x03"),
                            egui::Key::D if modifiers.ctrl => Some(b"\x04"),
                            egui::Key::Z if modifiers.ctrl => Some(b"\x1a"),
                            egui::Key::L if modifiers.ctrl => Some(b"\x0c"),
                            _ => None,
                        };

                        if let Some(bytes) = seq {
                            to_send.extend_from_slice(bytes);
                            exit_selection_mode = true;
                            false
                        } else {
                            true
                        }
                    }
                    _ => true,
                }
            });
        });

        if entered_selection_mode {
            self.selecting_in_terminal = true;
        }
        if exit_selection_mode {
            self.selecting_in_terminal = false;
        }
        if !to_send.is_empty() {
            self.write_to_pty(&to_send);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, panel_frame: egui::Frame) {
        let term_id = egui::Id::new("conpty_terminal");
        let term_focused = ctx.memory(|m| m.has_focus(term_id));

        self.handle_terminal_input(ctx, term_focused);

        let pty_updated = {
            let mut lock = self.term.lock().unwrap();
            let updated = lock.pty_updated || self.term_screen_text.is_empty();
            if updated {
                lock.pty_updated = false;
                let (text, idx) = render_vt100_screen(lock.parser.screen());
                self.term_screen_text = text;
                self.term_cursor_idx = idx;
            }
            updated
        };

        if pty_updated {
            self.selecting_in_terminal = false;
        }

        if !self.selecting_in_terminal {
            let mut state = egui::text_edit::TextEditState::load(ctx, term_id).unwrap_or_default();
            let ccursor = egui::text::CCursor::new(self.term_cursor_idx);
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(ccursor)));
            state.store(ctx, term_id);
        }

        let screen_text = &mut self.term_screen_text;
        let focus_terminal = &mut self.focus_terminal;

        egui::CentralPanel::default().frame(panel_frame).show(ctx, |ui| {
            let available_size = ui.available_size();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .animated(false)
                .show(ui, |ui| {
                    let output = egui::TextEdit::multiline(screen_text)
                        .id(term_id)
                        .code_editor()
                        .frame(false)
                        .desired_width(f32::INFINITY)
                        .min_size(available_size)
                        .lock_focus(true)
                        .show(ui);

                    if *focus_terminal {
                        output.response.request_focus();
                        *focus_terminal = false;
                    }

                    fix_accesskit_caret(ctx, &output);
                });
        });
    }
}

pub fn render_vt100_screen(screen: &vt100::Screen) -> (String, usize) {
    let (rows, cols) = screen.size();
    let (cur_row, cur_col) = screen.cursor_position();

    let mut full_text = String::with_capacity((rows as usize) * (cols as usize + 1));
    let mut cursor_char_idx = 0;
    let mut total_chars = 0;

    for (r, line) in screen.rows(0, cols).enumerate() {
        let r_u16 = r as u16;
        let line_chars = line.chars().count();
        full_text.push_str(&line);
        total_chars += line_chars;

        if r_u16 == cur_row {
            let target_col = cur_col as usize;
            if target_col > line_chars {
                let pad = target_col - line_chars;
                for _ in 0..pad {
                    full_text.push(' ');
                }
                total_chars += pad;
            }
            cursor_char_idx = (total_chars - line_chars.max(target_col)) + target_col;
        }

        if r_u16 + 1 < rows {
            full_text.push('\n');
            total_chars += 1;
        }
    }

    (full_text, cursor_char_idx)
}

pub fn fix_accesskit_caret(ctx: &egui::Context, output: &egui::text_edit::TextEditOutput) {
    let Some(cursor_range) = output.cursor_range else {
        return;
    };

    let cursor_rect = output
        .galley
        .pos_from_cursor(&cursor_range.primary)
        .translate(output.galley_pos.to_vec2());

    // If an edit/paste or large jump puts the cursor outside the visible scroll clip rect,
    // request an immediate second pass in the same frame so ScrollArea scrolls to it INSTANTLY,
    // and clear the selection on this pass so Windows Magnifier NEVER sees off-screen coordinates.
    if !output.text_clip_rect.contains(cursor_rect.center()) {
        ctx.request_discard("scroll to cursor after paste/edit");
        ctx.accesskit_node_builder(output.response.id, |builder| {
            builder.clear_text_selection();
        });
        return;
    }

    let focus = cursor_range.primary.rcursor;
    let Some(row) = output.galley.rows.get(focus.row) else {
        return;
    };

    let widget_id = output.response.id;
    let row_id = widget_id.with(focus.row);

    // Pad the active row's AccessKit TextRun with a 1px leading anchor at x=0 (index 0)
    // and a 1px trailing anchor at the end of the row (index N+1), then report the caret at
    // `focus.column + 1`.
    // This prevents `accesskit_consumer` from treating column 0 (`focus.column == 0`) as
    // `is_run_start()` (which otherwise jumps backward to the previous row's far-right `\n`
    // when arrowing right across a line break) or treating an empty last line as `is_document_end()`.
    ctx.accesskit_node_builder(row_id, |builder| {
        let mut row_min = output.galley_pos + row.rect.min.to_vec2();
        let mut row_max = output.galley_pos + row.rect.max.to_vec2();

        // Hard-clamp row bounds to the visible text area so coordinates can NEVER escape beyond the window
        row_min.y = row_min.y.clamp(output.text_clip_rect.min.y, output.text_clip_rect.max.y);
        row_max.y = row_max.y.clamp(row_min.y, output.text_clip_rect.max.y);
        row_min.x = row_min.x.clamp(output.text_clip_rect.min.x, output.text_clip_rect.max.x);
        row_max.x = row_max.x.clamp(row_min.x, output.text_clip_rect.max.x);

        if row_max.x - row_min.x < 2.0 {
            row_max.x = row_min.x + 2.0;
        }
        let rect = egui::Rect::from_min_max(row_min, row_max);
        builder.set_bounds(egui::accesskit::Rect {
            x0: rect.min.x.into(),
            y0: rect.min.y.into(),
            x1: rect.max.x.into(),
            y1: rect.max.y.into(),
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

    ctx.accesskit_node_builder(widget_id, |builder| {
        if let Some(sel) = builder.text_selection() {
            let target_idx = (focus.column + 1).min(row.glyphs.len() + 1);
            let pos = egui::accesskit::TextPosition {
                node: sel.focus.node,
                character_index: target_idx,
            };
            builder.set_text_selection(egui::accesskit::TextSelection {
                anchor: pos,
                focus: pos,
            });
        }
    });
}
