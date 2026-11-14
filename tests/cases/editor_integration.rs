//! Integration tests for the `FastEditor` widget.
//!
//! Validates text editing lifecycle, AccessKit node hierarchy, cursor navigation,
//! clipboard paste scrolling, and layout caching.

use eframe::egui;
use egui::{epaint::text::LayoutJob, Color32, Event, Id, Key, Modifiers, Ui, Vec2};

use rusted::editor::FastEditor;

#[test]
fn test_editor_full_lifecycle() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut text = String::new();
    let id = Id::unique("test_editor");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.global_style());

    let run_frame = |ctx: &egui::Context, text: &mut String, raw_input: egui::RawInput| {
        let font_id = font_id.clone();
        let mut out = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    let job = LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let editor = FastEditor::new(id, text, Vec2::new(100.0, 100.0), &mut layouter);
                let output = editor.show(ui);
                output.response.request_focus();
            });
        });
        out.textures_delta.clear();
        out
    };

    // Frame 1: Empty text, focus
    let out = run_frame(&ctx, &mut text, Default::default());
    let u = out.platform_output.accesskit_update.unwrap();
    assert!(u.nodes.iter().any(|(_, n)| n.role() == egui::accesskit::Role::Window));
    assert!(u.nodes.iter().any(|(_, n)| n.role() == egui::accesskit::Role::MultilineTextInput));
    assert!(u.nodes.iter().any(|(_, n)| n.role() == egui::accesskit::Role::TextRun));

    // Frame 2: Type "First Line"
    let mut input = egui::RawInput::default();
    input.events.push(Event::Text("First Line".to_string()));
    let out = run_frame(&ctx, &mut text, input);
    assert_eq!(text, "First Line");
    let u = out.platform_output.accesskit_update.unwrap();
    println!("Frame 2 nodes (count={}):", u.nodes.len());
    for (id, node) in &u.nodes {
        println!("  id={:?}, role={:?}, children={:?}", id, node.role(), node.children());
    }

    // Verify tree has no duplicate children
    let mut seen = std::collections::HashSet::new();
    for (_, node) in &u.nodes {
        for &child in node.children() {
            assert!(seen.insert(child), "Duplicate child in tree!");
        }
    }

    // Frame 3: Press Enter
    let mut input_enter = egui::RawInput::default();
    input_enter.events.push(Event::Key {
        key: Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let out = run_frame(&ctx, &mut text, input_enter);
    assert_eq!(text, "First Line\n");
    let u = out.platform_output.accesskit_update.unwrap();
    let mut seen = std::collections::HashSet::new();
    for (_, node) in &u.nodes {
        for &child in node.children() {
            assert!(seen.insert(child), "Duplicate child in tree!");
        }
    }

    // Frame 4: Type "Second Line" at time = 1.0s
    let input = egui::RawInput {
        time: Some(1.0),
        events: vec![Event::Text("Second Line".to_string())],
        ..Default::default()
    };
    let _ = run_frame(&ctx, &mut text, input);
    assert_eq!(text, "First Line\nSecond Line");

    // Frame 4b: Idle frame at time = 2.5s (> 1.0s stable_time) to stabilize undo point
    let input_idle = egui::RawInput {
        time: Some(2.5),
        ..Default::default()
    };
    let _ = run_frame(&ctx, &mut text, input_idle);

    // Frame 5: Press Backspace at time = 3.0s
    let input_bs = egui::RawInput {
        time: Some(3.0),
        events: vec![Event::Key {
            key: Key::Backspace,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
        ..Default::default()
    };
    let _ = run_frame(&ctx, &mut text, input_bs);
    assert_eq!(text, "First Line\nSecond Lin");

    // Frame 6: Undo (Ctrl+Z) at time = 3.5s
    let input_undo = egui::RawInput {
        time: Some(3.5),
        events: vec![Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        }],
        ..Default::default()
    };
    let _ = run_frame(&ctx, &mut text, input_undo);
    assert_eq!(text, "First Line\nSecond Line");

    // Frame 7: Ctrl+C without selection (should be a no-op, no hang)
    let mut input_copy = egui::RawInput::default();
    input_copy.events.push(Event::Key {
        key: Key::C,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    input_copy.events.push(Event::Copy);
    let _ = run_frame(&ctx, &mut text, input_copy);

    // Frame 8: Select all (Ctrl+A) then Ctrl+C with selection (copies text without deadlocking)
    let mut input_select_copy = egui::RawInput::default();
    input_select_copy.events.push(Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    input_select_copy.events.push(Event::Copy);
    let out_copy = run_frame(&ctx, &mut text, input_select_copy);
    assert!(out_copy.platform_output.commands.iter().any(|cmd| {
        matches!(cmd, egui::OutputCommand::CopyText(t) if t == "First Line\nSecond Line")
    }));

    println!("Full lifecycle test passed successfully!");
}

#[test]
fn test_arrow_keys() {
    let ctx = egui::Context::default();

    let mut text = "Line 1\nLine 2".to_string();
    let id = Id::unique("test_editor_arrows");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.global_style());

    let run_frame = |ctx: &egui::Context, text: &mut String, raw_input: egui::RawInput| {
        let font_id = font_id.clone();
        let mut last_cursor = None;
        let mut last_focus = false;
        let mut full_output = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    let job = LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let editor = FastEditor::new(id, text, Vec2::new(100.0, 100.0), &mut layouter);
                let output = editor.show(ui);
                if output.response.clicked() || !output.response.has_focus() {
                    output.response.request_focus();
                }
                last_cursor = output.cursor_range;
                last_focus = output.response.has_focus();
            });
        });
        full_output.textures_delta.clear();
        (last_cursor, last_focus, full_output.platform_output.cursor_icon)
    };

    // Frame 1: Request focus
    let out = run_frame(&ctx, &mut text, Default::default());
    assert!(out.1, "Editor should have focus");

    // Frame 2: Check hover cursor
    let mut hover_input = egui::RawInput::default();
    hover_input.events.push(Event::PointerMoved(egui::pos2(50.0, 50.0)));
    let out = run_frame(&ctx, &mut text, hover_input);
    assert_eq!(out.2, egui::CursorIcon::Text, "Cursor should be Text icon on hover");

    // Frame 3: Press ArrowRight 7 times -> should land on row 1, column 0
    let mut last_out = out;
    for _ in 1..=7 {
        let mut input = egui::RawInput::default();
        input.events.push(Event::Key {
            key: Key::ArrowRight,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
        last_out = run_frame(&ctx, &mut text, input);
    }
    let row_col = |idx: usize, text: &str| {
        let mut row = 0;
        let mut col = 0;
        for ch in text.chars().take(idx) {
            if ch == '\n' {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (row, col)
    };
    let cur = last_out.0.unwrap().primary;
    assert_eq!(row_col(cur.index.0, &text), (1, 0));

    // Frame 4: Press ArrowLeft -> should land on row 0, column 6
    let mut input = egui::RawInput::default();
    input.events.push(Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let out = run_frame(&ctx, &mut text, input);
    let cur = out.0.unwrap().primary;
    assert_eq!(row_col(cur.index.0, &text), (0, 6));

    // Frame 5: Press ArrowDown -> should land on row 1, column 6
    let mut input = egui::RawInput::default();
    input.events.push(Event::Key {
        key: Key::ArrowDown,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let out = run_frame(&ctx, &mut text, input);
    let cur = out.0.unwrap().primary;
    assert_eq!(row_col(cur.index.0, &text), (1, 6));

    // Frame 6: Press ArrowUp -> should land back on row 0, column 6
    let mut input = egui::RawInput::default();
    input.events.push(Event::Key {
        key: Key::ArrowUp,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let out = run_frame(&ctx, &mut text, input);
    let cur = out.0.unwrap().primary;
    assert_eq!(row_col(cur.index.0, &text), (0, 6));
}

#[test]
fn test_paste_scrolls_to_caret() {
    let ctx = egui::Context::default();
    let mut text = String::new();
    let id = Id::unique("test_editor_paste");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.global_style());
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(400.0, 300.0));

    let mut caret_visible = false;
    let mut run_frame = |ctx: &egui::Context, text: &mut String, events: Vec<Event>| {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let font_id = font_id.clone();
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .animated(false)
                    .show(ui, |ui| {
                        let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                            let job = LayoutJob::simple(
                                text.to_owned(),
                                font_id.clone(),
                                Color32::WHITE,
                                wrap_width,
                            );
                            ui.fonts_mut(|f| f.layout_job(job))
                        };
                        let editor = FastEditor::new(id, text, Vec2::new(100.0, 100.0), &mut layouter);
                        let output = editor.show(ui);
                        output.response.request_focus();
                        if let Some(c) = output.cursor_range {
                            let r = output
                                .galley
                                .pos_from_cursor(c.primary)
                                .translate(output.galley_pos.to_vec2());
                            caret_visible = ui.clip_rect().contains(r.center());
                        }
                    });
            });
        });
        out.textures_delta.clear();
    };

    run_frame(&ctx, &mut text, vec![]);
    let big: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    run_frame(&ctx, &mut text, vec![Event::Paste(big.clone())]);
    for _ in 0..5 {
        run_frame(&ctx, &mut text, vec![]);
    }
    assert_eq!(text, big);
    assert!(caret_visible, "caret should be scrolled into view after a big paste");
}

#[test]
fn test_coalesced_relayout_and_long_line_accesskit() {
    let ctx = egui::Context::default();
    ctx.options_mut(|o| {
        o.max_passes = std::num::NonZeroUsize::new(2).unwrap();
    });
    ctx.enable_accesskit();

    let mut text = String::new();
    let mut rev = 1u64;
    let id = Id::unique("test_editor_perf");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.global_style());
    let layout_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let run_frame = |ctx: &egui::Context,
                     text: &mut String,
                     rev: &mut u64,
                     events: Vec<Event>| {
        let input = egui::RawInput {
            events,
            ..Default::default()
        };
        let font_id = font_id.clone();
        let layout_calls = std::sync::Arc::clone(&layout_calls);
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    layout_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let job = LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let output = FastEditor::new(id, text, Vec2::new(400.0, 200.0), &mut layouter)
                    .text_revision(*rev)
                    .show(ui);
                output.response.request_focus();
                if output.text_changed {
                    *rev += 1;
                }
            });
        });
        out.textures_delta.clear();
        out
    };

    // Frame 1: Initial layout + focus -> emits Window, MultilineTextInput, and active TextRun
    let out1 = run_frame(&ctx, &mut text, &mut rev, vec![]);
    assert_eq!(layout_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let u1 = out1.platform_output.accesskit_update.unwrap();
    assert!(
        u1.nodes.iter().any(|(_, n)| n.role() == egui::accesskit::Role::TextRun),
        "Initial frame should emit the active TextRun node"
    );

    // Frame 2: Idle frame with same revision and no movement -> 0 new layout calls and 0 TextRun rebuilds
    let out2 = run_frame(&ctx, &mut text, &mut rev, vec![]);
    assert_eq!(layout_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let u2 = out2.platform_output.accesskit_update.unwrap();
    assert!(
        !u2.nodes.iter().any(|(_, n)| n.role() == egui::accesskit::Role::TextRun),
        "Idle frame should not rebuild or re-allocate the AccessKit TextRun node"
    );

    // Frame 3: Multiple text events in a single frame -> coalesced into 1 layout call
    let long_chunk = "a".repeat(150);
    let out = run_frame(
        &ctx,
        &mut text,
        &mut rev,
        vec![
            Event::Text(long_chunk.clone()),
            Event::Text(long_chunk.clone()),
            Event::Text("!".to_string()),
        ],
    );
    assert_eq!(text.len(), 301);
    assert_eq!(
        layout_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "Multiple edits in one frame should coalesce into a single relayout"
    );

    // Verify AccessKit word_lengths do not overflow u8 on >255 char line
    let u = out.platform_output.accesskit_update.unwrap();
    let text_run = u
        .nodes
        .iter()
        .find(|(_, n)| n.role() == egui::accesskit::Role::TextRun)
        .map(|(_, n)| n)
        .expect("Expected active TextRun node");
    assert!(!text_run.word_starts().is_empty());
    assert!(text_run
        .word_starts()
        .iter()
        .all(|&s| (s as usize) < text_run.character_lengths().len()));
}

#[test]
fn test_virtualized_large_file_layout_and_editing() {
    use rusted::PieceTable;

    let ctx = egui::Context::default();
    let mut pt = PieceTable::from_string((0..5000).map(|i| format!("line {i:04}\n")).collect());
    let mut rev = 1u64;
    let id = Id::unique("test_virtualized_editor");
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(600.0, 300.0));

    let run_frame = |pt: &mut PieceTable, rev: &mut u64, events: Vec<Event>| {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut last_rows = 0usize;
        let mut last_first_line = 0usize;
        let mut last_cursor = None;
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .animated(false)
                    .show(ui, |ui| {
                        let mut layouter = |_ui: &Ui, _buf: &PieceTable, _wrap: f32| {
                            panic!("full-buffer layouter must not be called in virtualized mode");
                        };
                        let output = FastEditor::new_with_buffer(
                            id,
                            pt,
                            Vec2::new(400.0, 200.0),
                            &mut layouter,
                        )
                        .text_revision(*rev)
                        .virtualized(true)
                        .show(ui);
                        output.response.request_focus();
                        last_rows = output.galley.rows.len();
                        last_first_line = output.first_line;
                        last_cursor = output.cursor_range;
                        if output.text_changed {
                            *rev = rev.wrapping_add(1);
                        }
                    });
            });
        });
        out.textures_delta.clear();
        (last_rows, last_first_line, last_cursor)
    };

    // Frame 1: Initial render — only visible window (+ overscan) is laid out, far less than 5000 rows
    let (last_rows, last_first_line, _) = run_frame(&mut pt, &mut rev, vec![]);
    assert_eq!(last_first_line, 0);
    assert!(
        last_rows > 0 && last_rows < 100,
        "Expected only visible rows to be laid out, got {last_rows}"
    );

    // Frame 2: Type "ABC" at start and press ArrowDown
    let (_, _, last_cursor) = run_frame(
        &mut pt,
        &mut rev,
        vec![
            Event::Text("ABC".to_string()),
            Event::Key {
                key: Key::ArrowDown,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            },
        ],
    );
    assert_eq!(pt.slice_chars(0..12), "ABCline 0000");
    let cur = last_cursor.unwrap().primary;
    let (row, col) = pt.char_to_line_col(cur.index.0);
    assert_eq!(row, 1);
    assert_eq!(col, 3);
}

