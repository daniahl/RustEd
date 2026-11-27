//! Unit and integration tests for code folding (`src/folding.rs`) and its integration
//! with `FastEditor`, `Document`, and `TextEditor`.

use eframe::egui;
use egui::{epaint::text::LayoutJob, Color32, Event, Id, Key, Modifiers, Ui, Vec2};
use rusted::{
    app::TextEditor,
    document::WRAPPED_ROW_FLAG,
    editor::{code_editor_id, FastEditor, FastEditorState},
    folding::{compute_fold_ranges, compute_fold_ranges_buffer, FoldRange, FoldState, FoldView},
    PieceTable,
};
use std::fs;

fn run_ui(
    ctx: &egui::Context,
    input: egui::RawInput,
    f: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let mut out = ctx.run_ui(input, f);
    out.textures_delta.clear();
    out
}

#[test]
fn test_brace_fold_ranges_nested_and_if_else() {
    let code = "\
fn main() {
    if a {
        foo();
    } else if b {
        bar();
    } else {
        baz();
    }
    done();
}";
    let ranges = compute_fold_ranges(code);
    assert_eq!(
        ranges,
        vec![
            FoldRange { start_line: 0, end_line: 8 },
            FoldRange { start_line: 1, end_line: 2 },
            FoldRange { start_line: 3, end_line: 4 },
            FoldRange { start_line: 5, end_line: 6 },
        ]
    );
}

#[test]
fn test_brace_fold_ignores_single_line_empty_and_comments_strings() {
    let code = "\
fn single() { return 1; }
fn empty() {
}
fn tricky<'a>(s: &'a str) {
    let a = \"}\"; // {
    let b = \"escaped \\\"}\\\" \\\\\";
    let c = '}';
    let d = '\"';
    let e = '\\\"';
    /* nested /* } */ } */
    let x = 1;
}";
    let ranges = compute_fold_ranges(code);
    // Line 0: single-line -> no fold
    // Line 1..2: empty 2-line `{` / `}` -> no fold
    // Line 3..11: brace matches line 11 -> hides lines 4..=10
    assert_eq!(
        ranges,
        vec![FoldRange {
            start_line: 3,
            end_line: 10
        }]
    );
}

#[test]
fn test_brace_fold_closing_on_content_line_and_unindented_braces() {
    let code = "\
fn messy() {
let x = 1;
let y = 2;
}
fn inline_close() {
    return 42; }";
    let ranges = compute_fold_ranges(code);
    assert_eq!(
        ranges,
        vec![
            FoldRange { start_line: 0, end_line: 2 },
            FoldRange { start_line: 4, end_line: 5 },
        ]
    );
}

#[test]
fn test_indentation_fold_ranges_python_and_blank_lines() {
    let code = "\
class Foo:
    def bar(self):
        x = 1

        return x

    def baz(self):
        return 2

def top():
\tpass";
    let ranges = compute_fold_ranges(code);
    assert_eq!(
        ranges,
        vec![
            FoldRange { start_line: 0, end_line: 7 },
            FoldRange { start_line: 1, end_line: 4 },
            FoldRange { start_line: 6, end_line: 7 },
            FoldRange { start_line: 9, end_line: 10 },
        ]
    );
}

#[test]
fn test_combined_brace_and_indentation_precedence() {
    // Brace fold at line 0 should span past the unindented comment on line 2,
    // while indentation folding still folds the multiline parameter list on line 5.
    let code = "\
fn foo() {
    let a = 1;
// unindented debug comment
    let b = 2;
}
fn bar(
    x: i32,
    y: i32,
) {
    do_work();
}";
    let ranges = compute_fold_ranges(code);
    // Line 0 brace fold (0..=3) takes precedence over indentation fold (which would stop at line 1).
    // Line 2 (indent 0) followed by line 3 (indent 4) also forms an indentation fold (2..=3).
    assert_eq!(
        ranges,
        vec![
            FoldRange { start_line: 0, end_line: 3 },
            FoldRange { start_line: 2, end_line: 3 },
            FoldRange { start_line: 5, end_line: 7 },
            FoldRange { start_line: 8, end_line: 9 },
        ]
    );
}

#[test]
fn test_streaming_buffer_matches_str_across_piece_and_crlf_splits() {
    let raw = "fn a() {\r\n    let x = 1;\r\n    if true {\r\n        y();\r\n    }\r\n}\r\n";
    let expected = compute_fold_ranges(raw);

    let mut pt = PieceTable::from_string("fn a() {\r\n    let x = 1;\r\n}\r\n".to_string());
    // Insert in the middle to create multiple pieces, including CRLF boundaries
    let insert_at = pt.line_to_char(2);
    pt.insert_text("    if true {\r\n        y();\r\n    }\r\n", insert_at);
    assert_eq!(pt.build_string(), raw);

    let from_buf = compute_fold_ranges_buffer(&pt);
    assert_eq!(from_buf, expected);
}

