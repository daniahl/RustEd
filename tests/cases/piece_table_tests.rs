//! Unit and integration tests for the `PieceTable` text buffer.
//!
//! Validates piece descriptor management, consecutive typing optimization,
//! insertions/deletions, UTF-8 char-to-byte mapping, and `egui::TextBuffer` trait compliance.

use eframe::egui::TextBuffer;
use rusted::piece_table::PieceTable;

#[test]
fn test_piece_table_empty_and_creation() {
    let pt = PieceTable::new();
    assert!(pt.is_empty());
    assert_eq!(pt.len(), 0);
    assert_eq!(pt.char_count(), 0);
    assert_eq!(pt.line_count(), 1);
    assert_eq!(pt.piece_count(), 0);
    assert_eq!(pt.as_str(), "");
    assert_eq!(pt, "");

    let pt2 = PieceTable::from_str("Hello, world!\nLine 2");
    assert!(!pt2.is_empty());
    assert_eq!(pt2.len(), 20);
    assert_eq!(pt2.char_count(), 20);
    assert_eq!(pt2.line_count(), 2);
    assert_eq!(pt2.piece_count(), 1);
    assert_eq!(pt2.as_str(), "Hello, world!\nLine 2");
    assert_eq!(pt2, "Hello, world!\nLine 2");
}

#[test]
fn test_consecutive_typing_optimization() {
    let mut pt = PieceTable::new();

    // In VS Code, sequential inserts at the end of the added buffer extend
    // the existing piece rather than creating new piece descriptors.
    for ch in "Hello, VS Code Piece Table!".chars() {
        pt.insert(pt.char_count(), &ch.to_string());
    }

    assert_eq!(pt.as_str(), "Hello, VS Code Piece Table!");
    assert_eq!(pt.char_count(), 27);
    // Despite 27 individual keystrokes, there should be only 1 piece descriptor!
    assert_eq!(pt.piece_count(), 1);
}

#[test]
fn test_insertions_start_middle_end() {
    let mut pt = PieceTable::from_str("world");

    // Prepend at index 0
    pt.insert(0, "Hello ");
    assert_eq!(pt.as_str(), "Hello world");
    assert_eq!(pt.piece_count(), 2); // [Hello , world]

    // Append at end
    pt.insert(pt.char_count(), "!");
    assert_eq!(pt.as_str(), "Hello world!");
    assert_eq!(pt.piece_count(), 3); // [Hello , world, !]

    // Insert in middle (inside the original "world" piece)
    // "Hello " is 6 chars. "world" is at 6..11.
    // Insert "beautiful " at index 6:
    pt.insert(6, "beautiful ");
    assert_eq!(pt.as_str(), "Hello beautiful world!");
    // Splitting "world" into "" and "world" or inserting between pieces
    assert_eq!(pt.char_count(), 22);
}

#[test]
fn test_deletions_and_piece_coalescing() {
    let mut pt = PieceTable::from_str("The quick brown fox jumps over the lazy dog");

    // Delete "quick " (5..11)
    pt.delete(4..10);
    assert_eq!(pt.as_str(), "The brown fox jumps over the lazy dog");

    // Delete spanning multiple words
    // Delete "brown fox "
    pt.delete(4..14);
    assert_eq!(pt.as_str(), "The jumps over the lazy dog");

    // Delete from start
    pt.delete(0..4);
    assert_eq!(pt.as_str(), "jumps over the lazy dog");

    // Delete to end (" dog" is 4 chars)
    let len = pt.char_count();
    pt.delete(len - 4..len);
    assert_eq!(pt.as_str(), "jumps over the lazy");

    // Clear all
    pt.clear();
    assert!(pt.is_empty());
    assert_eq!(pt.as_str(), "");
    assert_eq!(pt.piece_count(), 0);
}

#[test]
fn test_multibyte_unicode_offsets() {
    // "🦀" = 4 bytes, 1 char
    // "café" = 5 bytes, 4 chars
    let mut pt = PieceTable::from_str("🦀 café");
    assert_eq!(pt.len(), 4 + 1 + 5); // 10 bytes
    assert_eq!(pt.char_count(), 6);   // 6 chars: '🦀', ' ', 'c', 'a', 'f', 'é'

    // Verify char_to_byte mapping
    assert_eq!(pt.char_to_byte(0), 0);
    assert_eq!(pt.char_to_byte(1), 4); // after '🦀'
    assert_eq!(pt.char_to_byte(2), 5); // after ' '
    assert_eq!(pt.char_to_byte(6), 10); // end of string

    // Verify byte_to_char mapping
    assert_eq!(pt.byte_to_char(0), 0);
    assert_eq!(pt.byte_to_char(4), 1);
    assert_eq!(pt.byte_to_char(5), 2);
    assert_eq!(pt.byte_to_char(10), 6);

    // Insert an emoji in the middle
    pt.insert(2, "🚀 ");
    assert_eq!(pt.as_str(), "🦀 🚀 café");
    assert_eq!(pt.char_count(), 8);

    // Delete the original emoji
    pt.delete(0..2);
    assert_eq!(pt.as_str(), "🚀 café");
}