/// Runs Ctrl+A then Delete on a document made of one very long line and returns, after every
/// frame, `(run_text_len, character_index, caret_bounding_box)`.
fn long_line_ctrl_a_delete_focus(
    len: usize,
    no_spaces: bool,
    gap_frames: usize,
) -> Vec<Option<(usize, usize, egui::accesskit::Rect)>> {
    use rusted::app::TextEditor;
    let temp_dir = std::env::temp_dir().join(format!(
        "rusted_ak_{}_{}_{}_{}",
        len,
        no_spaces,
        gap_frames,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    ctx.options_mut(|o| {
        o.max_passes = std::num::NonZeroUsize::new(2).unwrap();
    });
    ctx.enable_accesskit();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.text = if no_spaces {
        "a".repeat(len).into()
    } else {
        "word ".repeat(len / 5).into()
    };
    app.text_revision = 1;
    app.focus_editor_pending = true;

    let key = |k: Key| Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: if k == Key::A {
            egui::Modifiers::COMMAND
        } else {
            egui::Modifiers::NONE
        },
    };
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(900.0, 600.0));
    let mut tree: Option<accesskit_consumer::Tree> = None;
    let mut history = Vec::new();
    let mut frame = |events: Vec<Event>, app: &mut TextEditor| {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.update_ui(ui));
        out.textures_delta.clear();
        struct NoOpHandler;
        impl accesskit_consumer::TreeChangeHandler for NoOpHandler {
            fn node_added(&mut self, _node: &accesskit_consumer::NodeRef) {}
            fn node_updated(
                &mut self,
                _old_node: &accesskit_consumer::NodeRef,
                _new_node: &accesskit_consumer::NodeRef,
            ) {
            }
            fn focus_moved(
                &mut self,
                _old_node: Option<&accesskit_consumer::NodeRef>,
                _new_node: Option<&accesskit_consumer::NodeRef>,
            ) {
            }
            fn node_removed(&mut self, _node: &accesskit_consumer::NodeRef) {}
        }
        if let Some(u) = out.platform_output.accesskit_update {
            match tree.as_mut() {
                None => tree = Some(accesskit_consumer::Tree::new(u, true)),
                Some(t) => t.update_and_process_changes(u, &mut NoOpHandler),
            }
        }
        let state = tree.as_ref().unwrap().state();
        let focus_node = state.focus();
        let caret_box = focus_node
            .as_ref()
            .and_then(|n| n.text_selection())
            .and_then(|s| s.bounding_boxes().into_iter().next());
        let focus = focus_node
            .and_then(|n| n.raw_text_selection().map(|s| s.focus))
            .and_then(|pos| {
                let run = state.node_by_tree_local_id(pos.node, egui::accesskit::TreeId::ROOT)?;
                let run_len = run.value().map_or(0, |v| v.chars().count());
                let rect = caret_box?;
                Some((run_len, pos.character_index, rect))
            });
        history.push(focus);
    };
    for _ in 0..4 {
        frame(vec![], &mut app);
    }
    frame(vec![key(Key::A)], &mut app);
    for _ in 0..gap_frames {
        frame(vec![], &mut app);
    }
    frame(vec![key(Key::Delete)], &mut app);
    for _ in 0..4 {
        frame(vec![], &mut app);
    }
    assert!(app.text.is_empty());
    let _ = std::fs::remove_dir_all(&temp_dir);
    history
}

