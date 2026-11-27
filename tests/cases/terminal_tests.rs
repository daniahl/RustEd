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

    // 8. Shift+ArrowLeft does NOT enter buffer selection mode; it forwards xterm Shift+Left to the PTY
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
        b"\x1b[1;2D",
        "Shift+Arrow should forward modified xterm sequence to PTY"
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
    use rusted::terminal::process_chunk_with_dsr;

    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(24).unwrap(),
        std::num::NonZeroU16::new(80).unwrap(),
        0,
    );
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;

    // Single chunk match
    assert!(!process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"abc\x1b[6ndef").is_empty());

    // Split across two chunks (2 + 2 bytes)
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"prefix\x1b[").is_empty());
    assert!(!process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"6nsuffix").is_empty());

    // Split 1 byte at a time, including an empty intermediate chunk
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"\x1b").is_empty());
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"").is_empty());
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"[").is_empty());
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"6").is_empty());
    assert!(!process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"n").is_empty());

    // Non-matching byte breaks partial sequence
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"\x1b[6").is_empty());
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"m").is_empty());
    assert!(process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"n").is_empty());
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

#[test]
fn test_encode_paste_bytes_and_paste_and_submit() {
    use rusted::terminal::encode_paste_bytes;

    // Plain mode normalizes CRLF / CR to LF
    assert_eq!(
        encode_paste_bytes("Explain a.rs:1-2: foo\r\nbar", false),
        b"Explain a.rs:1-2: foo\nbar"
    );

    // Bracketed paste mode wraps in ESC[200~ ... ESC[201~ and strips embedded ESC[201~
    assert_eq!(
        encode_paste_bytes("Explain a.rs:1-2: foo\r\n\x1b[201~bar", true),
        b"\x1b[200~Explain a.rs:1-2: foo\nbar\x1b[201~"
    );

    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    let (tx, rx) = mpsc::channel();
    pane.pty_writer = Some(tx);

    let take_sent = || {
        let mut out = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        out
    };

    // Bracketed paste disabled by default -> sends text then '\r'
    assert!(!pane.is_bracketed_paste());
    pane.paste_and_submit(&ctx, "Explain main.rs:1-1: let x = 1;");
    assert_eq!(take_sent(), b"Explain main.rs:1-1: let x = 1;\r");

    // Enable bracketed paste mode via DECSET 2004 -> wraps paste and then sends '\r'
    {
        let mut lock = pane.term.lock().unwrap();
        lock.parser.process(b"\x1b[?2004h");
    }
    assert!(pane.is_bracketed_paste());
    pane.paste_and_submit(&ctx, "Explain main.rs:1-2: a\r\nb");
    assert_eq!(
        take_sent(),
        b"\x1b[200~Explain main.rs:1-2: a\nb\x1b[201~\r"
    );
}

#[test]
fn test_terminal_scrollback_and_modified_key_encoding() {
    use rusted::terminal::{encode_terminal_key, render_vt100_screen_with_scrollback};

    // Scrollback history is preserved above the active viewport and cursor offset accounts for it
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(3).unwrap(),
        std::num::NonZeroU16::new(20).unwrap(),
        100,
    );
    parser.process(b"row0\r\nrow1\r\nrow2\r\nrow3\r\nrow4");
    let mut buf = String::new();
    let cursor = render_vt100_screen_with_scrollback(parser.screen_mut(), &mut buf);
    assert!(buf.starts_with("row0\nrow1\nrow2\nrow3\nrow4"));
    assert_eq!(cursor, "row0\nrow1\nrow2\nrow3\nrow4".chars().count());

    // Modified key sequences & DECCKM application cursor mode
    assert_eq!(
        encode_terminal_key(egui::Key::Tab, egui::Modifiers::SHIFT, false),
        Some(b"\x1b[Z".to_vec())
    );
    assert_eq!(
        encode_terminal_key(egui::Key::ArrowLeft, egui::Modifiers::CTRL, false),
        Some(b"\x1b[1;5D".to_vec())
    );
    assert_eq!(
        encode_terminal_key(egui::Key::ArrowRight, egui::Modifiers::CTRL, false),
        Some(b"\x1b[1;5C".to_vec())
    );
    assert_eq!(
        encode_terminal_key(egui::Key::ArrowUp, egui::Modifiers::NONE, true),
        Some(b"\x1bOA".to_vec())
    );

    // Ctrl+V paired with Event::Paste must NOT emit a trailing 0x16 (^V)
    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    let (tx, rx) = mpsc::channel();
    pane.pty_writer = Some(tx);
    let raw = egui::RawInput {
        events: vec![
            egui::Event::Paste("pasted".to_string()),
            egui::Event::Key {
                key: egui::Key::V,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            },
        ],
        ..Default::default()
    };
    ctx.run_ui(raw, |ui| pane.handle_terminal_input(ui.ctx(), true))
        .textures_delta
        .clear();
    let mut sent = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        sent.extend_from_slice(&chunk);
    }
    assert_eq!(sent, b"pasted");
}