#[test]
fn test_fold_state_nested_enclosing_and_edit_adjustments() {
    let code = "\
fn outer() {
    if cond {
        step1();
        step2();
    }
    after();
}";
    let mut state = FoldState::new();
    state.ensure_updated_str(code, 1);

    // Cursor inside inner block (line 2): first fold collapses inner (line 1)
    assert_eq!(state.fold_at_or_enclosing(2), Some(1));
    assert!(state.is_folded(1));
    assert!(state.is_line_hidden(2));
    assert!(state.is_line_hidden(3));
    assert!(!state.is_line_hidden(4));

    // Stepping outward from line 1 collapses outer (line 0)
    assert_eq!(state.fold_at_or_enclosing(1), Some(0));
    assert!(state.is_folded(0));
    assert!(state.is_folded(1));
    assert_eq!(state.visible_lines(7), &[0, 6]);
    assert_eq!(state.outermost_visible_ancestor(3), 0);

    // Unfolding outer preserves inner fold!
    assert_eq!(state.unfold_at_or_enclosing(0), Some(0));
    assert!(!state.is_folded(0));
    assert!(state.is_folded(1));
    assert_eq!(state.visible_lines(7), &[0, 1, 4, 5, 6]);

    // Unfolding from closing brace line (line 4) unfolds line 1
    assert_eq!(state.unfold_at_or_enclosing(4), Some(1));
    assert!(!state.has_active_folds());

    // Fold all & unfold_containing
    assert!(state.fold_all());
    assert!(state.is_line_hidden(2));
    assert!(state.unfold_containing(2));
    assert!(!state.is_line_hidden(2));

    // Edit shifting: fold line 1, insert 2 lines at line 0 -> fold shifts to line 3
    state.unfold_all();
    assert!(state.fold_line(1));
    state.adjust_for_edit(0, 0, 2);
    assert!(state.is_folded(3));
    assert!(!state.is_folded(1));
}

#[test]
fn test_fold_view_coordinate_mapping_crlf_and_unicode() {
    let code = "fn 🦀() {\r\n    let café = 1;\r\n    let b = 2;\r\n}\r\n";
    let mut state = FoldState::new();
    state.ensure_updated_str(code, 1);
    assert!(state.fold_line(0));

    let (view, display) = FoldView::build_full(code, &state);
    assert_eq!(display, "fn 🦀() {\n}\n");
    assert_eq!(view.lines.len(), 3);
    assert_eq!(view.lines[0].doc_line, 0);
    assert_eq!(view.lines[0].hidden_after, 2);
    assert_eq!(view.lines[1].doc_line, 3);
    assert_eq!(view.lines[2].doc_line, 4);

    // Round-trip every display position on visible lines
    for disp_idx in 0..=display.chars().count() {
        let doc_idx = view.disp_to_doc(disp_idx);
        let back = view.doc_to_disp(doc_idx);
        // Display newline position maps to doc_end of line 0, which maps back to disp_end of line 0.
        if disp_idx != view.lines[0].disp_end + 1 && disp_idx != view.lines[1].disp_end + 1 {
            // Valid content char positions round-trip cleanly
            assert_eq!(back, disp_idx.min(view.lines.last().unwrap().disp_end));
        }
    }

    // Hidden line doc char clamps to end of folded header line 0
    let hidden_char = code.chars().position(|c| c == 'é').unwrap();
    assert_eq!(view.doc_to_disp(hidden_char), view.lines[0].disp_end);

    // Closing brace on line 3 maps to line 1 col 0 of display
    let close_doc = code.chars().position(|c| c == '}').unwrap();
    assert_eq!(view.doc_to_disp(close_doc), view.lines[1].disp_start);
    assert_eq!(view.disp_to_doc(view.lines[1].disp_start), close_doc);
}