#[test]
fn test_accesskit_caret_moves_to_start_after_select_all_delete_on_long_line() {
    for no_spaces in [false, true] {
        for gap_frames in [0usize, 1] {
            for len in [3_000usize, 15_000, 200_000] {
                let history = long_line_ctrl_a_delete_focus(len, no_spaces, gap_frames);
                let (_, _, initial_rect) = history[3].expect("initial caret must be exposed");
                let delete_frame_idx = 5 + gap_frames;
                for (i, entry) in history[delete_frame_idx..].iter().enumerate() {
                    let (run_len, idx, rect) = entry
                        .unwrap_or_else(|| panic!("len={len}: missing caret after Delete ({i})"));
                    assert_eq!((run_len, idx), (2, 1), "len={len}: caret index mismatch");
                    assert!(
                        rect.height() >= 10.0,
                        "len={len}: post-Delete caret rect must have line height, got {rect:?}"
                    );
                    assert!(
                        (rect.x0 - initial_rect.x0).abs() <= 1.0
                            && (rect.y0 - initial_rect.y0).abs() <= 3.0,
                        "len={len}: caret must return to start ({initial_rect:?}), got {rect:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn test_virtualized_crlf_navigation_and_editing() {
    use rusted::PieceTable;

    let ctx = egui::Context::default();
    let mut pt = PieceTable::from_str("alpha\r\nbeta\r\ngamma");
    let mut rev = 1u64;
    let id = Id::unique("test_virtualized_crlf");
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(600.0, 300.0));

    let run_frame = |pt: &mut PieceTable, rev: &mut u64, events: Vec<Event>| {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut last_cursor = None;
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .animated(false)
                    .show(ui, |ui| {
                        let mut layouter = |_ui: &Ui, _buf: &PieceTable, _wrap: f32| {
                            panic!("full-buffer layouter must not be called in virtualized mode");
                        };
                        let output = FastEditor::new_with_buffer(
                            id,
                            pt,
                            Vec2::new(400.0, 200.0),
                            &mut layouter,
                        )
                        .text_revision(*rev)
                        .newline("\r\n")
                        .virtualized(true)
                        .show(ui);
                        output.response.request_focus();
                        last_cursor = output.cursor_range;
                        if output.text_changed {
                            *rev = rev.wrapping_add(1);
                        }
                    });
            });
        });
        out.textures_delta.clear();
        last_cursor.unwrap().primary.index.0
    };

    let key = |k: Key| Event::Key {
        key: k,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    };

    // Frame 1: Initial render & focus
    assert_eq!(run_frame(&mut pt, &mut rev, vec![]), 0);

    // End stops before '\r' (index 5)
    assert_eq!(run_frame(&mut pt, &mut rev, vec![key(Key::End)]), 5);

    // ArrowRight skips over "\r\n" onto index 7 ("beta")
    assert_eq!(run_frame(&mut pt, &mut rev, vec![key(Key::ArrowRight)]), 7);

    // ArrowLeft skips back over "\r\n" to index 5 (before '\r')
    assert_eq!(run_frame(&mut pt, &mut rev, vec![key(Key::ArrowLeft)]), 5);

    // Enter inserts "\r\n" (text becomes "alpha\r\n\r\nbeta\r\ngamma", caret at 7)
    assert_eq!(run_frame(&mut pt, &mut rev, vec![key(Key::Enter)]), 7);
    assert_eq!(pt, "alpha\r\n\r\nbeta\r\ngamma");

    // Backspace removes "\r\n" as one unit
    assert_eq!(run_frame(&mut pt, &mut rev, vec![key(Key::Backspace)]), 5);
    assert_eq!(pt, "alpha\r\nbeta\r\ngamma");

    // Delete removes "\r\n" as one unit
    assert_eq!(run_frame(&mut pt, &mut rev, vec![key(Key::Delete)]), 5);
    assert_eq!(pt, "alphabeta\r\ngamma");
}

