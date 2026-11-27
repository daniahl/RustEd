//! Integrated terminal pane using Windows ConPTY.
//!
//! Manages pseudo-terminal (PTY) child process execution, streaming I/O, VT100 escape sequence parsing,
//! screen buffer rendering, and terminal keyboard input forwarding.

use crate::{
    editor::{terminal_id, FastEditor, FastEditorState},
    piece_table::PieceTable,
};
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
    time::Duration,
};

pub const TERM_ROWS: u16 = 30;
pub const TERM_COLS: u16 = 120;
/// Number of scrollback lines retained above the active terminal viewport.
pub const TERM_SCROLLBACK_LEN: usize = 2_000;
/// Size of the read buffer for PTY output in bytes.
pub const PTY_READ_BUF_SIZE: usize = 4096;

/// Number of newest scrollback lines cached as an anchor for ring-buffer incremental sync.
const TAIL_ANCHOR_LINES: usize = 4;

enum PtyWriteCmd {
    Bytes(Vec<u8>),
    DelayedBytes(Duration, Vec<u8>),
}

fn update_dsr_tail(tail: &mut [u8; 3], tail_len: &mut usize, chunk: &[u8]) {
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
}

/// Processes `chunk` through `parser` while evaluating every `\x1b[6n` (DSR) cursor-position
/// query at its exact byte position (even when multiple queries or subsequent cursor moves
/// appear in the same chunk, or when `\x1b[6n` spans a chunk boundary).
pub fn process_chunk_with_dsr(
    parser: &mut vt100::Parser,
    tail: &mut [u8; 3],
    tail_len: &mut usize,
    chunk: &[u8],
) -> Vec<Vec<u8>> {
    const DSR: &[u8; 4] = b"\x1b[6n";
    let mut replies = Vec::new();
    if chunk.is_empty() {
        return replies;
    }

    let mut processed = 0usize;

    if *tail_len > 0 {
        let mut boundary = [0u8; 6];
        boundary[..*tail_len].copy_from_slice(&tail[..*tail_len]);
        let prefix_len = chunk.len().min(3);
        boundary[*tail_len..*tail_len + prefix_len].copy_from_slice(&chunk[..prefix_len]);
        if let Some(pos) = boundary[..*tail_len + prefix_len]
            .windows(DSR.len())
            .position(|w| w == DSR)
        {
            let dsr_end_in_chunk = (pos + DSR.len()).saturating_sub(*tail_len);
            parser.process(&chunk[..dsr_end_in_chunk]);
            let (row, col) = parser.screen().cursor_position();
            replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            processed = dsr_end_in_chunk;
        }
    }

    while processed + DSR.len() <= chunk.len() {
        if let Some(rel) = chunk[processed..]
            .windows(DSR.len())
            .position(|w| w == DSR)
        {
            let dsr_end = processed + rel + DSR.len();
            parser.process(&chunk[processed..dsr_end]);
            let (row, col) = parser.screen().cursor_position();
            replies.push(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
            processed = dsr_end;
        } else {
            break;
        }
    }

    if processed < chunk.len() {
        parser.process(&chunk[processed..]);
    }

    update_dsr_tail(tail, tail_len, chunk);
    replies
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
    pub term_screen_text: PieceTable,
    render_scratch: String,
    cached_scrollback_len: usize,
    cached_scrollback_chars: usize,
    cached_first_sb_line: String,
    cached_tail_sb_lines: Vec<String>,
    scrollback_edits_bytes: usize,
    pub term_revision: u64,
    pub term_cursor_idx: usize,
    pub rows: u16,
    pub cols: u16,
    pub size_text: String,
    shell_label: String,
    /// Test-injectable raw byte writer channel (overrides internal `pty_cmd_tx` when `Some`).
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
    pty_cmd_tx: Option<mpsc::Sender<PtyWriteCmd>>,
    dsr_writer: Arc<Mutex<Option<mpsc::Sender<PtyWriteCmd>>>>,
    pub(crate) _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    pub(crate) _pty_child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pub(crate) shutdown: Arc<AtomicBool>,
    /// When `true`, the OS PTY is deferred until the user opens the terminal or invokes AI Explain.
    lazy_spawn: bool,
    /// Whether the terminal scroll area was near the bottom on the last rendered frame.
    pinned_to_bottom: bool,
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
        self.pty_cmd_tx = None;
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
        Self::new_with_size(ctx, working_dir, TERM_ROWS, TERM_COLS)
    }

    pub fn new_with_size(
        ctx: egui::Context,
        working_dir: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Self {
        let rows = rows.max(2);
        let cols = cols.max(10);
        let term = Arc::new(Mutex::new(TerminalState {
            parser: vt100::Parser::new(
                std::num::NonZeroU16::new(rows).unwrap(),
                std::num::NonZeroU16::new(cols).unwrap(),
                TERM_SCROLLBACK_LEN,
            ),
            pty_updated: true,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut pty_cmd_tx = None;
        let dsr_writer_slot: Arc<Mutex<Option<mpsc::Sender<PtyWriteCmd>>>> =
            Arc::new(Mutex::new(None));
        let mut pty_master = None;
        let mut pty_child = None;
        let shell = default_shell();
        let shell_label = format_shell_label(&shell);

        let pty_system = native_pty_system();
        if let Ok(pair) = pty_system.openpty(PtySize {
            rows,
            cols,
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
                let (tx, rx) = mpsc::channel::<PtyWriteCmd>();
                let writer_shutdown = Arc::clone(&shutdown);
                let _ = thread::Builder::new()
                    .name("rusted-pty-writer".to_string())
                    .spawn(move || {
                        while let Ok(cmd) = rx.recv() {
                            match cmd {
                                PtyWriteCmd::Bytes(bytes) => {
                                    let _ = writer.write_all(&bytes);
                                    let _ = writer.flush();
                                }
                                PtyWriteCmd::DelayedBytes(delay, bytes) => {
                                    thread::sleep(delay);
                                    if !writer_shutdown.load(Ordering::Relaxed) {
                                        let _ = writer.write_all(&bytes);
                                        let _ = writer.flush();
                                    }
                                }
                            }
                        }
                    });
                *dsr_writer_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
                pty_cmd_tx = Some(tx);
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
                            let cpr_replies = process_chunk_with_dsr(
                                &mut lock.parser,
                                &mut dsr_tail,
                                &mut dsr_tail_len,
                                &buf[..n],
                            );
                            let was_updated = std::mem::replace(&mut lock.pty_updated, true);
                            drop(lock);
                            if !cpr_replies.is_empty() {
                                if let Some(tx) = dsr_writer
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .as_ref()
                                {
                                    for reply in cpr_replies {
                                        let _ = tx.send(PtyWriteCmd::Bytes(reply));
                                    }
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
            term_screen_text: PieceTable::new(),
            render_scratch: String::new(),
            cached_scrollback_len: 0,
            cached_scrollback_chars: 0,
            cached_first_sb_line: String::new(),
            cached_tail_sb_lines: Vec::new(),
            scrollback_edits_bytes: 0,
            term_revision: 0,
            term_cursor_idx: 0,
            rows,
            cols,
            size_text: format!("{cols}x{rows}"),
            shell_label,
            pty_writer: None,
            pty_cmd_tx,
            dsr_writer: dsr_writer_slot,
            _pty_master: pty_master,
            _pty_child: pty_child,
            shutdown,
            lazy_spawn: false,
            pinned_to_bottom: true,
            focus_terminal: false,
            selecting_in_terminal: false,
        }
    }

    /// Creates a terminal pane whose ConPTY shell is spawned lazily on first use via
    /// [`Self::ensure_started`], keeping cold editor startup fast and ensuring the shell
    /// inherits whichever project folder is active when the terminal is first opened.
    pub fn new_lazy() -> Self {
        let mut pane = Self::new_headless();
        pane.lazy_spawn = true;
        pane
    }

    /// Spawns the underlying ConPTY session if this pane was created with [`Self::new_lazy`]
    /// and has not started yet.
    pub fn ensure_started(&mut self, ctx: &egui::Context, working_dir: Option<&Path>) {
        if !self.lazy_spawn {
            return;
        }
        self.lazy_spawn = false;
        let next_revision = self.term_revision.wrapping_add(1);
        let (rows, cols) = (self.rows, self.cols);
        let focus = self.focus_terminal;
        *self = Self::new_with_size(ctx.clone(), working_dir, rows, cols);
        self.term_revision = next_revision;
        self.focus_terminal = focus;
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
            term_screen_text: PieceTable::new(),
            render_scratch: String::new(),
            cached_scrollback_len: 0,
            cached_scrollback_chars: 0,
            cached_first_sb_line: String::new(),
            cached_tail_sb_lines: Vec::new(),
            scrollback_edits_bytes: 0,
            term_revision: 0,
            term_cursor_idx: 0,
            rows: TERM_ROWS,
            cols: TERM_COLS,
            size_text: format!("{TERM_COLS}x{TERM_ROWS}"),
            shell_label: format_shell_label(&shell),
            pty_writer: None,
            pty_cmd_tx: None,
            dsr_writer: Arc::new(Mutex::new(None)),
            _pty_master: None,
            _pty_child: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            lazy_spawn: false,
            pinned_to_bottom: true,
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
            self.cached_scrollback_len = 0;
            self.cached_scrollback_chars = 0;
            self.cached_first_sb_line.clear();
            self.cached_tail_sb_lines.clear();
            self.scrollback_edits_bytes = 0;
            let mut lock = self.term.lock().unwrap_or_else(|e| e.into_inner());
            lock.parser.screen_mut().set_size(nz_rows, nz_cols);
            lock.pty_updated = true;
        }
    }

    /// Terminates the current PTY session (if any) and spawns a fresh terminal in `working_dir`
    /// while preserving the current grid dimensions and clearing any stale selection.
    pub fn restart(&mut self, ctx: &egui::Context, working_dir: Option<&Path>) {
        let next_revision = self.term_revision.wrapping_add(1);
        let (rows, cols) = (self.rows, self.cols);
        self.clear_terminal_selection(ctx);
        self.shutdown_session();
        *self = Self::new_with_size(ctx.clone(), working_dir, rows, cols);
        self.term_revision = next_revision;
        self.focus_terminal = true;
    }

    pub fn write_to_pty(&mut self, bytes: &[u8]) {
        if let Some(writer) = &self.pty_writer {
            let _ = writer.send(bytes.to_vec());
        } else if let Some(tx) = &self.pty_cmd_tx {
            let _ = tx.send(PtyWriteCmd::Bytes(bytes.to_vec()));
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
        self.pinned_to_bottom = true;
        let payload = encode_paste_bytes(text, self.is_bracketed_paste());
        if !payload.is_empty() {
            self.write_to_pty(&payload);
        }
        if self.pty_writer.is_none() {
            // Live ConPTY TUIs (e.g. agy / Ink / crossterm) can miss or absorb `\r`
            // if it arrives in the exact same pipe read as the bracketed-paste terminator.
            // Queue the delayed `\r` on the single FIFO PTY writer thread to preserve keystroke order.
            if let Some(tx) = &self.pty_cmd_tx {
                let _ = tx.send(PtyWriteCmd::DelayedBytes(
                    Duration::from_millis(50),
                    b"\r".to_vec(),
                ));
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
            let has_ctrl_c_key = i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::C,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.ctrl || modifiers.command
                )
            });
            let has_ctrl_x_key = i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::X,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.ctrl || modifiers.command
                )
            });
            // Pre-collect Alt+<letter> keys handled by `alt_key_byte` so any accompanying
            // printable `Event::Text` synthesized by winit in the same frame is not sent twice.
            let mut suppress_alt_text: Vec<u8> = i
                .events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.alt
                        && !modifiers.ctrl
                        && !modifiers.command
                        && encode_terminal_key(*key, *modifiers, application_cursor).is_none() =>
                    {
                        alt_key_byte(*key, modifiers.shift)
                    }
                    _ => None,
                })
                .collect();

            i.events.retain(|event| {
                match event {
                    egui::Event::Text(text) => {
                        if text.len() == 1 {
                            let ch = text.as_bytes()[0];
                            if let Some(pos) = suppress_alt_text
                                .iter()
                                .position(|&ab| ab.eq_ignore_ascii_case(&ch))
                            {
                                suppress_alt_text.remove(pos);
                                return false;
                            }
                        }
                        to_send.extend_from_slice(text.as_bytes());
                        clear_selection = true;
                        false
                    }
                    egui::Event::Paste(text) => {
                        to_send.extend_from_slice(&encode_paste_bytes(text, bracketed_paste));
                        clear_selection = true;
                        false
                    }
                    egui::Event::Cut => {
                        if i.modifiers.shift && !i.modifiers.ctrl && !i.modifiers.command {
                            if let Some(bytes) = encode_terminal_key(
                                egui::Key::Delete,
                                i.modifiers,
                                application_cursor,
                            ) {
                                to_send.extend_from_slice(&bytes);
                                clear_selection = true;
                            }
                        } else if !has_ctrl_x_key {
                            // `egui-winit` translates Ctrl+X into Event::Cut without Event::Key.
                            to_send.push(0x18);
                            clear_selection = true;
                        }
                        false
                    }
                    egui::Event::Copy => {
                        if self.selecting_in_terminal {
                            true
                        } else {
                            // `egui-winit` translates Ctrl+C into Event::Copy without Event::Key;
                            // forward ^C (0x03) unless a synthetic Event::Key(Ctrl+C) is also queued.
                            if !has_ctrl_c_key {
                                to_send.push(0x03);
                                clear_selection = true;
                            }
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
                        } else if modifiers.alt && !modifiers.ctrl {
                            if let Some(alt_byte) = alt_key_byte(*key, modifiers.shift) {
                                to_send.push(0x1b);
                                to_send.push(alt_byte);
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
            self.pinned_to_bottom = true;
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

        let was_pinned = self.pinned_to_bottom;
        self.handle_terminal_input(ctx, term_focused);
        let user_sent_input = !was_pinned && self.pinned_to_bottom;

        let pty_updated = {
            let term = Arc::clone(&self.term);
            let mut lock = term.lock().unwrap_or_else(|e| e.into_inner());
            let updated = lock.pty_updated || self.term_screen_text.is_empty();
            if updated {
                lock.pty_updated = false;
                self.sync_screen_buffer(lock.parser.screen_mut());
                self.term_revision = self.term_revision.wrapping_add(1);
                true
            } else {
                false
            }
        };

        let mut requested_scroll = false;
        if pty_updated || user_sent_input {
            let should_scroll =
                user_sent_input || (self.pinned_to_bottom && !self.selecting_in_terminal);
            ctx.data_mut(|d| {
                let mut state = d.get_temp::<FastEditorState>(term_id).unwrap_or_default();
                let target_range =
                    egui::text::CCursorRange::one(egui::text::CCursor::new(self.term_cursor_idx));
                let cursor_changed = state.cursor.char_range() != Some(target_range);
                if cursor_changed {
                    state.cursor.set_char_range(Some(target_range));
                }
                if should_scroll && (cursor_changed || user_sent_input) {
                    state.scroll_to_cursor = true;
                    requested_scroll = true;
                }
                d.insert_temp(term_id, state);
            });
        }

        let screen_text = &mut self.term_screen_text;
        let term_revision = self.term_revision;
        let focus_terminal = &mut self.focus_terminal;
        let shell_label = self.shell_label.as_str();
        let mut target_grid: Option<(u16, u16)> = None;
        let mut next_pinned_to_bottom = self.pinned_to_bottom;

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
            let inner_h =
                (available_size.y - crate::editor::EDITOR_MARGIN.y * 2.0).max(row_height * 2.0);
            let new_rows = (inner_h / row_height).floor().clamp(2.0, 200.0) as u16;

            let scroll_out = egui::ScrollArea::vertical()
                .id_salt("terminal_scroll_area")
                .auto_shrink([false, false])
                .animated(false)
                .show(ui, |ui| {
                    let editor_w = ui.available_width();
                    let inner_w = (editor_w - crate::editor::EDITOR_MARGIN.x * 2.0)
                        .max(char_width * 10.0);
                    let new_cols = (inner_w / char_width).floor().clamp(10.0, 500.0) as u16;
                    target_grid = Some((new_rows, new_cols));

                    let min_size = egui::vec2(editor_w, available_size.y);
                    let output = FastEditor::new(term_id, screen_text, min_size, &mut layouter)
                        .text_revision(term_revision)
                        .virtualized(true)
                        .pointer_moves_caret(false)
                        .read_only(true)
                        .accessible_label(shell_label)
                        .show(ui);

                    if *focus_terminal {
                        crate::editor::request_editor_focus(ui.ctx(), output.response.id);
                        *focus_terminal = false;
                    }
                });

            if !requested_scroll {
                let max_scroll_y =
                    (scroll_out.content_size.y - scroll_out.inner_rect.height()).max(0.0);
                next_pinned_to_bottom =
                    scroll_out.state.clamped_offset().y >= max_scroll_y - row_height * 2.0;
            }
        });

        self.pinned_to_bottom = next_pinned_to_bottom;

        if let Some((new_rows, new_cols)) = target_grid {
            let prev = (self.rows, self.cols);
            self.resize(new_rows, new_cols);
            if (self.rows, self.cols) != prev {
                ctx.request_repaint();
            }
        }
    }

    /// Updates `term_screen_text` and `term_cursor_idx` from `screen`, incrementally splicing
    /// only newly scrolled lines and the active viewport rows when existing scrollback is intact.
    fn sync_screen_buffer(&mut self, screen: &mut vt100::Screen) {
        let cols_u16 = screen.size().1.get();
        let rows_usize = screen.size().0.get() as usize;
        let sb_len = if screen.alternate_screen() {
            0
        } else {
            screen.scrollback_len()
        };

        if sb_len == 0 {
            self.term_cursor_idx = render_vt100_screen_into(screen, &mut self.render_scratch);
            self.term_screen_text.replace_with(&self.render_scratch);
            self.cached_scrollback_len = 0;
            self.cached_scrollback_chars = 0;
            self.cached_first_sb_line.clear();
            self.cached_tail_sb_lines.clear();
            self.scrollback_edits_bytes = 0;
            return;
        }

        // Growing scrollback (`sb_len < TERM_SCROLLBACK_LEN`): verify oldest scrollback row
        // matches so an inline `\x1b[3J` (clear scrollback) cannot be mistaken for an append.
        let can_incremental_grow = !self.term_screen_text.is_empty()
            && self.cached_scrollback_len > 0
            && sb_len >= self.cached_scrollback_len
            && sb_len < TERM_SCROLLBACK_LEN
            && {
                screen.set_scrollback(sb_len);
                let first_matches = screen
                    .rows(0, cols_u16)
                    .next()
                    .is_some_and(|first| first == self.cached_first_sb_line);
                screen.set_scrollback(0);
                first_matches
            };

        if can_incremental_grow {
            let delta = sb_len - self.cached_scrollback_len;
            self.render_scratch.clear();
            let mut delta_chars = 0usize;
            let new_lines = read_recent_scrollback_lines(screen, delta, cols_u16, rows_usize);
            for line in &new_lines {
                let lc = line
                    .as_bytes()
                    .iter()
                    .filter(|&&b| (b & 0xC0) != 0x80)
                    .count();
                self.render_scratch.push_str(line);
                self.render_scratch.push('\n');
                delta_chars += lc + 1;
            }
            if !new_lines.is_empty() {
                append_tail_anchor(&mut self.cached_tail_sb_lines, &new_lines);
            }
            let new_sb_chars = self.cached_scrollback_chars + delta_chars;
            self.term_cursor_idx =
                render_vt100_screen_append(screen, &mut self.render_scratch, new_sb_chars);

            let old_total = self.term_screen_text.char_count();
            self.term_screen_text
                .delete(self.cached_scrollback_chars..old_total);
            self.term_screen_text
                .insert(self.cached_scrollback_chars, &self.render_scratch);
            self.cached_scrollback_len = sb_len;
            self.cached_scrollback_chars = new_sb_chars;
            self.scrollback_edits_bytes += self.render_scratch.len();
            if self.scrollback_edits_bytes > 65_536 {
                let compacted = self.term_screen_text.build_string();
                self.term_screen_text.replace_with(&compacted);
                self.scrollback_edits_bytes = 0;
            }
            return;
        }

        // Full-capacity ring-buffer scrollback (`sb_len == TERM_SCROLLBACK_LEN`): detect
        // in-place updates (`evicted == 0`) or small ring-buffer shifts (`evicted = 1..=probe`)
        // by matching both the cached tail anchor and the shifted oldest line in `term_screen_text`.
        if !self.term_screen_text.is_empty()
            && self.cached_scrollback_len == TERM_SCROLLBACK_LEN
            && sb_len == TERM_SCROLLBACK_LEN
            && !self.cached_tail_sb_lines.is_empty()
        {
            screen.set_scrollback(sb_len);
            let first_now = screen.rows(0, cols_u16).next();
            screen.set_scrollback(0);

            if let Some(first_now) = first_now {
                let anchor_len = self.cached_tail_sb_lines.len();
                let probe_count = (rows_usize.max(32) + anchor_len).min(sb_len);
                let recent =
                    read_recent_scrollback_lines(screen, probe_count, cols_u16, rows_usize);
                if recent.len() >= anchor_len {
                    let max_shift = recent.len() - anchor_len;
                    let mut verified_shift = None;
                    for k in 0..=max_shift {
                        let start = recent.len() - anchor_len - k;
                        if recent[start..start + anchor_len] == self.cached_tail_sb_lines[..] {
                            let head_matches = if k == 0 {
                                first_now == self.cached_first_sb_line
                            } else {
                                let range = self.term_screen_text.line_char_range(k);
                                self.term_screen_text.slice_chars(range) == first_now
                            };
                            if head_matches {
                                verified_shift = Some(k);
                                break;
                            }
                        }
                    }

                    if let Some(k) = verified_shift {
                        if k > 0 {
                            let drop_chars = self.term_screen_text.line_to_char(k);
                            self.term_screen_text.delete(0..drop_chars);
                            self.cached_scrollback_chars -= drop_chars;
                            self.cached_first_sb_line = first_now;
                        }
                        self.render_scratch.clear();
                        let mut delta_chars = 0usize;
                        for line in &recent[recent.len() - k..] {
                            let lc = line
                                .as_bytes()
                                .iter()
                                .filter(|&&b| (b & 0xC0) != 0x80)
                                .count();
                            self.render_scratch.push_str(line);
                            self.render_scratch.push('\n');
                            delta_chars += lc + 1;
                        }
                        let new_sb_chars = self.cached_scrollback_chars + delta_chars;
                        self.term_cursor_idx = render_vt100_screen_append(
                            screen,
                            &mut self.render_scratch,
                            new_sb_chars,
                        );
                        let old_total = self.term_screen_text.char_count();
                        self.term_screen_text
                            .delete(self.cached_scrollback_chars..old_total);
                        self.term_screen_text
                            .insert(self.cached_scrollback_chars, &self.render_scratch);
                        self.cached_scrollback_chars = new_sb_chars;
                        let tail_start = recent.len().saturating_sub(TAIL_ANCHOR_LINES);
                        self.cached_tail_sb_lines.clear();
                        self.cached_tail_sb_lines
                            .extend_from_slice(&recent[tail_start..]);
                        self.scrollback_edits_bytes += self.render_scratch.len();
                        if self.scrollback_edits_bytes > 65_536 {
                            let compacted = self.term_screen_text.build_string();
                            self.term_screen_text.replace_with(&compacted);
                            self.scrollback_edits_bytes = 0;
                        }
                        return;
                    }
                }
            }
        }

        let (cursor_idx, actual_sb_len, sb_chars, first_line, tail_lines) =
            render_vt100_screen_with_scrollback_meta(screen, &mut self.render_scratch);
        self.term_cursor_idx = cursor_idx;
        self.term_screen_text.replace_with(&self.render_scratch);
        self.cached_scrollback_len = actual_sb_len;
        self.cached_scrollback_chars = sb_chars;
        self.cached_first_sb_line = first_line;
        self.cached_tail_sb_lines = tail_lines;
        self.scrollback_edits_bytes = 0;
    }
}

fn read_recent_scrollback_lines(
    screen: &mut vt100::Screen,
    count: usize,
    cols: u16,
    rows: usize,
) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut rem = count;
    while rem > 0 {
        screen.set_scrollback(rem);
        let take = rem.min(rows);
        for line in screen.rows(0, cols).take(take) {
            out.push(line);
        }
        rem -= take;
    }
    if count > 0 {
        screen.set_scrollback(0);
    }
    out
}

fn append_tail_anchor(anchor: &mut Vec<String>, new_lines: &[String]) {
    if new_lines.len() >= TAIL_ANCHOR_LINES {
        anchor.clear();
        anchor.extend_from_slice(&new_lines[new_lines.len() - TAIL_ANCHOR_LINES..]);
    } else {
        let total = anchor.len() + new_lines.len();
        if total > TAIL_ANCHOR_LINES {
            anchor.drain(..total - TAIL_ANCHOR_LINES);
        }
        anchor.extend_from_slice(new_lines);
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
    full_text.clear();
    full_text.reserve((rows.get() as usize) * (cols.get() as usize + 1));
    render_vt100_screen_append(screen, full_text, 0)
}

fn render_vt100_screen_append(
    screen: &vt100::Screen,
    full_text: &mut String,
    prefix_chars: usize,
) -> usize {
    let (rows, cols) = screen.size();
    let rows = rows.get();
    let cols = cols.get();
    let (cur_row, cur_col) = screen.cursor_position();

    let mut cursor_char_idx = prefix_chars;
    let mut total_chars = prefix_chars;

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
    render_vt100_screen_with_scrollback_meta(screen, full_text).0
}

fn render_vt100_screen_with_scrollback_meta(
    screen: &mut vt100::Screen,
    full_text: &mut String,
) -> (usize, usize, usize, String, Vec<String>) {
    let (rows, cols) = screen.size();
    let rows_usize = rows.get() as usize;
    let cols_u16 = cols.get();

    let scrollback_len = if screen.alternate_screen() {
        0
    } else {
        screen.scrollback_len()
    };
    if scrollback_len == 0 {
        screen.set_scrollback(0);
        let cursor = render_vt100_screen_into(screen, full_text);
        return (cursor, 0, 0, String::new(), Vec::new());
    }

    full_text.clear();
    full_text.reserve((scrollback_len + rows_usize) * (cols_u16 as usize + 1));
    let mut prefix_chars = 0usize;
    let mut first_line = Option::<String>::None;
    let mut tail_lines = Vec::with_capacity(TAIL_ANCHOR_LINES);
    let mut written_lines = 0usize;
    let tail_start_idx = scrollback_len.saturating_sub(TAIL_ANCHOR_LINES);

    let mut remaining = scrollback_len;
    while remaining > 0 {
        screen.set_scrollback(remaining);
        let take = remaining.min(rows_usize);
        for line in screen.rows(0, cols_u16).take(take) {
            if first_line.is_none() {
                first_line = Some(line.clone());
            }
            if written_lines >= tail_start_idx {
                tail_lines.push(line.clone());
            }
            written_lines += 1;
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
    let cursor = render_vt100_screen_append(screen, full_text, prefix_chars);
    (
        cursor,
        scrollback_len,
        prefix_chars,
        first_line.unwrap_or_default(),
        tail_lines,
    )
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
        egui::Key::Enter => {
            if modifiers.alt {
                Some(b"\x1b\r".to_vec())
            } else if modifiers.shift && !modifiers.ctrl {
                Some(b"\x1b[13;2u".to_vec())
            } else {
                Some(b"\r".to_vec())
            }
        }
        egui::Key::Backspace => {
            if modifiers.alt {
                Some(b"\x1b\x7f".to_vec())
            } else {
                Some(b"\x7f".to_vec())
            }
        }
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

fn alt_key_byte(key: egui::Key, shift: bool) -> Option<u8> {
    let base = ctrl_key_byte(key)?;
    Some(if shift {
        b'A' + (base - 1)
    } else {
        b'a' + (base - 1)
    })
}