#[test]
fn test_egui_text_buffer_trait() {
    let mut pt = PieceTable::from_str("Line 1\nLine 2");

    // Check trait methods
    assert_eq!(TextBuffer::as_str(&pt), "Line 1\nLine 2");
    assert_eq!(pt.insert_text("Inserted ", eframe::egui::text::CharIndex(7)), 9);
    assert_eq!(pt.as_str(), "Line 1\nInserted Line 2");

    pt.delete_char_range(eframe::egui::text::CharIndex(7)..eframe::egui::text::CharIndex(16));
    assert_eq!(pt.as_str(), "Line 1\nLine 2");

    pt.replace_with("Replaced");
    assert_eq!(pt.as_str(), "Replaced");

    let taken = pt.take();
    assert_eq!(taken, "Replaced");
    assert!(pt.is_empty());
}

#[test]
fn test_piece_table_large_edit_performance() {
    // Simulate a 100 KB document
    let base_line = "let value = calculate_something_important(42);\n";
    let doc_content = base_line.repeat(2000); // ~96 KB, 2000 lines
    let mut pt = PieceTable::from_string(doc_content.clone());

    assert_eq!(pt.line_count(), 2001);

    // Simulate typing characters sequentially in the middle of the document
    for (insert_pos, ch) in
        (1000..).zip("println!(\"debugging hot path performance\");\n".chars())
    {
        pt.insert(insert_pos, &ch.to_string());
    }

    // Fast check: neither original buffer was shifted in memory
    assert!(pt.len() > doc_content.len());
    assert!(pt.as_str().contains("debugging hot path performance"));
}

#[test]
fn test_has_crlf_and_byte_to_char_ascii_fast_path() {
    let pt = PieceTable::from_str("Hello\r\nWorld");
    assert!(pt.has_crlf_in_initial_bytes(4096));

    // Pure ASCII byte_to_char check
    assert_eq!(pt.byte_to_char(0), 0);
    assert_eq!(pt.byte_to_char(5), 5); // after "Hello"
    assert_eq!(pt.byte_to_char(7), 7); // after "\r\n"
    assert_eq!(pt.byte_to_char(12), 12);

    // Cross-piece CRLF check: piece 1 ends with '\r', piece 2 starts with '\n'
    let mut pt_split = PieceTable::from_str("Line1\r");
    pt_split.insert(pt_split.char_count(), "\nLine2");
    assert!(pt_split.piece_count() >= 2);
    assert!(pt_split.has_crlf_in_initial_bytes(4096));
}

#[test]
fn test_piece_table_snapshot_and_restore_zero_copy() {
    let mut pt = PieceTable::from_string("Hello world".to_string());
    let snap0 = pt.snapshot();

    // Edit 1: insert ", Rust" -> "Hello, Rust world"
    pt.insert(5, ", Rust");
    assert_eq!(pt.build_string(), "Hello, Rust world");
    let snap1 = pt.snapshot();
    assert_ne!(snap0, snap1);

    // Edit 2: delete " world" -> "Hello, Rust"
    pt.delete(11..17);
    assert_eq!(pt.as_str(), "Hello, Rust");
    let snap2 = pt.snapshot();

    // Undo to snap1
    pt.restore_snapshot(&snap1);
    assert_eq!(pt.as_str(), "Hello, Rust world");
    assert_eq!(pt.char_count(), 17);

    // Undo to snap0 (single original piece)
    pt.restore_snapshot(&snap0);
    assert_eq!(pt.as_str(), "Hello world");
    assert_eq!(pt.piece_count(), 1);

    // Redo to snap2
    pt.restore_snapshot(&snap2);
    assert_eq!(pt.as_str(), "Hello, Rust");
}

