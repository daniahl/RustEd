//! Unit tests for the `explorer` module.
//!
//! Covers tree node hierarchy sorting (directories first, case-insensitive),
//! recursive `find_mut` node resolution, and input validation / collision handling
//! for file and directory creation and renaming.

use eframe::egui;
use rusted::{
    document::Document,
    explorer::{FileExplorer, FileNode},
};
use std::fs;

#[test]
fn test_filenode_load_children_sorting_dirs_first_case_insensitive() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_sort_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    // Create directories with mixed cases
    fs::create_dir(temp_dir.join("src")).unwrap();
    fs::create_dir(temp_dir.join("docs")).unwrap();
    fs::create_dir(temp_dir.join("Assets")).unwrap();

    // Create files with mixed cases
    fs::write(temp_dir.join("z_last.txt"), "").unwrap();
    fs::write(temp_dir.join("A_first.txt"), "").unwrap();
    fs::write(temp_dir.join("middle.rs"), "").unwrap();
    fs::write(temp_dir.join("build.rs"), "").unwrap();

    let root = FileNode::new_root(temp_dir.clone());

    let names: Vec<String> = root.children.iter().map(|c| c.name.clone()).collect();
    let are_dirs: Vec<bool> = root.children.iter().map(|c| c.is_dir).collect();

    // The first 3 should be directories
    assert_eq!(&are_dirs[..3], &[true, true, true]);
    assert_eq!(&names[..3], &["Assets", "docs", "src"]);

    // The next 4 should be files
    assert_eq!(&are_dirs[3..], &[false, false, false, false]);
    assert_eq!(
        &names[3..],
        &["A_first.txt", "build.rs", "middle.rs", "z_last.txt"]
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_filenode_find_mut_deep_hierarchy() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_find_node_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let level1 = temp_dir.join("level1");
    let level2 = level1.join("level2");
    let level3 = level2.join("level3");
    fs::create_dir_all(&level3).unwrap();
    let deep_file = level3.join("nested.rs");
    fs::write(&deep_file, "// code").unwrap();

    let mut root = FileNode::new_root(temp_dir.clone());

    // Expand intermediate nodes to load children
    let node1 = root.find_mut(&level1).expect("level1 should be found");
    node1.expanded = true;
    node1.load_children();

    let node2 = root.find_mut(&level2).expect("level2 should be found");
    node2.expanded = true;
    node2.load_children();

    let node3 = root.find_mut(&level3).expect("level3 should be found");
    node3.expanded = true;
    node3.load_children();

    // Verify finding the deep file
    let found_file = root.find_mut(&deep_file);
    assert!(found_file.is_some());
    assert_eq!(found_file.unwrap().name, "nested.rs");

    // Non-existent path returns None
    assert!(root.find_mut(&temp_dir.join("phantom")).is_none());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_create_and_rename_validation_and_collisions() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_validation_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(temp_dir.clone());
    let mut doc = Document::new();

    // 1. Validation for create_folder_in
    assert!(explorer.create_folder_in(&temp_dir, "").is_none());
    assert!(explorer.create_folder_in(&temp_dir, "   ").is_none());
    assert!(explorer.create_folder_in(&temp_dir, "invalid/name").is_none());
    assert!(explorer.create_folder_in(&temp_dir, "invalid\\name").is_none());

    let valid_dir = explorer.create_folder_in(&temp_dir, "valid_folder");
    assert!(valid_dir.is_some());
    assert!(valid_dir.unwrap().is_dir());

    // Duplicate directory collision returns None
    assert!(explorer.create_folder_in(&temp_dir, "valid_folder").is_none());

    // 2. Validation for create_file_in
    assert!(explorer.create_file_in(&ctx, &mut doc, &temp_dir, "").is_none());
    assert!(explorer.create_file_in(&ctx, &mut doc, &temp_dir, "nested/bad.txt").is_none());
    assert!(explorer.create_file_in(&ctx, &mut doc, &temp_dir, "valid_folder").is_none()); // Collides with existing dir

    let valid_file = explorer.create_file_in(&ctx, &mut doc, &temp_dir, "test.rs");
    assert!(valid_file.is_some());
    assert!(valid_file.as_ref().unwrap().is_file());
    assert_eq!(doc.file_name, "test.rs");

    // Duplicate file collision returns None
    assert!(explorer.create_file_in(&ctx, &mut doc, &temp_dir, "test.rs").is_none());

    // 3. Validation for rename_node
    let test_file_path = valid_file.unwrap();

    // Invalid rename names
    assert!(explorer.rename_node(&mut doc, &test_file_path, "", false).is_none());
    assert!(explorer.rename_node(&mut doc, &test_file_path, "sub/dir.rs", false).is_none());
    // Rename to existing directory or file collision returns None
    assert!(explorer.rename_node(&mut doc, &test_file_path, "valid_folder", false).is_none());

    // Valid rename
    let renamed = explorer.rename_node(&mut doc, &test_file_path, "renamed.rs", false);
    assert!(renamed.is_some());
    assert_eq!(doc.file_name, "renamed.rs");
    assert!(temp_dir.join("renamed.rs").is_file());
    assert!(!temp_dir.join("test.rs").exists());

    let _ = fs::remove_dir_all(&temp_dir);
}
