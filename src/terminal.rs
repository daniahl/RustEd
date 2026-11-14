//! Integrated terminal pane using Windows ConPTY.
//!
//! Manages pseudo-terminal (PTY) child process execution, streaming I/O, VT100 escape sequence parsing,
//! screen buffer rendering, and terminal keyboard input forwarding.

use crate::editor::{terminal_id, FastEditor, FastEditorState};
use eframe::egui;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

pub const TERM_ROWS: u16 = 30;
pub const TERM_COLS: u16 = 120;

/// Detects `\x1b[6n` (DSR cursor position request) within `chunk` or split across the
/// previous chunk's trailing bytes (`tail[..*tail_len]`), updating the trailing buffer.
pub fn contains_dsr_request(tail: &mut [u8; 3], tail_len: &mut usize, chunk: &[u8]) -> bool {
    const DSR: &[u8; 4] = b"\x1b[6n";
    if chunk.is_empty() {
        return false;
    }

    let mut found = false;
    if *tail_len > 0 {
        let mut boundary = [0u8; 6];
        boundary[..*tail_len].copy_from_slice(&tail[..*tail_len]);
        let prefix_len = chunk.len().min(3);
        boundary[*tail_len..*tail_len + prefix_len].copy_from_slice(&chunk[..prefix_len]);
        if boundary[..*tail_len + prefix_len]
            .windows(DSR.len())
            .any(|w| w == DSR)
        {
            found = true;
        }
    }
    if !found && chunk.windows(DSR.len()).any(|w| w == DSR) {
        found = true;
    }

    if chunk.len() >= 3 {
        tail.copy_from_slice(&chunk[chunk.len() - 3..]);
        *tail_len = 3;
    } else {
        let mut combined = [0u8; 5];
        combined[..*tail_len].copy_from_slice(&tail[..*tail_len]);
        combined[*tail_len..*tail_len + chunk.len()].copy_from_slice(chunk);
        let total = *tail_len + chunk.len();
        let keep = total.min(3);
        tail[..keep].copy_from_slice(&combined[total - keep..total]);
        *tail_len = keep;
    }

    found
}

pub struct TerminalState {
    pub parser: vt100::Parser,
    pub pty_updated: bool,
}

pub struct TerminalPane {
    pub term: Arc<Mutex<TerminalState>>,
    pub term_screen_text: String,
    pub term_revision: u64,
    pub term_cursor_idx: usize,
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
    pub _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub _pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pub shutdown: Arc<AtomicBool>,
    pub focus_terminal: bool,
    pub selecting_in_terminal: bool,
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        self.shutdown_session();
    }
}

impl TerminalPane {
    fn shutdown_session(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.pty_writer = None;
        if let Some(mut child) = self._pty_child.take() {
            let _ = child.kill();
            let _ = child.try_wait();
        }
        self._pty_master = None;
    }

    pub fn new(ctx: egui::Context, working_dir: Option<&Path>) -> Self {
        let term = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(
                std::num::NonZeroU16::new(TERM_ROWS).unwrap(),
                std::num::NonZeroU16::new(TERM_COLS).unwrap(),
                0,
            ),
            pty_updated: true,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
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
            let cwd = working_dir
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok());
            if let Some(dir) = cwd {
                cmd.cwd(dir);
            }
            match pair.slave.spawn_command(cmd) {
                Ok(child) => {
                    pty_child = Some(child);
                }
                Err(e) => {
                    let mut lock = term.lock().unwrap();
                    lock.parser
                        .process(format!("Failed to spawn shell: {e}\r\n").as_bytes());
                }
            }
            drop(pair.slave);

            if let Ok(mut writer) = pair.master.take_writer() {
                let (tx, rx) = mpsc::channel::<Vec<u8>>();
                thread::spawn(move || {
                    while let Ok(bytes) = rx.recv() {
                        let _ = writer.write_all(&bytes);
                        let _ = writer.flush();
                    }
                });
                pty_writer = Some(tx);
            }

