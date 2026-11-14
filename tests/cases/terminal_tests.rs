//! Unit tests for the `terminal` module.
//!
//! Covers VT100 screen buffer rendering, cursor position calculation and padding,
//! ANSI escape code processing, and keyboard/control-sequence input mapping.

use eframe::egui;
use rusted::{
    editor::FastEditorState,
    terminal::{render_vt100_screen, TerminalPane},
};
use std::sync::mpsc;

#[test]
fn test_render_vt100_screen_plain_text_and_cursor() {
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(5).unwrap(),
        std::num::NonZeroU16::new(20).unwrap(),
        0,
    );

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
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(5).unwrap(),
        std::num::NonZeroU16::new(40).unwrap(),
        0,
    );

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
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(5).unwrap(),
        std::num::NonZeroU16::new(40).unwrap(),
        0,
    );

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
        ctx.run_ui(raw, |ui| {
            pane.handle_terminal_input(ui.ctx(), focused);
        })
        .textures_delta
        .clear();
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

    // 8. Shift+ArrowLeft does NOT enter buffer selection mode; it forwards to the PTY
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
    assert!(
        !pane.selecting_in_terminal,
        "Shift+Arrow should not activate buffer selection mode"
    );
    assert_eq!(
        &take_sent(),
        b"\x1b[D",
        "Shift+Arrow should forward to PTY instead of selecting in the buffer"
    );
}