/// After the OS window loses and regains focus, UIA clients must keep seeing a text
/// selection and receive a fresh selection change (magnifier caret tracking).
#[test]
fn test_accesskit_selection_resyncs_after_window_refocus() {
    use rusted::app::TextEditor;
    let temp_dir = std::env::temp_dir().join(format!("rusted_refocus_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();
    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.text = "hello world\nsecond line".to_string().into();
    app.text_revision = 1;
    app.focus_editor_pending = true;
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(900.0, 600.0));
    let mut tree: Option<accesskit_consumer::Tree> = None;
    let mut selections = Vec::new();
    for (i, focused) in [true, true, true, false, false, true, true, true].into_iter().enumerate() {
        let mut events = vec![];
        if i == 3 {
            events.push(Event::WindowFocused(false));
        }
        if i == 5 {
            events.push(Event::WindowFocused(true));
        }
        let input = egui::RawInput { screen_rect: Some(screen), events, focused, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.update_ui(ui));
        out.textures_delta.clear();
        let u = out.platform_output.accesskit_update.unwrap();
        match tree.as_mut() {
            None => tree = Some(accesskit_consumer::Tree::new(u, true)),
            Some(t) => {
                struct H;
                impl accesskit_consumer::TreeChangeHandler for H {
                    fn node_added(&mut self, _n: &accesskit_consumer::NodeRef) {}
                    fn node_updated(&mut self, _o: &accesskit_consumer::NodeRef, _n: &accesskit_consumer::NodeRef) {}
                    fn focus_moved(&mut self, _o: Option<&accesskit_consumer::NodeRef>, _n: Option<&accesskit_consumer::NodeRef>) {}
                    fn node_removed(&mut self, _n: &accesskit_consumer::NodeRef) {}
                }
                t.update_and_process_changes(u, &mut H);
            }
        }
        let state = tree.as_ref().unwrap().state();
        let sel = state
            .focus_in_tree()
            .raw_text_selection()
            .map(|s| s.focus.character_index);
        selections.push(sel);
    }
    let _ = std::fs::remove_dir_all(&temp_dir);
    assert!(selections[3..].iter().all(|s| s.is_some()), "selection must stay exposed: {selections:?}");
    assert_ne!(selections[4], selections[5], "regaining focus must change selection: {selections:?}");
    assert_eq!(selections[4], selections[6], "selection must settle back on the caret: {selections:?}");
}
