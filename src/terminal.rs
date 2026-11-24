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
/// Number of scrollback lines retained above the active terminal viewport.
pub const TERM_SCROLLBACK_LEN: usize = 2_000;
/// Size of the read buffer for PTY output in bytes.
pub const PTY_READ_BUF_SIZE: usize = 4096;

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

pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        "cmd.exe".to_string()
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }
}

fn format_shell_label(shell: &str) -> String {
    let display = Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(shell);
    format!("Terminal — {display}")
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
    pub rows: u16,
    pub cols: u16,
    pub size_text: String,
    shell_label: String,
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
    dsr_writer: Arc<Mutex<Option<mpsc::Sender<Vec<u8>>>>>,
    pub(crate) _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub(crate) _pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pub(crate) shutdown: Arc<AtomicBool>,
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
        self.dsr_writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut child) = self._pty_child.take() {
            let _ = child.kill();
            if matches!(child.try_wait(), Ok(None)) {
                // Reap off the UI thread so slow termination never stalls frames or leaves zombies.
                let _ = thread::Builder::new()
                    .name("rusted-pty-reaper".to_string())
                    .spawn(move || {
                        let _ = child.wait();
                    });
            }
        }
        self._pty_master = None;
    }

    pub fn new(ctx: egui::Context, working_dir: Option<&Path>) -> Self {
        let term = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(
                std::num::NonZeroU16::new(TERM_ROWS).unwrap(),
                std::num::NonZeroU16::new(TERM_COLS).unwrap(),
                TERM_SCROLLBACK_LEN,
            ),
            pty_updated: true,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut pty_writer = None;
        let dsr_writer_slot: Arc<Mutex<Option<mpsc::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(None));
        let mut pty_master = None;
        let mut pty_child = None;
        let shell = default_shell();
        let shell_label = format_shell_label(&shell);

        let pty_system = native_pty_system();
        if let Ok(pair) = pty_system.openpty(PtySize {
            rows: TERM_ROWS,
            cols: TERM_COLS,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            let mut cmd = CommandBuilder::new(&shell);
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
                    let mut lock = term.lock().unwrap_or_else(|e| e.into_inner());
                    lock.parser
                        .process(format!("Failed to spawn shell: {e}\r\n").as_bytes());
                }
            }
            drop(pair.slave);

            if let Ok(mut writer) = pair.master.take_writer() {
                let (tx, rx) = mpsc::channel::<Vec<u8>>();
                let _ = thread::Builder::new()
                    .name("rusted-pty-writer".to_string())
                    .spawn(move || {
                        while let Ok(bytes) = rx.recv() {
                            let _ = writer.write_all(&bytes);
                            let _ = writer.flush();
                        }
                    });
                *dsr_writer_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
                pty_writer = Some(tx);
            }

            if let Ok(mut reader) = pair.master.try_clone_reader() {
                let term_clone = Arc::clone(&term);
                let shutdown_flag = Arc::clone(&shutdown);
                let dsr_writer = Arc::clone(&dsr_writer_slot);
                let _ = thread::Builder::new()
                    .name("rusted-pty-reader".to_string())
                    .spawn(move || {
                    let mut buf = [0u8; PTY_READ_BUF_SIZE];
                    let mut dsr_tail = [0u8; 3];
                    let mut dsr_tail_len = 0usize;
                    while !shutdown_flag.load(Ordering::Relaxed) {
                        let Ok(n) = reader.read(&mut buf) else {
                            break;
                        };
                        if n == 0 || shutdown_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        let mut lock = term_clone.lock().unwrap_or_else(|e| e.into_inner());
                        lock.parser.process(&buf[..n]);
                        let cpr_reply =
                            contains_dsr_request(&mut dsr_tail, &mut dsr_tail_len, &buf[..n])
                                .then(|| {
                                    let (row, col) = lock.parser.screen().cursor_position();
                                    format!("\x1b[{};{}R", row + 1, col + 1).into_bytes()
                                });
                        let was_updated = std::mem::replace(&mut lock.pty_updated, true);
                        drop(lock);
                        if let Some(reply) = cpr_reply {
                            if let Some(tx) =
                                dsr_writer.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                            {
                                let _ = tx.send(reply);
                            }
                        }
                        if !was_updated {
                            ctx.request_repaint();
                        }
                    }
                });
            }

            pty_master = Some(pair.master);
        } else {
            let mut lock = term.lock().unwrap_or_else(|e| e.into_inner());
            lock.parser.process(b"Failed to open ConPTY.\r\n");
        }

        Self {
            term,
            term_screen_text: String::new(),
            term_revision: 0,
            term_cursor_idx: 0,
            rows: TERM_ROWS,
            cols: TERM_COLS,
            size_text: format!("{TERM_COLS}x{TERM_ROWS}"),
            shell_label,
            pty_writer,
            dsr_writer: dsr_writer_slot,
            _pty_master: pty_master,
            _pty_child: pty_child,
            shutdown,
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    pub fn new_headless() -> Self {
        let shell = default_shell();
        Self {
            term: Arc::new(Mutex::new(TerminalState {
                parser: vt100::Parser::new(
                    std::num::NonZeroU16::new(TERM_ROWS).unwrap(),
                    std::num::NonZeroU16::new(TERM_COLS).unwrap(),
                    TERM_SCROLLBACK_LEN,
                ),
                pty_updated: false,
            })),
            term_screen_text: String::new(),
            term_revision: 0,
            term_cursor_idx: 0,
            rows: TERM_ROWS,
            cols: TERM_COLS,
            size_text: format!("{TERM_COLS}x{TERM_ROWS}"),
            shell_label: format_shell_label(&shell),
            pty_writer: None,
            dsr_writer: Arc::new(Mutex::new(None)),
            _pty_master: None,
            _pty_child: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    pub fn shell_label(&self) -> &str {
        &self.shell_label
    }

    pub fn size_label(&self) -> &str {
        &self.size_text
    }

    /// Resizes the PTY and VT100 parser when the grid dimensions change.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(10);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.size_text = format!("{cols}x{rows}");
        if let Some(master) = &self._pty_master {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        if let (Some(nz_rows), Some(nz_cols)) =
            (std::num::NonZeroU16::new(rows), std::num::NonZeroU16::new(cols))
        {
            let mut lock = self.term.lock().unwrap_or_else(|e| e.into_inner());
            lock.parser.screen_mut().set_size(nz_rows, nz_cols);
            lock.pty_updated = true;
        }
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

    pub fn is_bracketed_paste(&self) -> bool {
        self.term
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .parser
            .screen()
            .bracketed_paste()
    }

    fn clear_terminal_selection(&mut self, ctx: &egui::Context) {
        let term_id = terminal_id();
        self.selecting_in_terminal = false;
        ctx.data_mut(|d| {
            if let Some(mut state) = d.get_temp::<FastEditorState>(term_id) {
                state.mouse_selection = None;
                d.insert_temp(term_id, state);
            }
        });
    }

    /// Pastes `text` into the terminal (honoring bracketed paste mode when enabled)
    /// and presses Enter (`\r`) to submit it.
    pub fn paste_and_submit(&mut self, ctx: &egui::Context, text: &str) {
        self.clear_terminal_selection(ctx);
        let payload = encode_paste_bytes(text, self.is_bracketed_paste());
        if !payload.is_empty() {
            self.write_to_pty(&payload);
        }
        if self._pty_master.is_some() {
            // Live ConPTY TUIs (e.g. agy / Ink / crossterm) can miss or absorb `\r`
            // if it arrives in the exact same pipe read as the bracketed-paste terminator.
            if let Some(tx) = self.pty_writer.clone() {
                let shutdown = Arc::clone(&self.shutdown);
                let _ = thread::Builder::new()
                    .name("rusted-pty-submit".to_string())
                    .spawn(move || {
                        thread::sleep(std::time::Duration::from_millis(50));
                        if !shutdown.load(Ordering::Relaxed) {
                            let _ = tx.send(b"\r".to_vec());
                        }
                    });
            }
        } else {
            self.write_to_pty(b"\r");
        }
    }

    pub fn handle_terminal_input(&mut self, ctx: &egui::Context, term_focused: bool) {
        if !term_focused {
            return;
        }

        let mut to_send: Vec<u8> = Vec::new();
        let mut clear_selection = false;
        let (bracketed_paste, application_cursor) = {
            let lock = self.term.lock().unwrap_or_else(|e| e.into_inner());
            let screen = lock.parser.screen();
            (screen.bracketed_paste(), screen.application_cursor())
        };

        ctx.input_mut(|i| {
            let has_paste_event = i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)));
            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        to_send.extend_from_slice(text.as_bytes());
                        clear_selection = true;
                        false
                    }
                    egui::Event::Paste(text) => {
                        to_send.extend_from_slice(&encode_paste_bytes(text, bracketed_paste));
                        clear_selection = true;
                        false
                    }
                    egui::Event::Cut => false,
                    egui::Event::Copy => {
                        if self.selecting_in_terminal {
                            true
                        } else {
                            // `Event::Key` for `Ctrl+C` sends `0x03` right after `Event::Copy`.
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
                        if *key == egui::Key::V && modifiers.ctrl && has_paste_event {
                            // Avoid sending a trailing `0x16` (`^V`) alongside `Event::Paste`.
                            return false;
                        }

                        if let Some(bytes) =
                            encode_terminal_key(*key, *modifiers, application_cursor)
                        {
                            to_send.extend_from_slice(&bytes);
                            clear_selection = true;
                            false
                        } else if modifiers.ctrl && !modifiers.alt {
                            if let Some(ctrl_byte) = ctrl_key_byte(*key) {
                                to_send.push(ctrl_byte);
                                clear_selection = true;
                                false
                            } else {
                                true
                            }
                        } else {
                            true
                        }
                    }
                    _ => true,
                }
            });
        });

        if clear_selection {
            self.clear_terminal_selection(ctx);
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
            let mut lock = self.term.lock().unwrap_or_else(|e| e.into_inner());
            let updated = lock.pty_updated || self.term_screen_text.is_empty();
            if updated {
                lock.pty_updated = false;
                self.term_cursor_idx = render_vt100_screen_with_scrollback(
                    lock.parser.screen_mut(),
                    &mut self.term_screen_text,
                );
                self.term_revision = self.term_revision.wrapping_add(1);
                true
            } else {
                false
            }
        };

        if pty_updated {
            ctx.data_mut(|d| {
                let mut state = d.get_temp::<FastEditorState>(term_id).unwrap_or_default();
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
        let mut target_grid: Option<(u16, u16)> = None;

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
            let row_height = ui.fonts_mut(|f| f.row_height(&font_id)).max(1.0);
            let char_width = ui.fonts_mut(|f| f.glyph_width(&font_id, ' ')).max(1.0);
            let inner_w = (available_size.x - 16.0).max(char_width * 10.0);
            let inner_h = (available_size.y - 8.0).max(row_height * 2.0);
            let new_cols = (inner_w / char_width).floor().clamp(10.0, 500.0) as u16;
            let new_rows = (inner_h / row_height).floor().clamp(2.0, 200.0) as u16;
            target_grid = Some((new_rows, new_cols));

            egui::ScrollArea::vertical()
                .id_salt("terminal_scroll_area")
                .auto_shrink([false, false])
                .animated(false)
                .show(ui, |ui| {
                    let output =
                        FastEditor::new(term_id, screen_text, available_size, &mut layouter)
                            .text_revision(term_revision)
                            .pointer_moves_caret(false)
                            .read_only(true)
                            .show(ui);

                    if *focus_terminal {
                        crate::editor::request_editor_focus(ui.ctx(), output.response.id);
                        *focus_terminal = false;
                    }
                });
        });

        if let Some((new_rows, new_cols)) = target_grid {
            let prev = (self.rows, self.cols);
            self.resize(new_rows, new_cols);
            if (self.rows, self.cols) != prev {
                ctx.request_repaint();
            }
        }
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
            let mut char_col = 0usize;
            let mut display_col = 0u16;
            for c in 0..cur_col.min(cols) {
                if let Some(cell) = screen.cell(r_u16, c) {
                    if !cell.is_wide_continuation() {
                        let contents = cell.contents();
                        char_col += if contents.is_empty() {
                            1
                        } else {
                            contents.chars().count()
                        };
                    }
                } else {
                    char_col += 1;
                }
                display_col += 1;
            }
            if cur_col > display_col {
                char_col += (cur_col - display_col) as usize;
            }
            if char_col > line_chars {
                let pad = char_col - line_chars;
                const SPACES: &str = "                                                                ";
                let mut rem = pad;
                while rem > 0 {
                    let chunk = rem.min(SPACES.len());
                    full_text.push_str(&SPACES[..chunk]);
                    rem -= chunk;
                }
                total_chars += pad;
            }
            cursor_char_idx = (total_chars - line_chars.max(char_col)) + char_col;
        }

        if r_u16 + 1 < rows {
            full_text.push('\n');
            total_chars += 1;
        }
    }

    cursor_char_idx
}

