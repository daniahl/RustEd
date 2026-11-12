//! Unit tests for the `EditorBuffer` abstraction: the `String` default implementation and
//! the optimized `PieceTable` implementation must behave identically.

use eframe::egui::{text::CCursor, TextBuffer};
use rusted::{
    editor::{EditorBuffer, UndoSnapshot},
    piece_table::PieceTable,
};

const SAMPLE: &str = "alpha beta\n日本語 gamma\n\nlast line";

fn both() -> (String, PieceTable) {
    (SAMPLE.to_string(), PieceTable::from_str(SAMPLE))
}

#[test]
fn test_counts_agree() {
    let (s, pt) = both();
    assert_eq!(EditorBuffer::char_count(&s), EditorBuffer::char_count(&pt));
    assert_eq!(EditorBuffer::line_count(&s), 4);
    assert_eq!(EditorBuffer::line_count(&pt), 4);
}

#[test]
fn test_line_to_char_agrees_including_out_of_range() {
    let (s, pt) = both();
    for line in 0..8 {
        assert_eq!(
            EditorBuffer::line_to_char(&s, line),
            EditorBuffer::line_to_char(&pt, line),
            "line {line}"
        );
    }
    assert_eq!(EditorBuffer::line_to_char(&s, 1), 11);
}

#[test]
fn test_char_to_line_col_agrees() {
    let (s, pt) = both();
    let total = EditorBuffer::char_count(&s);
    for i in 0..=total + 2 {
        assert_eq!(
            EditorBuffer::char_to_line_col(&s, i),
            EditorBuffer::char_to_line_col(&pt, i),
            "char {i}"
        );
    }
    assert_eq!(EditorBuffer::char_to_line_col(&s, 12), (1, 1));
}

#[test]
fn test_line_char_range_agrees() {
    let (s, pt) = both();
    for line in 0..5 {
        assert_eq!(
            EditorBuffer::line_char_range(&s, line),
            EditorBuffer::line_char_range(&pt, line),
            "line {line}"
        );
    }
    assert_eq!(EditorBuffer::line_char_range(&s, 0), 0..10);
    assert_eq!(EditorBuffer::line_char_range(&s, 2), 21..21);
}

#[test]
fn test_slice_and_extract_agree_and_clamp() {
    let (s, pt) = both();
    assert_eq!(EditorBuffer::slice_chars(&s, 11..14), "日本語");
    assert_eq!(EditorBuffer::slice_chars(&pt, 11..14), "日本語");
    assert_eq!(EditorBuffer::slice_chars(&s, 5..5), "");
    assert_eq!(EditorBuffer::slice_chars(&pt, 5..5), "");
    assert_eq!(
        EditorBuffer::slice_chars(&s, 20..9999),
        EditorBuffer::slice_chars(&pt, 20..9999)
    );

    let mut out = String::from("stale");
    EditorBuffer::extract_char_range(&pt, 0..5, &mut out);
    assert_eq!(out, "alpha");
}

#[test]
fn test_delete_prev_word_agrees() {
    for cursor in [0usize, 5, 10, 11, 14, 15, 20, 31] {
        let (mut s, mut pt) = both();
        let a = EditorBuffer::delete_prev_word(&mut s, CCursor::new(cursor));
        let b = EditorBuffer::delete_prev_word(&mut pt, CCursor::new(cursor));
        assert_eq!(a.index, b.index, "cursor {cursor}");
        assert_eq!(s, pt, "cursor {cursor}");
    }
}

#[test]
fn test_delete_next_word_agrees() {
    for cursor in [0usize, 3, 5, 10, 11, 14, 20, 31] {
        let (mut s, mut pt) = both();
        let a = EditorBuffer::delete_nxt_word(&mut s, CCursor::new(cursor));
        let b = EditorBuffer::delete_nxt_word(&mut pt, CCursor::new(cursor));
        assert_eq!(a.index, b.index, "cursor {cursor}");
        assert_eq!(s, pt, "cursor {cursor}");
    }
}

#[test]
fn test_delete_prev_word_with_long_whitespace_run_grows_window() {
    let text = format!("word{}tail", " ".repeat(1000));
    let mut pt = PieceTable::from_str(&text);
    let mut s = text.clone();
    let end = EditorBuffer::char_count(&s) - 4; // cursor right before "tail"
    let a = EditorBuffer::delete_prev_word(&mut s, CCursor::new(end));
    let b = EditorBuffer::delete_prev_word(&mut pt, CCursor::new(end));
    assert_eq!(a.index, b.index);
    assert_eq!(s, pt);
}

#[test]
fn test_string_undo_snapshot_roundtrip() {
    let mut s = String::from("before");
    let snap = EditorBuffer::undo_snapshot(&s);
    assert!(matches!(snap, UndoSnapshot::FullText(_)));
    s.push_str(" after");
    EditorBuffer::restore_undo_snapshot(&mut s, &snap);
    assert_eq!(s, "before");
}

#[test]
fn test_piece_table_undo_snapshot_roundtrip() {
    let mut pt = PieceTable::from_str("before");
    let snap = EditorBuffer::undo_snapshot(&pt);
    assert!(matches!(snap, UndoSnapshot::PieceTable(_)));
    pt.insert_text(" after", 6);
    pt.delete_char_range(0..2);
    EditorBuffer::restore_undo_snapshot(&mut pt, &snap);
    assert_eq!(pt, "before");
}

#[test]
fn test_piece_table_accepts_full_text_snapshot() {
    let mut pt = PieceTable::from_str("current");
    let snap = UndoSnapshot::FullText("from text".into());
    EditorBuffer::restore_undo_snapshot(&mut pt, &snap);
    assert_eq!(pt, "from text");
}

#[test]
fn test_string_ignores_piece_table_snapshot() {
    let pt = PieceTable::from_str("pt");
    let snap = EditorBuffer::undo_snapshot(&pt);
    let mut s = String::from("unchanged");
    EditorBuffer::restore_undo_snapshot(&mut s, &snap);
    assert_eq!(s, "unchanged");
}

#[test]
fn test_undo_snapshot_equality() {
    let pt = PieceTable::from_str("same");
    assert_eq!(EditorBuffer::undo_snapshot(&pt), EditorBuffer::undo_snapshot(&pt));
    let mut other = pt.clone();
    other.insert_text("!", 4);
    assert_ne!(EditorBuffer::undo_snapshot(&pt), EditorBuffer::undo_snapshot(&other));
}

#[test]
fn test_piece_table_word_deletion_clamps_out_of_range_cursor() {
    let mut pt = PieceTable::from_str("hello world");
    let c = EditorBuffer::delete_nxt_word(&mut pt, CCursor::new(500));
    assert_eq!(c.index, 11);
    assert_eq!(pt, "hello world");

    let c = EditorBuffer::delete_prev_word(&mut pt, CCursor::new(500));
    assert_eq!(c.index, 6);
    assert_eq!(pt, "hello ");

    let mut empty = PieceTable::new();
    assert_eq!(EditorBuffer::delete_prev_word(&mut empty, CCursor::new(3)).index, 0);
}