#[test]
fn test_fast_editor_folding_navigation_copy_select_all_and_editing() {
    let ctx = egui::Context::default();
    ctx.enable_accesskit();

    let mut text = "\
fn main() {
    let x = 1;
    let y = 2;
}
fn tail() {
    end();
}"
    .to_string();
    let mut fold_state = FoldState::new();
    fold_state.ensure_updated_str(&text, 1);
    assert!(fold_state.fold_line(0)); // hides lines 1..=2
    assert!(fold_state.fold_line(4)); // hides line 5

    let id = Id::unique("test_folded_editor");
    let font_id = egui::TextStyle::Monospace.resolve(&ctx.global_style());

    let run_frame = |ctx: &egui::Context,
                     text: &mut String,
                     fold_state: &mut FoldState,
                     raw_input: egui::RawInput| {
        let font_id = font_id.clone();
        let mut last_cursor = None;
        let mut row_count = 0usize;
        let mut galley_text = String::new();
        let mut out = ctx.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let mut layouter = |ui: &Ui, s: &str, wrap_width: f32| {
                    let job = LayoutJob::simple(
                        s.to_owned(),
                        font_id.clone(),
                        Color32::WHITE,
                        wrap_width,
                    );
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let output = FastEditor::new(id, text, Vec2::new(300.0, 200.0), &mut layouter)
                    .fold_state(fold_state)
                    .show(ui);
                output.response.request_focus();
                last_cursor = output.cursor_range;
                row_count = output.galley.rows.len();
                galley_text = output.galley.text().to_owned();
            });
        });
        out.textures_delta.clear();
        (last_cursor, row_count, galley_text, out)
    };

    // Frame 1: Focus & verify folded galley contains only visible lines (0, 3, 4, 6)
    let (cur, rows, gtext, out1) = run_frame(&ctx, &mut text, &mut fold_state, Default::default());
    assert_eq!(rows, 4);
    assert_eq!(gtext, "fn main() {\n}\nfn tail() {\n}");
    assert_eq!(cur.unwrap().primary.index.0, 0);

    // Verify AccessKit tree has no duplicate children
    let u = out1.platform_output.accesskit_update.unwrap();
    let mut seen = std::collections::HashSet::new();
    for (_, node) in &u.nodes {
        for &child in node.children() {
            assert!(seen.insert(child), "Duplicate AccessKit child!");
        }
    }

    // Frame 2: ArrowDown from line 0 col 0 should jump directly to line 3 (`}`), skipping lines 1..=2
    let mut down = egui::RawInput::default();
    down.events.push(Event::Key {
        key: Key::ArrowDown,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let (cur, _, _, _) = run_frame(&ctx, &mut text, &mut fold_state, down);
    let close_brace_idx = text.chars().position(|c| c == '}').unwrap();
    assert_eq!(
        cur.unwrap().primary.index.0,
        close_brace_idx,
        "ArrowDown should skip hidden lines 1..=2 and land on line 3"
    );

    // Frame 3: Shift+ArrowUp selects from line 3 (`}`) up to line 0 col 0; Ctrl+C copies full hidden body!
    let mut sel_copy = egui::RawInput::default();
    sel_copy.events.push(Event::Key {
        key: Key::ArrowUp,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::SHIFT,
    });
    sel_copy.events.push(Event::Copy);
    let (_, _, _, out_copy) = run_frame(&ctx, &mut text, &mut fold_state, sel_copy);
    assert!(
        out_copy.platform_output.commands.iter().any(|cmd| {
            matches!(
                cmd,
                egui::OutputCommand::CopyText(s) if s == "fn main() {\n    let x = 1;\n    let y = 2;\n"
            )
        }),
        "Copying across a folded region should copy the hidden lines too"
    );

    // Frame 4: Ctrl+A selects the entire document including hidden lines
    let mut select_all = egui::RawInput::default();
    select_all.events.push(Event::Key {
        key: Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND,
    });
    let (cur, _, _, _) = run_frame(&ctx, &mut text, &mut fold_state, select_all);
    let [min_c, max_c] = cur.unwrap().sorted_cursors();
    assert_eq!((min_c.index.0, max_c.index.0), (0, text.chars().count()));

    // Frame 5: Collapse selection to start of line 0, type comment prefix on line 0 -> fold 0 stays folded!
    let mut home_type = egui::RawInput::default();
    home_type.events.push(Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    home_type.events.push(Event::Text("pub ".to_string()));
    let (_, rows_after_type, _, _) = run_frame(&ctx, &mut text, &mut fold_state, home_type);
    assert!(text.starts_with("pub fn main() {"));
    assert!(fold_state.is_folded(0), "Typing on folded header should keep fold intact");
    assert_eq!(rows_after_type, 4);

    // Frame 6: Move to end of line 0 and press Delete -> touches hidden line 1 -> auto-unfolds line 0!
    let mut end_del = egui::RawInput::default();
    end_del.events.push(Event::Key {
        key: Key::End,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    end_del.events.push(Event::Key {
        key: Key::Delete,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    });
    let (_, _, _, _) = run_frame(&ctx, &mut text, &mut fold_state, end_del);
    assert!(
        !fold_state.is_folded(0),
        "Pressing Delete at end of folded line should auto-unfold it"
    );
}

#[test]
fn test_app_shortcuts_gutter_numbers_and_find_auto_unfold() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_fold_app_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.doc.set_text(
        "\
fn alpha() {
    let needle = 123;
    println!(\"{needle}\");
}
fn beta() {
    let other = 456;
}",
    );

    // Frame 1: Initial render (all 7 lines visible)
    let _ = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.doc.folding.ranges().len(), 2);
    assert!(!app.doc.folding.has_active_folds());
    let nums = &app.doc.gutter_cache.as_ref().unwrap().1;
    let visible_line_nums: Vec<u32> = nums
        .iter()
        .copied()
        .filter(|&n| (n & WRAPPED_ROW_FLAG) == 0)
        .collect();
    assert_eq!(visible_line_nums, vec![1, 2, 3, 4, 5, 6, 7]);

    // Frame 1b: Ctrl+Alt+Left (e.g. Windows Magnifier pan) must NOT trigger Alt+Left folding
    let mut ctrl_alt_left = egui::RawInput::default();
    ctrl_alt_left.events.push(Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        },
    });
    let _ = run_ui(&ctx, ctrl_alt_left, |ui| {
        app.update_ui(ui);
    });
    assert!(
        !app.doc.folding.has_active_folds(),
        "Ctrl+Alt+Left must not trigger Alt+Left folding"
    );

    // Frame 2: Press Alt+Left on line 0 -> folds alpha() (lines 1..=2 hidden)
    let mut fold_key = egui::RawInput::default();
    fold_key.events.push(Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::ALT,
    });
    let _ = run_ui(&ctx, fold_key, |ui| {
        app.update_ui(ui);
    });
    assert!(app.doc.folding.is_folded(0));
    assert!(!app.doc.folding.is_folded(4));
    let nums = &app.doc.gutter_cache.as_ref().unwrap().1;
    let visible_line_nums: Vec<u32> = nums
        .iter()
        .copied()
        .filter(|&n| (n & WRAPPED_ROW_FLAG) == 0)
        .collect();
    assert_eq!(
        visible_line_nums,
        vec![1, 4, 5, 6, 7],
        "Gutter should display true document line numbers skipping folded lines 2 and 3"
    );

    // Frame 2b: Press Alt+Right on line 0 -> unfolds alpha()
    let mut unfold_key = egui::RawInput::default();
    unfold_key.events.push(Event::Key {
        key: Key::ArrowRight,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::ALT,
    });
    let _ = run_ui(&ctx, unfold_key, |ui| {
        app.update_ui(ui);
    });
    assert!(!app.doc.folding.is_folded(0));

    // Frame 3: Press Alt+Shift+Left -> folds all blocks (alpha and beta)
    let mut fold_all_key = egui::RawInput::default();
    fold_all_key.events.push(Event::Key {
        key: Key::ArrowLeft,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        },
    });
    let _ = run_ui(&ctx, fold_all_key, |ui| {
        app.update_ui(ui);
    });
    assert!(app.doc.folding.is_folded(0));
    assert!(app.doc.folding.is_folded(4));

    // Find "needle" (which is hidden inside alpha() on line 1) -> auto-unfolds alpha()!
    app.doc.find_query = "needle".to_string();
    assert!(app.doc.find_next(&ctx));
    assert!(
        !app.doc.folding.is_folded(0),
        "find_next should auto-unfold the block containing the match"
    );
    assert!(
        app.doc.folding.is_folded(4),
        "Unrelated fold beta() should remain collapsed"
    );

    // Press Alt+Shift+Right -> unfolds all remaining folds
    let mut unfold_all_key = egui::RawInput::default();
    unfold_all_key.events.push(Event::Key {
        key: Key::ArrowRight,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        },
    });
    let _ = run_ui(&ctx, unfold_all_key, |ui| {
        app.update_ui(ui);
    });
    assert!(!app.doc.folding.has_active_folds());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_toggle_fold_clamps_cursor_and_preserves_across_dirty_file_switch() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_fold_switch_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let file_a = temp_dir.join("a.rs");
    let file_b = temp_dir.join("b.rs");
    fs::write(&file_a, "fn a() {\n    let x = 1;\n    let y = 2;\n}\n").unwrap();
    fs::write(&file_b, "fn b() {\n    let z = 3;\n}\n").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.open_file(&ctx, file_a.clone());

    // Place caret on hidden-to-be line 1 ("    let x = 1;")
    let line1_char = app.doc.text.line_to_char(1) + 4;
    ctx.data_mut(|d| {
        let mut st = d
            .get_temp::<FastEditorState>(code_editor_id())
            .unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(line1_char),
        )));
        d.insert_temp(code_editor_id(), st);
    });

    // Fold line 0 -> cursor should clamp out of hidden line 1 to end of line 0!
    assert!(app.doc.toggle_fold_at(&ctx, 0));
    assert!(app.doc.folding.is_folded(0));
    let cur = ctx
        .data_mut(|d| d.get_temp::<FastEditorState>(code_editor_id()))
        .unwrap()
        .cursor
        .char_range()
        .unwrap()
        .primary
        .index
        .0;
    assert_eq!(cur, app.doc.text.line_char_range(0).end);

    // Mark dirty and switch to file_b, then back to file_a -> fold state restored!
    app.doc.is_current_dirty = true;
    app.open_file(&ctx, file_b.clone());
    assert!(!app.doc.folding.has_active_folds());

    app.open_file(&ctx, file_a.clone());
    assert!(
        app.doc.folding.is_folded(0),
        "Collapsed folds should be preserved when switching back to a dirty file"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_virtualized_large_file_folding() {
    let ctx = egui::Context::default();
    // Build a > 64 KB file with many foldable blocks
    let mut big = String::with_capacity(80_000);
    for i in 0..1200 {
        big.push_str(&format!(
            "fn func_{i:04}() {{\n    let value_{i:04} = \"some padding text for size\";\n    println!(\"{{value_{i:04}}}\");\n}}\n"
        ));
    }
    assert!(big.len() > rusted::document::MAX_HIGHLIGHT_BYTES);

    let mut pt = PieceTable::from_string(big);
    let mut fold_state = FoldState::new();
    fold_state.ensure_updated_buffer(&pt, Some(1));
    assert_eq!(fold_state.ranges().len(), 1200);
    // Fold first block (line 0, hides lines 1..=2)
    assert!(fold_state.fold_line(0));

    let id = Id::unique("test_virtualized_fold");
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(600.0, 300.0));

    let run_frame = |ctx: &egui::Context,
                     pt: &mut PieceTable,
                     fold_state: &mut FoldState,
                     events: Vec<Event>| {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut cursor_pos = None;
        let mut galley_text = String::new();
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .animated(false)
                    .show(ui, |ui| {
                        let mut dummy = |_ui: &Ui, _s: &str, _w: f32| unreachable!();
                        let output = FastEditor::new(id, pt, Vec2::new(500.0, 280.0), &mut dummy)
                            .text_revision(1)
                            .virtualized(true)
                            .fold_state(fold_state)
                            .show(ui);
                        output.response.request_focus();
                        cursor_pos = output.cursor_range;
                        galley_text = output.galley.text().to_owned();
                    });
            });
        });
        out.textures_delta.clear();
        (cursor_pos, galley_text)
    };

    // Frame 1: First block body should be hidden in virtualized window galley
    let (cur, gtext) = run_frame(&ctx, &mut pt, &mut fold_state, vec![]);
    assert!(gtext.starts_with("fn func_0000() {\n}\nfn func_0001() {"));
    assert_eq!(cur.unwrap().primary.index.0, 0);

    // Frame 2: ArrowDown should skip hidden lines 1..=2 and land on line 3 (`}`)
    let (cur, _) = run_frame(
        &ctx,
        &mut pt,
        &mut fold_state,
        vec![Event::Key {
            key: Key::ArrowDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
    );
    let (line, col) = pt.char_to_line_col(cur.unwrap().primary.index.0);
    assert_eq!((line, col), (3, 0));
}

#[test]
fn test_clicking_inline_placeholder_and_gutter_chevron_toggles_fold() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_fold_click_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.doc.set_text("fn demo() {\n    let a = 1;\n    let b = 2;\n}\n");

    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(800.0, 500.0));
    let frame_input = |events: Vec<Event>| egui::RawInput {
        screen_rect: Some(screen),
        events,
        ..Default::default()
    };

    // Frame 1: Initial render to populate gutter & editor layout
    let _ = run_ui(&ctx, frame_input(vec![]), |ui| {
        app.update_ui(ui);
    });
    assert!(!app.doc.folding.is_folded(0));

    // Locate the fold chevron in the gutter for row 0 using the editor's galley rect.
    // The fold column sits directly to the left of the editor rect (offset by separator = 4px,
    // width = 16px, so its center is roughly 12px left of the editor's left edge).
    let editor_rect = ctx
        .read_response(code_editor_id())
        .expect("editor response")
        .rect;
    let first_row = &app.doc.gutter_cache.as_ref().unwrap().0.rows[0];
    let galley_pos = editor_rect.min + rusted::editor::EDITOR_MARGIN;
    let chevron_pos = egui::pos2(
        editor_rect.min.x - 12.0,
        galley_pos.y + first_row.rect().center().y,
    );

    // Click the gutter fold chevron (press then release)
    let _ = run_ui(
        &ctx,
        frame_input(vec![
            Event::PointerMoved(chevron_pos),
            Event::PointerButton {
                pos: chevron_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ]),
        |ui| app.update_ui(ui),
    );
    let _ = run_ui(
        &ctx,
        frame_input(vec![
            Event::PointerMoved(chevron_pos),
            Event::PointerButton {
                pos: chevron_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ]),
        |ui| app.update_ui(ui),
    );
    assert!(
        app.doc.folding.is_folded(0),
        "Clicking gutter fold chevron should collapse line 0"
    );

    // Run a frame so the folded galley and inline `...` badge are rendered
    let _ = run_ui(&ctx, frame_input(vec![]), |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.doc.gutter_cache.as_ref().unwrap().0.rows.len(), 3);

    // Click the inline `...` placeholder badge right after row 0's text
    let row0 = &app.doc.gutter_cache.as_ref().unwrap().0.rows[0];
    let badge_pos = egui::pos2(
        galley_pos.x + row0.rect().max.x + 14.0,
        galley_pos.y + row0.rect().center().y,
    );
    let _ = run_ui(
        &ctx,
        frame_input(vec![
            Event::PointerMoved(badge_pos),
            Event::PointerButton {
                pos: badge_pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ]),
        |ui| app.update_ui(ui),
    );
    let _ = run_ui(
        &ctx,
        frame_input(vec![
            Event::PointerMoved(badge_pos),
            Event::PointerButton {
                pos: badge_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            },
        ]),
        |ui| app.update_ui(ui),
    );
    assert!(
        !app.doc.folding.is_folded(0),
        "Clicking inline `...` fold badge should expand line 0"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_crossing_brace_and_indent_folds_stay_consistent() {
    // Line 0..=2 is a brace fold (closing `}` indented at 8 spaces on line 3),
    // while line 1 (indent 4) would form an indentation fold spanning 1..=4.
    let code = "\
fn a() {
    let x = 1;
        let y = 2;
        }
        let z = 3;";
    let mut state = FoldState::new();
    state.ensure_updated_str(code, 1);
    assert!(state.fold_all());

    let visible: Vec<usize> = state.visible_lines(5).to_vec();
    for line in 0..5 {
        let in_visible = visible.contains(&line);
        assert_eq!(
            !state.is_line_hidden(line),
            in_visible,
            "is_line_hidden({line}) must agree with visible_lines {visible:?}"
        );
        let ancestor = state.outermost_visible_ancestor(line);
        assert!(
            visible.contains(&ancestor),
            "outermost_visible_ancestor({line}) = {ancestor} must be visible in {visible:?}"
        );
    }
}

#[test]
fn test_fold_header_shifts_on_enter_and_backspace_at_col_0() {
    let code = "\
fn a() {
    let x = 1;
    let y = 2;
}
fn b() {
    let z = 3;
}";
    let mut state = FoldState::new();
    state.ensure_updated_str(code, 1);
    assert!(state.fold_line(0));

    // Inserting a blank line at col 0 of folded line 0 shifts the fold to line 1.
    state.adjust_for_edit_with_col(0, 0, 0, 1);
    assert!(state.is_folded(1));
    assert!(!state.is_folded(0));

    // Backspacing at col 0 of folded line 1 (merging onto line 0) shifts it back to line 0.
    state.adjust_for_edit_with_col(0, usize::MAX, 1, -1);
    assert!(state.is_folded(0));
    assert!(!state.is_folded(1));
}

#[test]
fn test_toggle_fold_clamps_secondary_cursor_out_of_hidden_lines() {
    let ctx = egui::Context::default();
    let mut doc = rusted::document::Document::new();
    doc.set_text("fn a() {\n    let x = 1;\n    let y = 2;\n}\n");

    // Selection with primary on visible line 0 and secondary inside soon-to-be-hidden line 2.
    let line2_char = doc.text.line_to_char(2) + 4;
    ctx.data_mut(|d| {
        let mut st = FastEditorState::default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(line2_char),
            egui::text::CCursor::new(0),
        )));
        d.insert_temp(code_editor_id(), st);
    });

    assert!(doc.toggle_fold_at(&ctx, 0));
    let range = ctx
        .data_mut(|d| d.get_temp::<FastEditorState>(code_editor_id()))
        .unwrap()
        .cursor
        .char_range()
        .unwrap();
    let (sec_line, _) = doc.text.char_to_line_col(range.secondary.index.0);
    assert!(
        !doc.folding.is_line_hidden(sec_line),
        "Secondary cursor must be clamped out of hidden lines"
    );
}