            if let Ok(mut reader) = pair.master.try_clone_reader() {
                let term_clone = Arc::clone(&term);
                let shutdown_flag = Arc::clone(&shutdown);
                let dsr_writer = pty_writer.clone();
                thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let mut dsr_tail = [0u8; 3];
                    let mut dsr_tail_len = 0usize;
                    while !shutdown_flag.load(Ordering::Relaxed) {
                        let Ok(n) = reader.read(&mut buf) else {
                            break;
                        };
                        if n == 0 || shutdown_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        let mut lock = term_clone.lock().unwrap();
                        lock.parser.process(&buf[..n]);
                        let cpr_reply =
                            contains_dsr_request(&mut dsr_tail, &mut dsr_tail_len, &buf[..n])
                                .then(|| {
                                    let (row, col) = lock.parser.screen().cursor_position();
                                    format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
                                });
                        let was_updated = std::mem::replace(&mut lock.pty_updated, true);
                        drop(lock);
                        if let (Some(tx), Some(reply)) = (&dsr_writer, cpr_reply) {
                            let _ = tx.send(reply);
                        }
                        if !was_updated {
                            ctx.request_repaint();
                        }
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
            term_revision: 0,
            term_cursor_idx: 0,
            pty_writer,
            _pty_master: pty_master,
            _pty_child: pty_child,
            shutdown,
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    pub fn new_headless() -> Self {
        Self {
            term: Arc::new(Mutex::new(TerminalState {
                parser: vt100::Parser::new(
                    std::num::NonZeroU16::new(TERM_ROWS).unwrap(),
                    std::num::NonZeroU16::new(TERM_COLS).unwrap(),
                    0,
                ),
                pty_updated: false,
            })),
            term_screen_text: String::new(),
            term_revision: 0,
            term_cursor_idx: 0,
            pty_writer: None,
            _pty_master: None,
            _pty_child: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    pub const fn size_label(&self) -> &'static str {
        "120x30"
    }

    /// Terminates the current PTY session (if any) and spawns a fresh terminal in `working_dir`.
    pub fn restart(&mut self, ctx: &egui::Context, working_dir: Option<&Path>) {
        let next_revision = self.term_revision.wrapping_add(1);
        self.shutdown_session();
        *self = Self::new(ctx.clone(), working_dir);
        self.term_revision = next_revision;
        self.focus_terminal = true;
    }

    pub fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Some(writer) = &self.pty_writer {
            let _ = writer.send(bytes.to_vec());
        }
    }

    pub fn handle_terminal_input(&mut self, ctx: &egui::Context, term_focused: bool) {
        if !term_focused {
            return;
        }

        let mut to_send: Vec<u8> = Vec::new();
        let mut clear_selection = false;

        ctx.input_mut(|i| {
            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        clear_selection = true;
                        false
                    }
                    egui::Event::Paste(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        clear_selection = true;
                        false
                    }
                    egui::Event::Cut => false,
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
                        if *key == egui::Key::C && modifiers.ctrl && self.selecting_in_terminal {
                            return false;
                        }

                        let seq: Option<&[u8]> = match key {
                            egui::Key::Enter => Some(b"\r"),
                            egui::Key::Backspace => Some(b"\x7f"),
                            egui::Key::Tab if !modifiers.ctrl && !modifiers.command => Some(b"\t"),
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
                            clear_selection = true;
                            false
                        } else {
                            true
                        }
                    }
                    _ => true,
                }
            });
        });

        if clear_selection {
            let term_id = terminal_id();
            self.selecting_in_terminal = false;
            ctx.data_mut(|d| {
                if let Some(mut state) = d.get_temp::<FastEditorState>(term_id) {
                    state.mouse_selection = None;
                    d.insert_temp(term_id, state);
                }
            });
        }
        if !to_send.is_empty() {
            self.write_to_pty(&to_send);
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, panel_frame: egui::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        let term_id = terminal_id();
        let term_focused = ctx.memory(|m| m.has_focus(term_id));

        self.selecting_in_terminal = ctx.data(|d| {
            d.get_temp::<FastEditorState>(term_id)
                .and_then(|s| s.mouse_selection)
                .is_some_and(|r| r.primary != r.secondary)
        });

        self.handle_terminal_input(ctx, term_focused);

        let pty_updated = {
            let mut lock = self.term.lock().unwrap();
            let updated = lock.pty_updated || self.term_screen_text.is_empty();
            if updated {
                lock.pty_updated = false;
                self.term_cursor_idx =
                    render_vt100_screen_into(lock.parser.screen(), &mut self.term_screen_text);
                self.term_revision = self.term_revision.wrapping_add(1);
                true
            } else {
                false
            }
        };

        if pty_updated {
            self.selecting_in_terminal = false;
            ctx.data_mut(|d| {
                let mut state = d.get_temp::<FastEditorState>(term_id).unwrap_or_default();
                state.mouse_selection = None;
                let target_range =
                    egui::text::CCursorRange::one(egui::text::CCursor::new(self.term_cursor_idx));
                if state.cursor.char_range() != Some(target_range) {
                    state.cursor.set_char_range(Some(target_range));
                    state.scroll_to_cursor = true;
                }
                d.insert_temp(term_id, state);
            });
        }

        let screen_text = &mut self.term_screen_text;
        let term_revision = self.term_revision;
        let focus_terminal = &mut self.focus_terminal;

        egui::CentralPanel::default().frame(panel_frame).show(ui, |ui| {
            let font_id = egui::TextStyle::Monospace.resolve(ui.style());
            let text_color = ui.visuals().widgets.inactive.text_color();
            let mut layouter = |ui: &egui::Ui, text: &str, _wrap_width: f32| {
                let job = egui::text::LayoutJob::simple(
                    text.to_owned(),
                    font_id.clone(),
                    text_color,
                    f32::INFINITY,
                );
                ui.fonts_mut(|f| f.layout_job(job))
            };

            let available_size = ui.available_size();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .animated(false)
                .show(ui, |ui| {
                    let output =
                        FastEditor::new(term_id, screen_text, available_size, &mut layouter)
                            .text_revision(term_revision)
                            .pointer_moves_caret(false)
                            .show(ui);

                    if *focus_terminal {
                        output.response.request_focus();
                        *focus_terminal = false;
                    }
                });
        });
    }
}

