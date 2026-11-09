use eframe::egui;
use egui::{epaint::text::LayoutJob, Color32, Event, Id, Key, Modifiers, Ui, Vec2};

#[path = "../src/editor.rs"]
mod editor;
use editor::FastEditor;

#[test]
fn test_editor_full_lifecycle() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut text = String::new();
    let id = Id::new("test_editor");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.style());

    let run_frame = |ctx: &egui::Context, text: &mut String, raw_input: egui::RawInput| {
        let font_id = font_id.clone();
        ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    let job = LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts(|f| f.layout_job(job))
                };
                let editor = FastEditor::new(id, text, Vec2::new(100.0, 100.0), &mut layouter);
                let output = editor.show(ui);
                output.response.request_focus();
            });
        })
    };

    // Frame 1: Empty text, focus
    let out = run_frame(&ctx, &mut text, Default::default());
    let u = out.platform_output.accesskit_update.unwrap();
    assert_eq!(u.nodes.len(), 3); // Window, MultilineTextInput (self.id), and active TextRun child

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
    let mut input = egui::RawInput::default();
    input.time = Some(1.0);
    input.events.push(Event::Text("Second Line".to_string()));
    let _ = run_frame(&ctx, &mut text, input);
    assert_eq!(text, "First Line\nSecond Line");

    // Frame 4b: Idle frame at time = 2.5s (> 1.0s stable_time) to stabilize undo point
    let mut input_idle = egui::RawInput::default();
    input_idle.time = Some(2.5);
    let _ = run_frame(&ctx, &mut text, input_idle);

    // Frame 5: Press Backspace at time = 3.0s
    let mut input_bs = egui::RawInput::default();
    input_bs.time = Some(3.0);
    input_bs.events.push(Event::Key {
        key: Key::Backspace,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let _ = run_frame(&ctx, &mut text, input_bs);
    assert_eq!(text, "First Line\nSecond Lin");

    // Frame 6: Undo (Ctrl+Z) at time = 3.5s
    let mut input_undo = egui::RawInput::default();
    input_undo.time = Some(3.5);
    input_undo.events.push(Event::Key {
        key: Key::Z,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    let _ = run_frame(&ctx, &mut text, input_undo);
    assert_eq!(text, "First Line\nSecond Line");

    println!("Full lifecycle test passed successfully!");
}

#[test]
fn test_arrow_keys() {
    let ctx = egui::Context::default();

    let mut text = "Line 1\nLine 2".to_string();
    let id = Id::new("test_editor_arrows");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.style());

    let run_frame = |ctx: &egui::Context, text: &mut String, raw_input: egui::RawInput| {
        let font_id = font_id.clone();
        let mut last_cursor = None;
        let mut last_focus = false;
        let full_output = ctx.run(raw_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    let job = LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts(|f| f.layout_job(job))
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
    let cur = last_out.0.unwrap().primary;
    assert_eq!(cur.rcursor.row, 1);
    assert_eq!(cur.rcursor.column, 0);

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
    assert_eq!(cur.rcursor.row, 0);
    assert_eq!(cur.rcursor.column, 6);

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
    assert_eq!(cur.rcursor.row, 1);
    assert_eq!(cur.rcursor.column, 6);

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
    assert_eq!(cur.rcursor.row, 0);
    assert_eq!(cur.rcursor.column, 6);
}

#[test]
fn test_paste_scrolls_to_caret() {
    let ctx = egui::Context::default();
    let mut text = String::new();
    let id = Id::new("test_editor_paste");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.style());
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(400.0, 300.0));

    let mut caret_visible = false;
    let mut run_frame = |ctx: &egui::Context, text: &mut String, events: Vec<Event>| {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(screen);
        input.events = events;
        let font_id = font_id.clone();
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
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
                            ui.fonts(|f| f.layout_job(job))
                        };
                        let editor = FastEditor::new(id, text, Vec2::new(100.0, 100.0), &mut layouter);
                        let output = editor.show(ui);
                        output.response.request_focus();
                        if let Some(c) = output.cursor_range {
                            let r = output
                                .galley
                                .pos_from_cursor(&c.primary)
                                .translate(output.galley_pos.to_vec2());
                            caret_visible = ui.clip_rect().contains(r.center());
                        }
                    });
            });
        });
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
    ctx.enable_accesskit();

    let mut text = String::new();
    let mut rev = 1u64;
    let id = Id::new("test_editor_perf");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.style());
    let layout_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let run_frame = |ctx: &egui::Context,
                     text: &mut String,
                     rev: &mut u64,
                     events: Vec<Event>| {
        let mut input = egui::RawInput::default();
        input.events = events;
        let font_id = font_id.clone();
        let layout_calls = std::sync::Arc::clone(&layout_calls);
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let mut layouter = |ui: &Ui, text: &str, wrap_width: f32| {
                    layout_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let job = LayoutJob::simple(
                        text.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts(|f| f.layout_job(job))
                };
                let output = FastEditor::new(id, text, Vec2::new(400.0, 200.0), &mut layouter)
                    .text_revision(*rev)
                    .show(ui);
                output.response.request_focus();
                if output.text_changed {
                    *rev += 1;
                }
            });
        })
    };

    // Frame 1: Initial layout + focus
    let _ = run_frame(&ctx, &mut text, &mut rev, vec![]);
    assert_eq!(layout_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Frame 2: Idle frame with same revision -> 0 new layout calls
    let _ = run_frame(&ctx, &mut text, &mut rev, vec![]);
    assert_eq!(layout_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

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
    let total_word_len: usize = text_run.word_lengths().iter().map(|&w| w as usize).sum();
    assert_eq!(total_word_len, text_run.character_lengths().len());
}