#[test]
fn test_process_chunk_with_dsr_and_ai_cli_keys() {
    use rusted::terminal::{encode_terminal_key, process_chunk_with_dsr};

    // DSR probe followed by cursor restore in the SAME chunk must reply with the probed position
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(24).unwrap(),
        std::num::NonZeroU16::new(80).unwrap(),
        0,
    );
    let mut tail = [0u8; 3];
    let mut tail_len = 0usize;
    let replies = process_chunk_with_dsr(
        &mut parser,
        &mut tail,
        &mut tail_len,
        b"\x1b[999;999H\x1b[6n\x1b[1;1H\x1b[6n",
    );
    assert_eq!(replies, vec![b"\x1b[24;80R".to_vec(), b"\x1b[1;1R".to_vec()]);

    // Split across two chunks with a move right after
    let replies1 = process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"\x1b[5;10H\x1b[");
    assert!(replies1.is_empty());
    let replies2 = process_chunk_with_dsr(&mut parser, &mut tail, &mut tail_len, b"6n\x1b[1;1H");
    assert_eq!(replies2, vec![b"\x1b[5;10R".to_vec()]);

    // AI CLI modified Enter and Alt keys
    assert_eq!(
        encode_terminal_key(egui::Key::Enter, egui::Modifiers::SHIFT, false),
        Some(b"\x1b[13;2u".to_vec())
    );
    assert_eq!(
        encode_terminal_key(egui::Key::Enter, egui::Modifiers::ALT, false),
        Some(b"\x1b\r".to_vec())
    );
    assert_eq!(
        encode_terminal_key(egui::Key::Backspace, egui::Modifiers::ALT, false),
        Some(b"\x1b\x7f".to_vec())
    );

    // Alt+B sends ESC b via handle_terminal_input
    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    let (tx, rx) = mpsc::channel();
    pane.pty_writer = Some(tx);
    let raw = egui::RawInput {
        events: vec![egui::Event::Key {
            key: egui::Key::B,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::ALT,
        }],
        ..Default::default()
    };
    ctx.run_ui(raw, |ui| pane.handle_terminal_input(ui.ctx(), true))
        .textures_delta
        .clear();
    assert_eq!(rx.try_recv().unwrap(), b"\x1bb");
}