#[test]
fn test_piece_table_line_and_range_helpers() {
    let mut pt = PieceTable::from_str("alpha\n🦀beta\ngamma");
    pt.insert(6, "X"); // "alpha\nX🦀beta\ngamma" (multiple pieces + multibyte UTF-8)
    assert!(pt.piece_count() >= 2);
    assert_eq!(pt.line_count(), 3);

    assert_eq!(pt.line_to_char(0), 0);
    assert_eq!(pt.line_to_char(1), 6);
    assert_eq!(pt.line_to_char(2), 13);
    assert_eq!(pt.line_to_char(3), pt.char_count());

    assert_eq!(pt.char_to_line_col(0), (0, 0));
    assert_eq!(pt.char_to_line_col(5), (0, 5));
    assert_eq!(pt.char_to_line_col(6), (1, 0));
    assert_eq!(pt.char_to_line_col(8), (1, 2)); // after "X🦀"
    assert_eq!(pt.char_to_line_col(13), (2, 0));
    assert_eq!(pt.char_to_line_col(18), (2, 5));

    assert_eq!(pt.line_char_range(0), 0..5);
    assert_eq!(pt.line_char_range(1), 6..12);
    assert_eq!(pt.line_char_range(2), 13..18);

    assert_eq!(pt.slice_chars(6..12), "X🦀beta");
    let mut buf = String::from("stale");
    pt.extract_char_range(0..5, &mut buf);
    assert_eq!(buf, "alpha");
}

#[test]
fn test_piece_table_checkpoints_and_non_flattening_word_delete() {
    use eframe::egui::text::CCursor;
    use rusted::editor::EditorBuffer;

    // 120 lines with multibyte UTF-8 characters to exercise 32-line checkpoints
    let original: String = (0..120).map(|i| format!("line_{i:03}_🦀_café\n")).collect();
    let mut pt = PieceTable::from_string(original);
    assert_eq!(pt.line_count(), 121);

    // Each line has "line_XXX_🦀_café\n" = 16 chars
    for line in [0, 1, 31, 32, 33, 63, 64, 65, 95, 96, 119] {
        let start = pt.line_to_char(line);
        assert_eq!(start, line * 16);
        assert_eq!(pt.char_to_line_col(start + 10), (line, 10));
        assert_eq!(pt.line_char_range(line), start..start + 15);
    }

    // Split near line 50 and test non-flattening word deletion
    let pos = pt.line_to_char(50);
    pt.insert(pos, "hello world ");
    let after_insert = pos + 12;
    let c1 = pt.delete_prev_word(CCursor::new(after_insert));
    assert_eq!(c1.index.0, pos + 6);
    assert_eq!(pt.slice_chars(pos..pos + 6), "hello ");

    let c2 = pt.delete_nxt_word(CCursor::new(pos));
    assert_eq!(c2.index.0, pos);

    // Stream via write_to and verify matches build_string
    let mut streamed = Vec::new();
    pt.write_to(&mut streamed).unwrap();
    assert_eq!(String::from_utf8(streamed).unwrap(), pt.build_string());
}

// ---------------------------------------------------------------------------
// Additional coverage: delete/locality/snapshot semantics
// ---------------------------------------------------------------------------

/// Applies the same edit to a `String` model and a `PieceTable`.
fn model_insert(model: &mut String, pt: &mut PieceTable, char_idx: usize, text: &str) {
    let byte = model
        .char_indices()
        .nth(char_idx)
        .map_or(model.len(), |(b, _)| b);
    model.insert_str(byte, text);
    pt.insert(char_idx, text);
}

fn model_delete(model: &mut String, pt: &mut PieceTable, start: usize, end: usize) {
    let b = |i: usize, m: &String| m.char_indices().nth(i).map_or(m.len(), |(b, _)| b);
    let (bs, be) = (b(start, model), b(end, model));
    model.replace_range(bs..be, "");
    pt.delete(start..end);
}

fn assert_matches_model(pt: &PieceTable, model: &str) {
    assert_eq!(pt.build_string(), model);
    assert_eq!(pt.len(), model.len());
    assert_eq!(pt.char_count(), model.chars().count());
    assert_eq!(pt.line_count(), model.matches('\n').count() + 1);
}

#[test]
#[allow(clippy::reversed_empty_ranges)]
fn test_delete_noop_cases() {
    let mut pt = PieceTable::from_str("abc");
    pt.delete(1..1);
    pt.delete(2..1);
    pt.delete(10..20);
    assert_eq!(pt, "abc");

    let mut empty = PieceTable::new();
    empty.delete(0..5);
    assert!(empty.is_empty());
}

#[test]
fn test_delete_range_clamped_to_length() {
    let mut pt = PieceTable::from_str("hello world");
    pt.delete(5..1000);
    assert_eq!(pt, "hello");
    assert_eq!(pt.char_count(), 5);
}