#[test]
fn test_virtualized_ctrl_arrows_skip_folded_lines_and_plain_arrows_collapse_selection() {
    let ctx = egui::Context::default();
    let mut big = String::with_capacity(80_000);
    for i in 0..1200 {
        big.push_str(&format!(
            "fn func_{i:04}() {{\n    let value_{i:04} = \"some padding text for size\";\n    println!(\"{{value_{i:04}}}\");\n}}\n"
        ));
    }
    let mut pt = PieceTable::from_string(big);
    let mut fold_state = FoldState::new();
    fold_state.ensure_updated_buffer(&pt, Some(1));
    assert!(fold_state.fold_line(0)); // hides lines 1..=2

    let id = Id::unique("test_virtualized_ctrl_arrows");
    let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(600.0, 300.0));
    let run_frame = |pt: &mut PieceTable, fold_state: &mut FoldState, events: Vec<Event>| {
        let input = egui::RawInput {
            screen_rect: Some(screen),
            events,
            ..Default::default()
        };
        let mut cursor_pos = None;
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .animated(false)
                    .show(ui, |ui| {
                        let mut dummy = |_ui: &Ui, _s: &str, _w: f32| unreachable!();
                        let output = FastEditor::new(id, pt, Vec2::new(500.0, 280.0), &mut dummy)
                            .text_revision(1)
                            .virtualized(true)
                            .fold_state(fold_state)
                            .show(ui);
                        output.response.request_focus();
                        cursor_pos = output.cursor_range;
                    });
            });
        });
        out.textures_delta.clear();
        cursor_pos.unwrap()
    };

    // Place caret at end of folded line 0
    let line0_end = pt.line_char_range(0).end;
    ctx.data_mut(|d| {
        let mut st = FastEditorState::default();
        st.cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(
                line0_end,
            ))));
        d.insert_temp(id, st);
    });
    let _ = run_frame(&mut pt, &mut fold_state, vec![]);

    // Ctrl+ArrowRight from end of folded line 0 must jump to line 3 (`}`), not into hidden line 1
    let cur = run_frame(
        &mut pt,
        &mut fold_state,
        vec![Event::Key {
            key: Key::ArrowRight,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }],
    );
    let (line_after_right, _) = pt.char_to_line_col(cur.primary.index.0);
    assert!(!fold_state.is_line_hidden(line_after_right));
    assert_eq!(line_after_right, 3);

    // Ctrl+ArrowLeft from start of line 3 must jump back to end of line 0, not into hidden line 2
    let cur = run_frame(
        &mut pt,
        &mut fold_state,
        vec![Event::Key {
            key: Key::ArrowLeft,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        }],
    );
    let (line_after_left, _) = pt.char_to_line_col(cur.primary.index.0);
    assert!(!fold_state.is_line_hidden(line_after_left));
    assert_eq!(line_after_left, 0);

    // With selection 0..5, plain ArrowLeft collapses to 0 and plain ArrowRight collapses to 5
    ctx.data_mut(|d| {
        let mut st = d.get_temp::<FastEditorState>(id).unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(5),
        )));
        d.insert_temp(id, st);
    });
    let cur = run_frame(
        &mut pt,
        &mut fold_state,
        vec![Event::Key {
            key: Key::ArrowLeft,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }],
    );
    assert_eq!((cur.secondary.index.0, cur.primary.index.0), (0, 0));
}

