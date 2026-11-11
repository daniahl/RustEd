//! Unit tests for the `terminal` module.
//!
//! Covers VT100 screen buffer rendering, cursor position calculation and padding,
//! ANSI escape code processing, and keyboard/control-sequence input mapping.

use eframe::egui;
use rusted::terminal::{render_vt100_screen, TerminalPane};
use std::sync::mpsc;

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

    let (tx, rx) = mpsc::channel();
    pane.pty_writer = Some(tx);

    let send_event = |pane: &mut TerminalPane, ctx: &egui::Context, event: egui::Event, focused: bool| {
        let mut raw = egui::RawInput::default();
        raw.events.push(event);
        let _ = ctx.run(raw, |ctx| {
            pane.handle_terminal_input(ctx, focused);
        });
    };

    let take_sent = || {
        let mut out = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
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
    assert!(take_sent().is_empty());

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
    assert_eq!(&take_sent(), b"\r");

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
    assert_eq!(&take_sent(), b"\x7f");

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
    assert_eq!(&take_sent(), b"\t");

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
    assert_eq!(&take_sent(), b"\x1b[A");

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
    assert_eq!(&take_sent(), b"\x03");

    // 7. Text input sends raw UTF-8 bytes
    send_event(&mut pane, &ctx, egui::Event::Text("cargo test".to_string()), true);
    assert_eq!(&take_sent(), b"cargo test");

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
    assert!(take_sent().is_empty(), "Shift+Arrow should not send bytes to PTY");

    // 9. Subsequent text input exits selection mode
    send_event(&mut pane, &ctx, egui::Event::Text("a".to_string()), true);
    assert!(!pane.selecting_in_terminal, "Typing text should exit selection mode");
    assert_eq!(&take_sent(), b"a");
}