/// Renders any accumulated scrollback lines followed by the active `vt100::Screen` into
/// `full_text`, returning the cursor's character index inside the combined text.
pub fn render_vt100_screen_with_scrollback(
    screen: &mut vt100::Screen,
    full_text: &mut String,
) -> usize {
    let (rows, cols) = screen.size();
    let rows_usize = rows.get() as usize;
    let cols_u16 = cols.get();

    screen.set_scrollback(usize::MAX);
    let scrollback_len = screen.scrollback();
    if scrollback_len == 0 {
        return render_vt100_screen_into(screen, full_text);
    }

    full_text.clear();
    full_text.reserve((scrollback_len + rows_usize) * (cols_u16 as usize + 1));
    let mut prefix_chars = 0usize;

    let mut remaining = scrollback_len;
    while remaining > 0 {
        screen.set_scrollback(remaining);
        let take = remaining.min(rows_usize);
        for line in screen.rows(0, cols_u16).take(take) {
            let line_chars = line
                .as_bytes()
                .iter()
                .filter(|&&b| (b & 0xC0) != 0x80)
                .count();
            full_text.push_str(&line);
            full_text.push('\n');
            prefix_chars += line_chars + 1;
        }
        remaining -= take;
    }

    screen.set_scrollback(0);
    let mut active_buf = String::with_capacity(rows_usize * (cols_u16 as usize + 1));
    let active_cursor = render_vt100_screen_into(screen, &mut active_buf);
    full_text.push_str(&active_buf);
    prefix_chars + active_cursor
}

