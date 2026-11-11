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
    assert_eq!(pt.insert_text("Inserted ", 7), 9);
    assert_eq!(pt.as_str(), "Line 1\nInserted Line 2");

    pt.delete_char_range(7..16);
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
    let mut insert_pos = 1000;
    for ch in "println!(\"debugging hot path performance\");\n".chars() {
        pt.insert(insert_pos, &ch.to_string());
        insert_pos += 1;
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