#[test]
fn test_winit_style_ctrl_c_ctrl_x_alt_text_dedup_and_incremental_scrollback() {
    use rusted::terminal::render_vt100_screen_with_scrollback;

    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    let (tx, rx) = mpsc::channel();
    pane.pty_writer = Some(tx);

    let drain = || {
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            out.extend_from_slice(&b);
        }
        out
    };

    // 1. Live `egui-winit` translates Ctrl+C into Event::Copy WITHOUT Event::Key(C)
    ctx.run_ui(
        egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(egui::Modifiers::CTRL),
                egui::Event::Copy,
            ],
            ..Default::default()
        },
        |ui| pane.handle_terminal_input(ui.ctx(), true),
    )
    .textures_delta
    .clear();
    assert_eq!(
        drain(),
        b"\x03",
        "Event::Copy without active selection must send ^C (0x03)"
    );

    // 2. Live `egui-winit` translates Ctrl+X into Event::Cut WITHOUT Event::Key(X)
    ctx.run_ui(
        egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(egui::Modifiers::CTRL),
                egui::Event::Cut,
            ],
            ..Default::default()
        },
        |ui| pane.handle_terminal_input(ui.ctx(), true),
    )
    .textures_delta
    .clear();
    assert_eq!(drain(), b"\x18", "Event::Cut with Ctrl must send ^X (0x18)");

    // 3. Windows Shift+Delete translates into Event::Cut with Modifiers::SHIFT
    ctx.run_ui(
        egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(egui::Modifiers::SHIFT),
                egui::Event::Cut,
            ],
            ..Default::default()
        },
        |ui| pane.handle_terminal_input(ui.ctx(), true),
    )
    .textures_delta
    .clear();
    assert_eq!(
        drain(),
        b"\x1b[3;2~",
        "Event::Cut with Shift must send xterm Shift+Delete"
    );

    // 4. Live `egui-winit` emits BOTH Event::Key(Alt+B) and Event::Text("b") on Alt+B
    ctx.run_ui(
        egui::RawInput {
            events: vec![
                egui::Event::ModifiersChanged(egui::Modifiers::ALT),
                egui::Event::Key {
                    key: egui::Key::B,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::ALT,
                },
                egui::Event::Text("b".to_string()),
            ],
            ..Default::default()
        },
        |ui| pane.handle_terminal_input(ui.ctx(), true),
    )
    .textures_delta
    .clear();
    assert_eq!(
        drain(),
        b"\x1bb",
        "Alt+B with accompanying Event::Text(\"b\") must not send duplicate 'b'"
    );

    // 5. Incremental scrollback sync matches full render across prompt edits, streaming, and ESC[3J
    let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(800.0, 400.0));
    let mut step_and_verify = |chunk: &[u8]| {
        {
            let mut lock = pane.term.lock().unwrap();
            lock.parser.process(chunk);
            lock.pty_updated = true;
        }
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| pane.show(ui, egui::Frame::default()),
        )
        .textures_delta
        .clear();

        let mut expected_text = String::new();
        let expected_cursor = {
            let mut lock = pane.term.lock().unwrap();
            render_vt100_screen_with_scrollback(lock.parser.screen_mut(), &mut expected_text)
        };
        assert_eq!(pane.term_screen_text.build_string(), expected_text);
        assert_eq!(pane.term_cursor_idx, expected_cursor);
    };

    // Push 60 lines to populate scrollback
    let initial_lines: String = (0..60).map(|i| format!("history line {i}\r\n")).collect();
    step_and_verify(initial_lines.as_bytes());
    // Edit in-place on active prompt without scrolling
    step_and_verify(b"C:\\Proj>cargo ");
    step_and_verify(b"test");
    // Stream 5 more lines into scrollback
    step_and_verify(b"\r\nout 1\r\nout 2\r\nout 3\r\nout 4\r\nout 5\r\nC:\\Proj>");
    // Clear scrollback (\x1b[3J) + clear screen (\x1b[2J\x1b[H) + fresh output
    step_and_verify(b"\x1b[3J\x1b[2J\x1b[Hfresh start\r\nsecond\r\n");
}

#[test]
fn test_incremental_scrollback_at_full_capacity_and_scroll_pinning() {
    use rusted::terminal::{render_vt100_screen_with_scrollback, TERM_SCROLLBACK_LEN};

    let ctx = egui::Context::default();
    let mut pane = TerminalPane::new_headless();
    let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(800.0, 400.0));

    let step_and_verify = |pane: &mut TerminalPane, chunk: &[u8]| {
        {
            let mut lock = pane.term.lock().unwrap();
            lock.parser.process(chunk);
            lock.pty_updated = true;
        }
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen_rect),
                ..Default::default()
            },
            |ui| pane.show(ui, egui::Frame::default()),
        )
        .textures_delta
        .clear();

        let mut expected_text = String::new();
        let expected_cursor = {
            let mut lock = pane.term.lock().unwrap();
            render_vt100_screen_with_scrollback(lock.parser.screen_mut(), &mut expected_text)
        };
        assert_eq!(pane.term_screen_text.build_string(), expected_text);
        assert_eq!(pane.term_cursor_idx, expected_cursor);
    };

    // Push past TERM_SCROLLBACK_LEN (2,000) so scrollback hits full ring-buffer capacity
    let bulk: String = (0..TERM_SCROLLBACK_LEN + 60)
        .map(|i| format!("full_sb_line_{i}\r\n"))
        .collect();
    step_and_verify(&mut pane, bulk.as_bytes());
    assert_eq!(
        pane.term.lock().unwrap().parser.screen().scrollback_len(),
        TERM_SCROLLBACK_LEN
    );

    // 1. In-place prompt edit at full capacity (evicted == 0)
    step_and_verify(&mut pane, b"prompt> cargo ");
    step_and_verify(&mut pane, b"check");

    // 2. Single-line and multi-line shifts at full capacity (evicted == 1, 5, 20)
    step_and_verify(&mut pane, b"\r\nshift_one_0\r\n");
    let five: String = (0..5).map(|i| format!("shift_five_{i}\r\n")).collect();
    step_and_verify(&mut pane, five.as_bytes());
    let twenty: String = (0..20).map(|i| format!("shift_twenty_{i}\r\n")).collect();
    step_and_verify(&mut pane, twenty.as_bytes());

    // 3. Clearing scrollback from full capacity recovers cleanly
    step_and_verify(&mut pane, b"\x1b[3J\x1b[2J\x1b[Hafter_clear\r\n");
}