/// Encodes non-character terminal keys into VT100/xterm escape sequences, honoring
/// `Shift`/`Alt`/`Ctrl` modifier parameters, `Shift+Tab` (`\x1b[Z`), and `DECCKM`
/// application cursor mode.
pub fn encode_terminal_key(
    key: egui::Key,
    modifiers: egui::Modifiers,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    let mod_param = 1u8
        + u8::from(modifiers.shift)
        + u8::from(modifiers.alt) * 2
        + u8::from(modifiers.ctrl) * 4;

    match key {
        egui::Key::Enter => Some(b"\r".to_vec()),
        egui::Key::Backspace => Some(b"\x7f".to_vec()),
        egui::Key::Tab if !modifiers.ctrl && !modifiers.command => {
            if modifiers.shift {
                Some(b"\x1b[Z".to_vec())
            } else {
                Some(b"\t".to_vec())
            }
        }
        egui::Key::Escape => Some(b"\x1b".to_vec()),
        egui::Key::ArrowUp
        | egui::Key::ArrowDown
        | egui::Key::ArrowRight
        | egui::Key::ArrowLeft
        | egui::Key::Home
        | egui::Key::End => {
            let code = match key {
                egui::Key::ArrowUp => b'A',
                egui::Key::ArrowDown => b'B',
                egui::Key::ArrowRight => b'C',
                egui::Key::ArrowLeft => b'D',
                egui::Key::Home => b'H',
                egui::Key::End => b'F',
                _ => unreachable!(),
            };
            if mod_param == 1 {
                let prefix = if application_cursor { b'O' } else { b'[' };
                Some(vec![0x1b, prefix, code])
            } else {
                Some(format!("\x1b[1;{mod_param}{}", code as char).into_bytes())
            }
        }
        egui::Key::Delete
        | egui::Key::PageUp
        | egui::Key::PageDown
        | egui::Key::F5
        | egui::Key::F6
        | egui::Key::F7
        | egui::Key::F8
        | egui::Key::F9
        | egui::Key::F10
        | egui::Key::F11
        | egui::Key::F12 => {
            let num = match key {
                egui::Key::Delete => 3,
                egui::Key::PageUp => 5,
                egui::Key::PageDown => 6,
                egui::Key::F5 => 15,
                egui::Key::F6 => 17,
                egui::Key::F7 => 18,
                egui::Key::F8 => 19,
                egui::Key::F9 => 20,
                egui::Key::F10 => 21,
                egui::Key::F11 => 23,
                egui::Key::F12 => 24,
                _ => unreachable!(),
            };
            if mod_param == 1 {
                Some(format!("\x1b[{num}~").into_bytes())
            } else {
                Some(format!("\x1b[{num};{mod_param}~").into_bytes())
            }
        }
        egui::Key::F1 | egui::Key::F2 | egui::Key::F3 | egui::Key::F4 => {
            let code = match key {
                egui::Key::F1 => 'P',
                egui::Key::F2 => 'Q',
                egui::Key::F3 => 'R',
                egui::Key::F4 => 'S',
                _ => unreachable!(),
            };
            if mod_param == 1 {
                Some(format!("\x1bO{code}").into_bytes())
            } else {
                Some(format!("\x1b[1;{mod_param}{code}").into_bytes())
            }
        }
        _ => None,
    }
}