#[cfg(windows)]
#[test]
fn test_terminal_pane_new_starts_in_working_dir() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_term_cwd_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let pane = TerminalPane::new(ctx.clone(), Some(&temp_dir));

    // Wait up to 3 seconds for cmd.exe to spawn and print its initial prompt
    let folder_name = temp_dir.file_name().unwrap().to_string_lossy().to_string();
    let mut prompt_found = false;
    for _ in 0..60 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let lock = pane.term.lock().unwrap();
        let (screen_text, _) = render_vt100_screen(lock.parser.screen());
        if screen_text.contains(&folder_name) {
            prompt_found = true;
            break;
        }
    }

    assert!(
        prompt_found,
        "Terminal ConPTY should launch cmd.exe in the provided working directory"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[cfg(windows)]
#[test]
fn test_terminal_pane_restart_spawns_fresh_session() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_term_restart_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new(ctx.clone(), Some(&temp_dir));
    let folder_name = temp_dir.file_name().unwrap().to_string_lossy().to_string();

    let wait_for_prompt = |pane: &TerminalPane| -> bool {
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let lock = pane.term.lock().unwrap();
            let (screen_text, _) = render_vt100_screen(lock.parser.screen());
            if screen_text.contains(&folder_name) {
                return true;
            }
        }
        false
    };

    assert!(wait_for_prompt(&pane), "Initial terminal prompt should appear");

    // Exit the shell, then restart the terminal pane
    pane.write_to_pty(b"exit\r");
    std::thread::sleep(std::time::Duration::from_millis(200));

    let prev_revision = pane.term_revision;
    pane.restart(&ctx, Some(&temp_dir));

    assert!(pane.focus_terminal, "Restart should request focus on the terminal");
    assert!(
        pane.term_revision > prev_revision,
        "Restart should advance term_revision"
    );
    assert!(
        wait_for_prompt(&pane),
        "Restarted terminal should spawn a fresh cmd.exe prompt in the working directory"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_terminal_clicking_does_not_move_caret() {
    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    {
        let mut lock = pane.term.lock().unwrap();
        lock.parser.process(b"line one\r\nC:\\>hello");
        lock.pty_updated = true;
    }

    let term_id = egui::Id::unique("conpty_terminal");
    let run_frame = |pane: &mut TerminalPane, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            pane.show(ui, egui::Frame::default());
        });
        out.textures_delta.clear();
        out
    };

    // Initial frame renders the PTY buffer and sets the caret to the PTY cursor position.
    run_frame(&mut pane, Vec::new());
    let expected_cursor = pane.term_cursor_idx;
    assert!(
        expected_cursor > 9,
        "Expected PTY cursor on the second line, got {expected_cursor}"
    );

    let read_caret = || {
        ctx.data(|d| {
            d.get_temp::<FastEditorState>(term_id)
                .and_then(|s| s.cursor.char_range())
        })
    };
    assert_eq!(
        read_caret(),
        Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
            expected_cursor
        )))
    );

    // Left-click near the top-left of the terminal (on "line one", far from expected_cursor).
    let click_pos = egui::Pos2::new(12.0, 8.0);
    run_frame(
        &mut pane,
        vec![
            egui::Event::PointerMoved(click_pos),
            egui::Event::PointerButton {
                pos: click_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    run_frame(
        &mut pane,
        vec![egui::Event::PointerButton {
            pos: click_pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );

    assert!(
        ctx.memory(|m| m.has_focus(term_id)),
        "Left-clicking the terminal should focus it"
    );
    assert_eq!(
        read_caret(),
        Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
            expected_cursor
        ))),
        "Left-clicking in the terminal should not move the caret"
    );

    // Right-click near the top-left of the terminal.
    run_frame(
        &mut pane,
        vec![
            egui::Event::PointerMoved(click_pos),
            egui::Event::PointerButton {
                pos: click_pos,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    run_frame(
        &mut pane,
        vec![egui::Event::PointerButton {
            pos: click_pos,
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );

    assert_eq!(
        read_caret(),
        Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
            expected_cursor
        ))),
        "Right-clicking in the terminal should not move the caret"
    );
}

#[test]
fn test_terminal_mouse_drag_select_and_copy_without_moving_caret() {
    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    let (tx, rx) = mpsc::channel();
    pane.pty_writer = Some(tx);
    {
        let mut lock = pane.term.lock().unwrap();
        lock.parser.process(b"hello world\r\nC:\\>cmd");
        lock.pty_updated = true;
    }

    let term_id = egui::Id::unique("conpty_terminal");
    let run_frame = |pane: &mut TerminalPane, events: Vec<egui::Event>| {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(800.0, 600.0),
            )),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            pane.show(ui, egui::Frame::default());
        });
        out.textures_delta.clear();
        out
    };

    run_frame(&mut pane, Vec::new());
    let expected_cursor = pane.term_cursor_idx;

    // Drag across the first line ("hello world")
    let start_pos = egui::Pos2::new(8.0, 8.0);
    let end_pos = egui::Pos2::new(120.0, 8.0);
    run_frame(
        &mut pane,
        vec![
            egui::Event::PointerMoved(start_pos),
            egui::Event::PointerButton {
                pos: start_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    run_frame(
        &mut pane,
        vec![egui::Event::PointerMoved(end_pos)],
    );
    run_frame(
        &mut pane,
        vec![egui::Event::PointerButton {
            pos: end_pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    );

    let (caret_after_drag, mouse_sel) = ctx.data(|d| {
        let st = d.get_temp::<FastEditorState>(term_id).unwrap();
        (st.cursor.char_range(), st.mouse_selection)
    });
    assert_eq!(
        caret_after_drag,
        Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
            expected_cursor
        ))),
        "Dragging to select text must not move the terminal caret"
    );
    assert!(
        mouse_sel.is_some_and(|r| r.primary != r.secondary),
        "Dragging with primary button should create a non-empty mouse selection"
    );

    // Ctrl+C (Event::Copy + Key::C) should copy the selection and NOT send 0x03 to PTY
    let out = run_frame(
        &mut pane,
        vec![
            egui::Event::Copy,
            egui::Event::Key {
                key: egui::Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            },
        ],
    );
    let copied = out.platform_output.commands.iter().find_map(|cmd| {
        if let egui::OutputCommand::CopyText(text) = cmd {
            Some(text.clone())
        } else {
            None
        }
    });
    assert!(
        copied.as_deref().is_some_and(|s| s.contains("hello")),
        "Ctrl+C should copy the mouse-selected text, got {copied:?}"
    );
    assert!(
        rx.try_recv().is_err(),
        "Ctrl+C with an active selection should not send 0x03 to PTY"
    );

    // Right-clicking with an active selection should also copy it and clear selection without moving caret
    let out_rc = run_frame(
        &mut pane,
        vec![
            egui::Event::PointerMoved(start_pos),
            egui::Event::PointerButton {
                pos: start_pos,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: start_pos,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    let copied_rc = out_rc.platform_output.commands.iter().find_map(|cmd| {
        if let egui::OutputCommand::CopyText(text) = cmd {
            Some(text.clone())
        } else {
            None
        }
    });
    assert!(
        copied_rc.as_deref().is_some_and(|s| s.contains("hello")),
        "Right-click with selection should copy the selected text, got {copied_rc:?}"
    );
    let (caret_after_rc, sel_after_rc) = ctx.data(|d| {
        let st = d.get_temp::<FastEditorState>(term_id).unwrap();
        (st.cursor.char_range(), st.mouse_selection)
    });
    assert_eq!(
        caret_after_rc,
        Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
            expected_cursor
        ))),
        "Right-clicking to copy must not move the terminal caret"
    );
    assert!(
        sel_after_rc.is_none(),
        "Right-clicking to copy should clear the selection"
    );
}

#[test]
fn test_contains_dsr_request_across_chunks() {
    use rusted::terminal::contains_dsr_request;

    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;

    // Single chunk match
    assert!(contains_dsr_request(&mut tail, &mut tail_len, b"abc\x1b[6ndef"));

    // Split across two chunks (2 + 2 bytes)
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"prefix\x1b["));
    assert!(contains_dsr_request(&mut tail, &mut tail_len, b"6nsuffix"));

    // Split 1 byte at a time, including an empty intermediate chunk
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"\x1b"));
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b""));
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"["));
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"6"));
    assert!(contains_dsr_request(&mut tail, &mut tail_len, b"n"));

    // Non-matching byte breaks partial sequence
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"\x1b[6"));
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"m"));
    assert!(!contains_dsr_request(&mut tail, &mut tail_len, b"n"));
}

#[test]
fn test_terminal_resize_and_wide_char_cursor() {
    let mut pane = TerminalPane::new_headless();
    assert_eq!(pane.size_label(), "120x30");
    pane.resize(40, 100);
    assert_eq!(pane.size_label(), "100x40");
    assert_eq!(pane.rows, 40);
    assert_eq!(pane.cols, 100);

    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(4).unwrap(),
        std::num::NonZeroU16::new(20).unwrap(),
        0,
    );
    // Two wide chars ("你好") take 4 display columns, followed by "ab" -> cursor at cell col 6, char index 4.
    parser.process("你好ab".as_bytes());
    let (text, cursor_idx) = render_vt100_screen(parser.screen());
    assert!(text.starts_with("你好ab"));
    assert_eq!(cursor_idx, 4);
}
