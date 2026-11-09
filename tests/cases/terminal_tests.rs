//! Unit tests for the `terminal` module.
//!
//! Covers VT100 screen buffer rendering, cursor position calculation and padding,
//! ANSI escape code processing, and keyboard/control-sequence input mapping.

use eframe::egui;
use rusted::terminal::{render_vt100_screen, TerminalPane};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

struct TestWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_render_vt100_screen_plain_text_and_cursor() {
    let mut parser = vt100::Parser::new(5, 20, 0);

    // Write text to parser: "Hello, World!"
    parser.process(b"Hello, World!");

    let (text, cursor_idx) = render_vt100_screen(parser.screen());

    // Cursor should be right after "Hello, World!" (13 characters)
    assert_eq!(cursor_idx, 13);
    let first_line = text.lines().next().unwrap();
    assert_eq!(first_line, "Hello, World!");
    assert_eq!(text.split('\n').count(), 5);
}

#[test]
fn test_render_vt100_screen_cursor_padding_past_line_end() {
    let mut parser = vt100::Parser::new(5, 40, 0);

    // Write "abc" then move cursor forward 10 positions via escape code \x1b[10C
    parser.process(b"abc\x1b[10C");

    let (text, cursor_idx) = render_vt100_screen(parser.screen());

    // Cursor is at column 13 (0-indexed). The row text should be padded with spaces up to col 13.
    assert_eq!(cursor_idx, 13);
    let first_line = text.lines().next().unwrap();
    assert_eq!(first_line.len(), 13);
    assert_eq!(&first_line[..3], "abc");
    assert_eq!(&first_line[3..], "          ");
}

#[test]
fn test_render_vt100_screen_ansi_codes_stripped() {
    let mut parser = vt100::Parser::new(5, 40, 0);

    // Process ANSI codes: red text, reset, bold text, reset
    parser.process(b"\x1b[31mRed\x1b[0m \x1b[1mBold\x1b[0m");

    let (text, _) = render_vt100_screen(parser.screen());
    let first_line = text.lines().next().unwrap();
    assert_eq!(first_line, "Red Bold");
}

#[test]
fn test_handle_terminal_input_key_mappings() {
    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();

    let sent_bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
    pane.pty_writer = Some(Box::new(TestWriter(Arc::clone(&sent_bytes))));

    let send_event = |pane: &mut TerminalPane, ctx: &egui::Context, event: egui::Event, focused: bool| {
        let mut raw = egui::RawInput::default();
        raw.events.push(event);
        let _ = ctx.run(raw, |ctx| {
            pane.handle_terminal_input(ctx, focused);
        });
    };

    // 1. Not focused -> nothing sent
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        false,
    );
    assert!(sent_bytes.lock().unwrap().is_empty());

    // 2. Focused -> Enter key sends '\r'
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        true,
    );
    assert_eq!(&*sent_bytes.lock().unwrap(), b"\r");
    sent_bytes.lock().unwrap().clear();

    // 3. Backspace sends 0x7F
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::Backspace,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        true,
    );
    assert_eq!(&*sent_bytes.lock().unwrap(), b"\x7f");
    sent_bytes.lock().unwrap().clear();

    // 4. Tab sends '\t'
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        true,
    );
    assert_eq!(&*sent_bytes.lock().unwrap(), b"\t");
    sent_bytes.lock().unwrap().clear();

    // 5. Arrow keys send ANSI escape sequences
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::ArrowUp,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        true,
    );
    assert_eq!(&*sent_bytes.lock().unwrap(), b"\x1b[A");
    sent_bytes.lock().unwrap().clear();

    // 6. Ctrl+C sends 0x03
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::C,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        },
        true,
    );
    assert_eq!(&*sent_bytes.lock().unwrap(), b"\x03");
    sent_bytes.lock().unwrap().clear();

    // 7. Text input sends raw UTF-8 bytes
    send_event(&mut pane, &ctx, egui::Event::Text("cargo test".to_string()), true);
    assert_eq!(&*sent_bytes.lock().unwrap(), b"cargo test");
    sent_bytes.lock().unwrap().clear();

    // 8. Shift+ArrowLeft enters selection mode without sending bytes
    assert!(!pane.selecting_in_terminal);
    send_event(
        &mut pane,
        &ctx,
        egui::Event::Key {
            key: egui::Key::ArrowLeft,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        },
        true,
    );
    assert!(pane.selecting_in_terminal, "Shift+Arrow should activate selection mode");
    assert!(sent_bytes.lock().unwrap().is_empty(), "Shift+Arrow should not send bytes to PTY");

    // 9. Subsequent text input exits selection mode
    send_event(&mut pane, &ctx, egui::Event::Text("a".to_string()), true);
    assert!(!pane.selecting_in_terminal, "Typing text should exit selection mode");
    assert_eq!(&*sent_bytes.lock().unwrap(), b"a");
}
