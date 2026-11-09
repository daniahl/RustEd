//! Unit tests for the `document` module.
//!
//! Covers language detection, stack-allocated number formatting, language salt hashing,
//! line ending sequence detection, unicode/wrap-around search in `find_next`,
//! and path renaming / deletion state updates.

use eframe::egui;
use rusted::document::{
    detect_language_name, format_u32_stack, hash_language_salt, Document, UnsavedFileState,
};
use std::path::{Path, PathBuf};

#[test]
fn test_detect_language_name_known_and_case_insensitive() {
    // Standard extensions
    assert_eq!(detect_language_name("main.rs"), "Rust");
    assert_eq!(detect_language_name("Cargo.toml"), "TOML");
    assert_eq!(detect_language_name("package.json"), "JSON");
    assert_eq!(detect_language_name("README.md"), "Markdown");
    assert_eq!(detect_language_name("guide.markdown"), "Markdown");
    assert_eq!(detect_language_name("script.py"), "Python");
    assert_eq!(detect_language_name("index.js"), "JavaScript");
    assert_eq!(detect_language_name("app.ts"), "TypeScript");
    assert_eq!(detect_language_name("Component.jsx"), "JavaScript React");
    assert_eq!(detect_language_name("Component.tsx"), "TypeScript React");
    assert_eq!(detect_language_name("page.html"), "HTML");
    assert_eq!(detect_language_name("style.css"), "CSS");
    assert_eq!(detect_language_name("main.c"), "C");
    assert_eq!(detect_language_name("header.h"), "C");
    assert_eq!(detect_language_name("main.cpp"), "C++");
    assert_eq!(detect_language_name("Program.cs"), "C#");
    assert_eq!(detect_language_name("server.go"), "Go");
    assert_eq!(detect_language_name("App.java"), "Java");
    assert_eq!(detect_language_name("build.sh"), "Shell Script");
    assert_eq!(detect_language_name("run.bat"), "Batch");
    assert_eq!(detect_language_name("run.cmd"), "Batch");
    assert_eq!(detect_language_name("config.yaml"), "YAML");
    assert_eq!(detect_language_name("config.yml"), "YAML");
    assert_eq!(detect_language_name("data.xml"), "XML");
    assert_eq!(detect_language_name("query.sql"), "SQL");
    assert_eq!(detect_language_name("script.lua"), "Lua");
    assert_eq!(detect_language_name("analysis.r"), "R");
    assert_eq!(detect_language_name("script.rb"), "Ruby");
    assert_eq!(detect_language_name("index.php"), "PHP");
    assert_eq!(detect_language_name("notes.txt"), "Plain Text");

    // Case insensitivity
    assert_eq!(detect_language_name("MAIN.RS"), "Rust");
    assert_eq!(detect_language_name("CONFIG.TOML"), "TOML");
    assert_eq!(detect_language_name("Script.Py"), "Python");

    // Multi-dot filenames
    assert_eq!(detect_language_name("app.component.ts"), "TypeScript");
    assert_eq!(detect_language_name("archive.tar.gz"), "Text");

    // Missing extensions and dotfiles
    assert_eq!(detect_language_name(""), "Plain Text");
    assert_eq!(detect_language_name(".gitignore"), "Text");
    assert_eq!(detect_language_name("Dockerfile"), "Text");
    assert_eq!(detect_language_name("mystery_file.xyz123"), "Text");
}

#[test]
fn test_format_u32_stack() {
    let mut buf = [0u8; 16];

    assert_eq!(format_u32_stack(0, &mut buf), "0");
    assert_eq!(format_u32_stack(1, &mut buf), "1");
    assert_eq!(format_u32_stack(9, &mut buf), "9");
    assert_eq!(format_u32_stack(10, &mut buf), "10");
    assert_eq!(format_u32_stack(42, &mut buf), "42");
    assert_eq!(format_u32_stack(100, &mut buf), "100");
    assert_eq!(format_u32_stack(1000, &mut buf), "1000");
    assert_eq!(format_u32_stack(999999, &mut buf), "999999");
    assert_eq!(format_u32_stack(u32::MAX, &mut buf), "4294967295");
}

#[test]
fn test_hash_language_salt() {
    // Deterministic hashing
    let rs1 = hash_language_salt("rs");
    let rs2 = hash_language_salt("rs");
    assert_eq!(rs1, rs2);

    // Different languages produce different salts
    let py = hash_language_salt("py");
    let toml = hash_language_salt("toml");
    assert_ne!(rs1, py);
    assert_ne!(rs1, toml);
    assert_ne!(py, toml);
}

#[test]
fn test_refresh_line_ending() {
    let mut doc = Document::new();

    // Default empty text
    doc.refresh_line_ending();
    assert_eq!(doc.line_ending, "LF");

    // Pure LF text
    doc.text = "line1\nline2\nline3\n".to_string();
    doc.refresh_line_ending();
    assert_eq!(doc.line_ending, "LF");

    // CRLF text
    doc.text = "line1\r\nline2\r\n".to_string();
    doc.refresh_line_ending();
    assert_eq!(doc.line_ending, "CRLF");

    // Mixed text with CRLF early
    doc.text = "first\r\nsecond\nthird".to_string();
    doc.refresh_line_ending();
    assert_eq!(doc.line_ending, "CRLF");

    // Large file with CRLF placed after the 4096-byte check window -> detected as LF
    let mut large_text = "a".repeat(5000);
    large_text.push_str("\r\nb");
    doc.text = large_text;
    doc.refresh_line_ending();
    assert_eq!(doc.line_ending, "LF");

    // Large file with CRLF within the first 4096 bytes -> detected as CRLF
    let mut large_crlf = "a".repeat(100);
    large_crlf.push_str("\r\n");
    large_crlf.push_str(&"b".repeat(5000));
    doc.text = large_crlf;
    doc.refresh_line_ending();
    assert_eq!(doc.line_ending, "CRLF");
}