#[test]
fn test_delete_entire_document() {
    let mut pt = PieceTable::from_str("a\nb\nc");
    pt.insert(1, "XYZ");
    pt.delete(0..pt.char_count());
    assert!(pt.is_empty());
    assert_eq!(pt.line_count(), 1);
    assert_eq!(pt.piece_count(), 0);
    pt.insert(0, "again");
    assert_eq!(pt, "again");
}

#[test]
fn test_delete_spanning_multiple_pieces_updates_totals() {
    let mut pt = PieceTable::from_str("0123456789");
    pt.insert(5, "é\n");
    pt.insert(0, "日");
    pt.insert(pt.char_count(), "end\n");
    // "日01234é\n56789end\n"
    assert_eq!(pt, "日01234é\n56789end\n");
    pt.delete(3..10);
    assert_eq!(pt, "日01789end\n");
    assert_eq!(pt.char_count(), "日01789end\n".chars().count());
    assert_eq!(pt.len(), "日01789end\n".len());
    assert_eq!(pt.line_count(), 2);
}

#[test]
fn test_delete_inside_single_piece_splits_it() {
    let mut pt = PieceTable::from_str("abcdef");
    pt.delete(2..4);
    assert_eq!(pt, "abef");
    assert_eq!(pt.piece_count(), 2);
}

#[test]
fn test_delete_then_reinsert_restores_text() {
    let mut pt = PieceTable::from_str("hello world");
    pt.delete(5..6);
    assert_eq!(pt, "helloworld");
    pt.insert(5, " ");
    assert_eq!(pt, "hello world");
}

#[test]
fn test_repeated_backspace_from_end_and_forward_delete() {
    let mut pt = PieceTable::from_str("abcdefghij");
    for i in (5..10).rev() {
        pt.delete(i..i + 1);
    }
    assert_eq!(pt, "abcde");
    for _ in 0..5 {
        pt.delete(0..1);
    }
    assert!(pt.is_empty());
}

#[test]
fn test_insert_position_clamped_past_end() {
    let mut pt = PieceTable::from_str("abc");
    assert_eq!(pt.insert(99, "Z"), 1);
    assert_eq!(pt, "abcZ");
    assert_eq!(pt.insert(0, ""), 0);
}

#[test]
fn test_insert_returns_char_count_for_multibyte() {
    let mut pt = PieceTable::new();
    assert_eq!(pt.insert(0, "日本語"), 3);
    assert_eq!(pt.len(), 9);
    assert_eq!(pt.char_count(), 3);
}

#[test]
fn test_locality_hint_survives_jumping_edits() {
    // Alternate edits far apart so the cached lookup position is repeatedly invalidated.
    let base = "line\n".repeat(200);
    let mut model = base.clone();
    let mut pt = PieceTable::from_string(base);
    let positions = [0usize, 900, 3, 850, 500, 501, 499, 1000, 0, 700];
    for (i, &p) in positions.iter().cycle().take(200).enumerate() {
        let p = p.min(model.chars().count());
        model_insert(&mut model, &mut pt, p, if i % 3 == 0 { "é" } else { "x\n" });
        if i % 4 == 0 {
            let n = model.chars().count();
            let s = (p * 7 + i) % n;
            model_delete(&mut model, &mut pt, s, (s + 3).min(n));
        }
    }
    assert_matches_model(&pt, &model);
}

#[test]
fn test_randomized_edits_match_string_model() {
    let mut seed: u64 = 0xDEAD_BEEF;
    let mut rnd = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let base = "héllo\nwörld\n日本語\nabc\n".repeat(10);
    let mut model = base.clone();
    let mut pt = PieceTable::from_string(base);
    let snippets = ["x", "é\n", "日本", "ab\ncd", "\n"];
    for step in 0..3000 {
        let n = model.chars().count();
        if rnd() % 2 == 0 {
            model_insert(&mut model, &mut pt, rnd() % (n + 1), snippets[rnd() % 5]);
        } else {
            let a = rnd() % (n + 1);
            let e = (a + rnd() % 9).min(n);
            model_delete(&mut model, &mut pt, a, e);
        }
        if step % 50 == 0 {
            assert_matches_model(&pt, &model);
            let probe = rnd() % (model.chars().count() + 1);
            let byte = model
                .char_indices()
                .nth(probe)
                .map_or(model.len(), |(b, _)| b);
            assert_eq!(pt.char_to_byte(probe), byte);
            assert_eq!(pt.byte_to_char(byte), probe);
        }
    }
    assert_matches_model(&pt, &model);
}