#[test]
fn test_fold_scanner_ignores_hash_comments_multiline_strings_and_raw_strings() {
    use rusted::folding::{compute_fold_ranges, compute_fold_ranges_with_options, FoldRange};

    // Python / hash-comment file with stray `{` in a `#` comment
    let py = "\
# comment with stray {
def foo():
    d = {
        'a': 1,
    }
    return d
";
    let ranges = compute_fold_ranges_with_options(py, true);
    assert!(
        ranges.contains(&FoldRange {
            start_line: 2,
            end_line: 3,
        }),
        "brace fold for dict should not be broken by `# {{`, got {ranges:?}"
    );
    assert!(
        !ranges.iter().any(|r| r.start_line == 0),
        "comment line 0 must not start a brace fold, got {ranges:?}"
    );

    // Rust file with multi-line double-quoted string and raw string containing unmatched braces
    let rs = "\
fn demo() {
    let s = \"multiline {
        still inside string {{{
    \";
    let raw = r#\"
        raw string with }}}} and {
    \"#;
    let x = 1;
}
";
    let rs_ranges = compute_fold_ranges(rs);
    assert!(
        rs_ranges.contains(&FoldRange {
            start_line: 0,
            end_line: 7,
        }),
        "fn demo() fold should span lines 0..=7 despite braces inside strings, got {rs_ranges:?}"
    );
}