#[test]
fn test_find_next_unicode_and_wraparound() {
    let ctx = egui::Context::default();
    let mut doc = Document::new();

    // Multibyte emojis and accented letters:
    // "🦀" (4 bytes, 1 char)
    // "café" (5 bytes, 4 chars)
    doc.text = "🦀 Rust café 🦀 Rust café 🦀".to_string();
    doc.text_revision = 1;

    let editor_id = egui::Id::new("code_editor");
    let get_sel = |ctx: &egui::Context| -> Option<(usize, usize)> {
        let st = ctx.data_mut(|d| d.get_temp::<rusted::editor::FastEditorState>(editor_id))?;
        let r = st.cursor.char_range()?;
        let [min_c, max_c] = r.sorted();
        Some((min_c.index, max_c.index))
    };

    // 1. Empty query returns false
    doc.find_query = String::new();
    assert!(!doc.find_next(&ctx));

    // 2. Non-existent query returns false
    doc.find_query = "python".to_string();
    assert!(!doc.find_next(&ctx));

    // 3. Search for emoji "🦀"
    doc.find_query = "🦀".to_string();

    // Match 1: char 0..1
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((0, 1)));

    // Match 2: char 12..13
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((12, 13)));

    // Match 3: char 24..25
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((24, 25)));

    // Match 4: Wraps around to 0..1
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((0, 1)));

    // 4. Search for "café"
    doc.find_query = "café".to_string();

    // Match 1: "café" at char 7..11
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((7, 11)));

    // Match 2: "café" at char 19..23
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((19, 23)));

    // Match 3: Wraps around back to 7..11
    assert!(doc.find_next(&ctx));
    assert_eq!(get_sel(&ctx), Some((7, 11)));
}

#[test]
fn test_document_update_path_if_renamed() {
    let mut doc = Document::new();

    // Active file is inside a subdirectory
    let old_dir = PathBuf::from("my_project").join("src");
    let old_file = old_dir.join("main.rs");
    doc.path = old_file.to_string_lossy().into_owned();
    doc.refresh_file_metadata();
    assert_eq!(doc.file_name, "main.rs");
    assert_eq!(doc.language_name, "Rust");

    // Also simulate an unsaved file in the same project directory
    let other_file = old_dir.join("lib.rs");
    doc.unsaved_files.insert(
        other_file.clone(),
        UnsavedFileState {
            text: "// dirty".to_string(),
            saved_text: "".to_string(),
            editor_state: None,
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
        },
    );

    // 1. Rename ancestor directory "my_project" -> "renamed_project"
    let new_project = PathBuf::from("renamed_project");
    doc.update_path_if_renamed(Path::new("my_project"), &new_project, true);

    let expected_active = new_project.join("src").join("main.rs");
    let expected_other = new_project.join("src").join("lib.rs");

    assert_eq!(Path::new(&doc.path), expected_active.as_path());
    assert_eq!(doc.file_name, "main.rs");
    assert!(doc.unsaved_files.contains_key(&expected_other));
    assert!(!doc.unsaved_files.contains_key(&other_file));

    // 2. Direct rename of the active file
    let final_file = new_project.join("src").join("app.py");
    doc.update_path_if_renamed(&expected_active, &final_file, false);
    assert_eq!(Path::new(&doc.path), final_file.as_path());
    assert_eq!(doc.file_name, "app.py");
    assert_eq!(doc.language_name, "Python");
}

#[test]
fn test_document_close_if_deleted() {
    let ctx = egui::Context::default();
    let mut doc = Document::new();

    let project_dir = PathBuf::from("temp_proj");
    let active_file = project_dir.join("active.rs");
    let other_file = project_dir.join("other.rs");
    let external_file = PathBuf::from("other_proj").join("ext.rs");

    doc.path = active_file.to_string_lossy().into_owned();
    doc.text = "fn main() {}".to_string();
    doc.saved_text = "fn main() {}".to_string();
    doc.is_current_dirty = false;
    doc.refresh_file_metadata();

    // Populate unsaved files: one under project_dir and one outside
    doc.unsaved_files.insert(
        other_file.clone(),
        UnsavedFileState {
            text: "dirty".to_string(),
            saved_text: "".to_string(),
            editor_state: None,
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
        },
    );
    doc.unsaved_files.insert(
        external_file.clone(),
        UnsavedFileState {
            text: "external".to_string(),
            saved_text: "".to_string(),
            editor_state: None,
            line_count: 1,
            cursor_line: 1,
            cursor_col: 1,
            selected_chars: 0,
        },
    );

    // 1. Deleting an unrelated file does not close active file
    doc.close_if_deleted(&ctx, Path::new("unrelated.rs"), false);
    assert_eq!(Path::new(&doc.path), active_file.as_path());
    assert_eq!(doc.text, "fn main() {}");
    assert_eq!(doc.unsaved_files.len(), 2);

    // 2. Deleting project_dir closes active file and removes all project_dir unsaved files
    doc.close_if_deleted(&ctx, &project_dir, true);
    assert!(doc.path.is_empty(), "Active path should be cleared");
    assert!(doc.text.is_empty(), "Active text should be cleared");
    assert_eq!(doc.file_name, "Untitled");
    assert!(!doc.unsaved_files.contains_key(&other_file));
    assert!(
        doc.unsaved_files.contains_key(&external_file),
        "External unsaved file should remain intact"
    );
}