/// Encodes text for pasting into a PTY, normalizing CRLF/CR line endings to LF so
/// multi-line Windows files do not prematurely trigger Enter mid-paste, and wrapping
/// in bracketed-paste sequences (`\x1b[200~` ... `\x1b[201~`) when active.
pub fn encode_paste_bytes(text: &str, bracketed_paste: bool) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if bracketed_paste {
        let safe = normalized.replace("\x1b[201~", "");
        let mut out = Vec::with_capacity(safe.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(safe.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.into_bytes()
    }
}

fn ctrl_key_byte(key: egui::Key) -> Option<u8> {
    let idx = match key {
        egui::Key::A => 1,
        egui::Key::B => 2,
        egui::Key::C => 3,
        egui::Key::D => 4,
        egui::Key::E => 5,
        egui::Key::F => 6,
        egui::Key::G => 7,
        egui::Key::H => 8,
        egui::Key::I => 9,
        egui::Key::J => 10,
        egui::Key::K => 11,
        egui::Key::L => 12,
        egui::Key::M => 13,
        egui::Key::N => 14,
        egui::Key::O => 15,
        egui::Key::P => 16,
        egui::Key::Q => 17,
        egui::Key::R => 18,
        egui::Key::S => 19,
        egui::Key::T => 20,
        egui::Key::U => 21,
        egui::Key::V => 22,
        egui::Key::W => 23,
        egui::Key::X => 24,
        egui::Key::Y => 25,
        egui::Key::Z => 26,
        _ => return None,
    };
    Some(idx)
}