#[test]
fn test_fold_syntax_profiles_handle_python_js_and_rust_escapes() {
    use rusted::folding::{compute_fold_ranges_with_syntax, FoldRange, FoldSyntax};

    // Python: `//` floor division, `/*` glob, and `'{'` single-quoted string must not break folding
    let py = "\
def calc():
    d = {
        'pattern': '/* {',
        'half': 10 // 2,
    }
    return d
";
    let py_ranges = compute_fold_ranges_with_syntax(py, FoldSyntax::from_extension("py"));
    assert!(
        py_ranges.contains(&FoldRange {
            start_line: 1,
            end_line: 3,
        }),
        "Python dict fold should survive `/*`, `//`, and single-quoted `{{`: {py_ranges:?}"
    );

    // JS: non-nested `/* /* */` block comments and single-quoted `'{'` strings
    let js = "\
function run() {
    /* path /* */
    const s = '{ not a block';
    if (true) {
        return s;
    }
}
";
    let js_ranges = compute_fold_ranges_with_syntax(js, FoldSyntax::from_extension("js"));
    assert!(
        js_ranges.contains(&FoldRange {
            start_line: 0,
            end_line: 5,
        }) && js_ranges.contains(&FoldRange {
            start_line: 3,
            end_line: 4,
        }),
        "JS folds should handle non-nested block comments and single-quoted strings: {js_ranges:?}"
    );

    // Rust: unicode char escape `'\u{7b}'` must not corrupt brace matching
    let rs = "\
fn brace_char() {
    let c = '\\u{7b}';
    let x = c;
}
";
    let rs_ranges = compute_fold_ranges_with_syntax(rs, FoldSyntax::from_extension("rs"));
    assert!(
        rs_ranges.contains(&FoldRange {
            start_line: 0,
            end_line: 2,
        }),
        "Rust unicode char escape should not break brace fold: {rs_ranges:?}"
    );
}