pub fn render_vt100_screen(screen: &vt100::Screen) -> (String, usize) {
    let (rows, cols) = screen.size();
    let rows = rows.get();
    let cols = cols.get();
    let mut full_text = String::with_capacity((rows as usize) * (cols as usize + 1));
    let cursor_char_idx = render_vt100_screen_into(screen, &mut full_text);
    (full_text, cursor_char_idx)
}

pub fn render_vt100_screen_into(screen: &vt100::Screen, full_text: &mut String) -> usize {
    let (rows, cols) = screen.size();
    let rows = rows.get();
    let cols = cols.get();
    let (cur_row, cur_col) = screen.cursor_position();

    full_text.clear();
    full_text.reserve((rows as usize) * (cols as usize + 1));
    let mut cursor_char_idx = 0;
    let mut total_chars = 0;

    for (r, line) in screen.rows(0, cols).enumerate() {
        let r_u16 = r as u16;
        let line_chars = line
            .as_bytes()
            .iter()
            .filter(|&&b| (b & 0xC0) != 0x80)
            .count();
        full_text.push_str(&line);
        total_chars += line_chars;

        if r_u16 == cur_row {
            let target_col = cur_col as usize;
            if target_col > line_chars {
                let pad = target_col - line_chars;
                const SPACES: &str = "                                                                ";
                let mut rem = pad;
                while rem > 0 {
                    let chunk = rem.min(SPACES.len());
                    full_text.push_str(&SPACES[..chunk]);
                    rem -= chunk;
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

    cursor_char_idx
}