#[test]
fn test_snapshot_isolated_from_later_edits() {
    let mut pt = PieceTable::from_str("alpha beta");
    pt.insert(5, ",");
    let snap = pt.snapshot();
    pt.delete(0..3);
    pt.insert(0, "ZZ");
    pt.insert(pt.char_count(), "!");
    assert_ne!(pt, "alpha, beta");
    pt.restore_snapshot(&snap);
    assert_eq!(pt, "alpha, beta");
    assert_eq!(pt.char_count(), 11);
    assert_eq!(pt.line_count(), 1);
}

#[test]
fn test_snapshot_chain_undo_redo_order() {
    let mut pt = PieceTable::from_str("a");
    let mut snaps = vec![pt.snapshot()];
    for t in ["b", "c\n", "d"] {
        pt.insert(pt.char_count(), t);
        snaps.push(pt.snapshot());
    }
    let expected = ["a", "ab", "abc\n", "abc\nd"];
    for i in [3usize, 1, 0, 2, 3] {
        pt.restore_snapshot(&snaps[i]);
        assert_eq!(pt, expected[i]);
        assert_eq!(pt.line_count(), expected[i].matches('\n').count() + 1);
    }
}

#[test]
fn test_clone_is_independent() {
    let mut a = PieceTable::from_str("shared");
    a.insert(6, " text");
    let mut b = a.clone();
    b.delete(0..6);
    b.insert(0, "own");
    assert_eq!(a, "shared text");
    assert_eq!(b, "own text");
}

#[test]
fn test_clear_and_replace_with() {
    let mut pt = PieceTable::from_str("old\ncontent");
    pt.insert(3, "!");
    TextBuffer::replace_with(&mut pt, "new\n日本");
    assert_eq!(pt, "new\n日本");
    assert_eq!(pt.line_count(), 2);
    pt.insert(0, ">");
    assert_eq!(pt, ">new\n日本");
    pt.clear();
    assert!(pt.is_empty());
    assert_eq!(pt.piece_count(), 0);
    assert_eq!(TextBuffer::take(&mut PieceTable::from_str("xy")), "xy");
}

#[test]
fn test_line_helpers_after_edits() {
    let mut pt = PieceTable::from_str("one\ntwo\nthree");
    pt.insert(4, "2.5\n");
    // "one\n2.5\ntwo\nthree"
    assert_eq!(pt.line_count(), 4);
    assert_eq!(pt.line_to_char(1), 4);
    assert_eq!(pt.line_to_char(2), 8);
    assert_eq!(pt.line_to_char(99), pt.char_count());
    assert_eq!(pt.line_char_range(1), 4..7);
    assert_eq!(pt.line_char_range(3), 12..17);
    assert_eq!(pt.char_to_line_col(9), (2, 1));
    assert_eq!(pt.char_to_line_col(0), (0, 0));
    assert_eq!(pt.slice_chars(4..7), "2.5");
}

#[test]
fn test_write_to_streams_all_pieces() {
    let mut pt = PieceTable::from_str("abc");
    pt.insert(1, "é");
    let mut out = Vec::new();
    pt.write_to(&mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "aébc");
}

#[test]
fn test_crlf_detection_across_piece_boundary() {
    let mut pt = PieceTable::from_str("a\r");
    pt.insert(2, "\nb");
    assert!(pt.has_crlf_in_initial_bytes(4096));
    assert!(!pt.has_crlf_in_initial_bytes(2));
    assert!(!PieceTable::from_str("a\nb").has_crlf_in_initial_bytes(4096));
}

#[test]
fn test_crlf_line_char_range_and_replace_with() {
    use eframe::egui::TextBuffer;

    let pt = PieceTable::from_str("one\r\ntwo\r\nthree");
    assert_eq!(pt.line_count(), 3);
    assert_eq!(pt.line_char_range(0), 0..3);
    assert_eq!(pt.slice_chars(pt.line_char_range(0)), "one");
    assert_eq!(pt.line_char_range(1), 5..8);
    assert_eq!(pt.slice_chars(pt.line_char_range(1)), "two");
    assert_eq!(pt.line_char_range(2), 10..15);
    assert_eq!(pt.slice_chars(pt.line_char_range(2)), "three");

    // CRLF split right between '\r' and '\n' across two pieces
    let mut split = PieceTable::from_str("ab\r");
    split.insert(3, "\ncd");
    assert_eq!(split.line_count(), 2);
    assert_eq!(split.line_char_range(0), 0..2);
    assert_eq!(split.line_char_range(1), 4..6);

    // replace_with resets added buffer & piece count cleanly
    split.insert(0, &"Z".repeat(512));
    assert!(split.piece_count() > 1);
    split.replace_with("fresh");
    assert_eq!(split, "fresh");
    assert_eq!(split.char_count(), 5);
    assert_eq!(split.line_count(), 1);
    assert_eq!(split.piece_count(), 1);
}