#[test]
fn test_undo_redo_restores_shifted_folds_and_single_line_quote_reset() {
    use rusted::folding::{compute_fold_ranges_with_syntax, FoldSyntax};

    // 1. Unclosed single-line quote in JS/Python/C and prose in Markdown must not break downstream folds
    let js_unclosed = "\
function typing() {
    const s = \"unclosed string while typing
}
function intact() {
    const ok = \"valid\";
    return ok;
}
";
    let js_ranges = compute_fold_ranges_with_syntax(js_unclosed, FoldSyntax::from_extension("js"));
    assert!(
        js_ranges.contains(&FoldRange {
            start_line: 3,
            end_line: 5,
        }),
        "Unclosed quote on an earlier JS line must not corrupt folds below it: {js_ranges:?}"
    );

    let md = "\
It's a Markdown doc with \"unclosed quote and /* not a comment
example {
    nested item 1
    nested item 2
}
";
    let md_ranges = compute_fold_ranges_with_syntax(md, FoldSyntax::from_extension("md"));
    assert!(
        md_ranges.contains(&FoldRange {
            start_line: 1,
            end_line: 3,
        }),
        "Markdown syntax profile should not treat prose quotes/slashes as Rust tokens: {md_ranges:?}"
    );

    // 2. Undo and Redo restore shifted collapsed fold lines accurately
    let ctx = egui::Context::default();
    let id = Id::unique("test_undo_redo_folds");
    let mut pt = PieceTable::from_str(
        "fn alpha() {\n    a();\n}\nfn beta() {\n    b1();\n    b2();\n}\n",
    );
    let mut rev = 1u64;
    let mut fold_state = FoldState::new();
    fold_state.ensure_updated_buffer(&pt, Some(rev));
    // Fold `fn beta()` starting on line 3
    assert!(fold_state.fold_line(3));

    let run = |pt: &mut PieceTable,
               rev: &mut u64,
               fold_state: &mut FoldState,
               time: f64,
               events: Vec<Event>| {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(600.0, 400.0))),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            let mut layouter = |ui: &Ui, s: &str, w: f32| {
                let job = LayoutJob::simple(
                    s.to_owned(),
                    egui::TextStyle::Monospace.resolve(ui.style()),
                    Color32::WHITE,
                    w,
                );
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let output = FastEditor::new(id, pt, Vec2::new(500.0, 300.0), &mut layouter)
                .text_revision(*rev)
                .fold_state(fold_state)
                .show(ui);
            rusted::editor::request_editor_focus(ui.ctx(), output.response.id);
            if output.text_changed {
                *rev = rev.wrapping_add(1);
            }
        });
        out.textures_delta.clear();
    };

    run(&mut pt, &mut rev, &mut fold_state, 0.0, vec![]);
    // Insert two newlines at (0, 0) -> `fn beta()` shifts from line 3 to line 5
    let enter = Event::Key {
        key: Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    };
    run(
        &mut pt,
        &mut rev,
        &mut fold_state,
        1.0,
        vec![enter.clone(), enter],
    );
    assert!(fold_state.is_folded(5));
    assert!(!fold_state.is_folded(3));

    // Stabilize undoer
    run(&mut pt, &mut rev, &mut fold_state, 3.0, vec![]);

    // Ctrl+Z undoes the 2-line insertion AND restores `fn beta()` fold back to line 3!
    run(
        &mut pt,
        &mut rev,
        &mut fold_state,
        4.0,
        vec![Event::Key {
            key: Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        }],
    );
    assert!(
        fold_state.is_folded(3),
        "Undo must restore shifted fold back to line 3, got {:?}",
        fold_state.folded_lines()
    );
    assert!(!fold_state.is_folded(5));

    // Ctrl+Y redoes the insertion AND shifts `fn beta()` fold back to line 5!
    run(
        &mut pt,
        &mut rev,
        &mut fold_state,
        5.0,
        vec![Event::Key {
            key: Key::Y,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        }],
    );
    assert!(fold_state.is_folded(5));
    assert!(!fold_state.is_folded(3));
}

