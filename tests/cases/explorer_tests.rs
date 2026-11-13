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

// ---------------------------------------------------------------------------
// Additional coverage: tree actions, selection, folder navigation, commit
// ---------------------------------------------------------------------------

use rusted::explorer::{PendingTreeEdit, TreeAction, TreeEditKind};
use std::path::PathBuf;

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rusted_{}_{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_new_root_name_and_visible_count() {
    let dir = fresh_dir("visible");
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("inner.txt"), "").unwrap();
    fs::write(dir.join("a.txt"), "").unwrap();
    fs::write(dir.join("b.txt"), "").unwrap();

    let mut root = FileNode::new_root(dir.clone());
    assert!(root.is_dir && root.expanded && root.children_loaded);
    assert_eq!(root.name, dir.file_name().unwrap().to_string_lossy());
    // root + sub (collapsed) + 2 files
    assert_eq!(root.visible_count(), 4);

    let sub = root.find_mut(&dir.join("sub")).unwrap();
    sub.expanded = true;
    sub.load_children();
    assert_eq!(root.visible_count(), 5);

    root.expanded = false;
    assert_eq!(root.visible_count(), 1);
    // A file node is always a single row.
    let file = root.find_mut(&dir.join("a.txt")).unwrap();
    assert_eq!(file.visible_count(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_load_children_preserves_expanded_subtrees_and_picks_up_changes() {
    let dir = fresh_dir("reload");
    fs::create_dir(dir.join("sub")).unwrap();
    fs::write(dir.join("sub").join("inner.txt"), "").unwrap();

    let mut root = FileNode::new_root(dir.clone());
    {
        let sub = root.find_mut(&dir.join("sub")).unwrap();
        sub.expanded = true;
        sub.load_children();
    }
    fs::write(dir.join("new.txt"), "").unwrap();
    root.load_children();

    assert!(root.children.iter().any(|c| c.name == "new.txt"));
    let sub = root.find_mut(&dir.join("sub")).unwrap();
    assert!(sub.expanded);
    assert_eq!(sub.children.len(), 1);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_load_children_of_missing_dir_is_empty() {
    let dir = fresh_dir("missing");
    let mut root = FileNode::new_root(dir.clone());
    fs::remove_dir_all(&dir).unwrap();
    root.load_children();
    assert!(root.children.is_empty());
}

#[test]
fn test_start_new_file_and_folder_actions_create_pending_edit() {
    let dir = fresh_dir("start_new");
    fs::create_dir(dir.join("sub")).unwrap();
    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();

    explorer.handle_tree_action(&ctx, &mut doc, TreeAction::StartNewFolder(dir.join("sub")));
    let edit = explorer.pending_tree_edit.as_ref().unwrap();
    assert!(edit.input_text.is_empty() && !edit.had_focus);
    assert!(matches!(&edit.kind, TreeEditKind::NewFolder { parent_path } if *parent_path == dir.join("sub")));
    // Target folder is expanded and loaded so the inline editor row is visible.
    let sub = explorer.tree_root.as_mut().unwrap().find_mut(&dir.join("sub")).unwrap();
    assert!(sub.expanded && sub.children_loaded);

    explorer.handle_tree_action(&ctx, &mut doc, TreeAction::StartNewFile(dir.clone()));
    assert!(matches!(
        &explorer.pending_tree_edit.as_ref().unwrap().kind,
        TreeEditKind::NewFile { parent_path } if *parent_path == dir
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_start_rename_action_selects_and_prefills_name() {
    let dir = fresh_dir("start_rename");
    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();
    let target = dir.join("file.txt");

    explorer.handle_tree_action(
        &ctx,
        &mut doc,
        TreeAction::StartRename {
            path: target.clone(),
            current_name: "file.txt".to_string(),
            is_dir: false,
        },
    );
    assert_eq!(explorer.selected_node, Some((target.clone(), false)));
    let edit = explorer.pending_tree_edit.as_ref().unwrap();
    assert_eq!(edit.input_text, "file.txt");
    assert!(matches!(&edit.kind, TreeEditKind::Rename { target_path, is_dir: false } if *target_path == target));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_start_rename_selected_requires_selection() {
    let dir = fresh_dir("rename_sel");
    let mut explorer = FileExplorer::new(dir.clone());
    explorer.start_rename_selected();
    assert!(explorer.pending_tree_edit.is_none());

    explorer.selected_node = Some((dir.join("x.rs"), false));
    explorer.start_rename_selected();
    let edit = explorer.pending_tree_edit.as_ref().unwrap();
    assert_eq!(edit.input_text, "x.rs");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_create_folder_selects_it_and_refreshes_tree() {
    let dir = fresh_dir("create_select");
    let mut explorer = FileExplorer::new(dir.clone());
    let created = explorer.create_folder_in(&dir, "  padded  ").unwrap();
    assert_eq!(created, dir.join("padded"));
    assert_eq!(explorer.selected_node, Some((created.clone(), true)));
    let root = explorer.tree_root.as_ref().unwrap();
    assert!(root.children.iter().any(|c| c.name == "padded" && c.is_dir));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_rename_directory_updates_selection_and_open_document() {
    let dir = fresh_dir("rename_dir");
    let old = dir.join("old_dir");
    fs::create_dir(&old).unwrap();
    fs::write(old.join("inside.txt"), "hello").unwrap();

    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();
    doc.open_file(&ctx, old.join("inside.txt"));
    explorer.selected_node = Some((old.join("inside.txt"), false));
    explorer.tree_root.as_mut().unwrap().find_mut(&old).unwrap().expanded = true;

    let new = explorer.rename_node(&mut doc, &old, "new_dir", true).unwrap();
    assert_eq!(new, dir.join("new_dir"));
    assert!(new.join("inside.txt").is_file());
    assert_eq!(
        explorer.selected_node.as_ref().unwrap().0,
        new.join("inside.txt")
    );
    assert_eq!(PathBuf::from(&doc.path), new.join("inside.txt"));
    // Expanded state carries over to the renamed node.
    assert!(explorer.tree_root.as_mut().unwrap().find_mut(&new).unwrap().expanded);

    // Renaming to the same name is rejected.
    assert!(explorer.rename_node(&mut doc, &new, "new_dir", true).is_none());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_commit_tree_edit_dispatches_each_kind() {
    let dir = fresh_dir("commit");
    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();

    explorer.commit_tree_edit(
        &ctx,
        &mut doc,
        PendingTreeEdit {
            kind: TreeEditKind::NewFolder { parent_path: dir.clone() },
            input_text: "folder".to_string(),
            had_focus: true,
        },
    );
    assert!(dir.join("folder").is_dir());

    explorer.commit_tree_edit(
        &ctx,
        &mut doc,
        PendingTreeEdit {
            kind: TreeEditKind::NewFile { parent_path: dir.clone() },
            input_text: "f.txt".to_string(),
            had_focus: true,
        },
    );
    assert!(dir.join("f.txt").is_file());
    assert_eq!(doc.file_name, "f.txt");

    explorer.commit_tree_edit(
        &ctx,
        &mut doc,
        PendingTreeEdit {
            kind: TreeEditKind::Rename { target_path: dir.join("folder"), is_dir: true },
            input_text: "renamed".to_string(),
            had_focus: true,
        },
    );
    assert!(dir.join("renamed").is_dir() && !dir.join("folder").exists());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_open_folder_and_parent_reset_state() {
    let dir = fresh_dir("open_folder");
    let child = dir.join("child");
    fs::create_dir(&child).unwrap();

    let mut explorer = FileExplorer::new(dir.clone());
    explorer.selected_node = Some((dir.join("x"), false));
    explorer.pending_tree_edit = Some(PendingTreeEdit {
        kind: TreeEditKind::NewFile { parent_path: dir.clone() },
        input_text: String::new(),
        had_focus: false,
    });

    explorer.open_folder(child.clone());
    assert_eq!(PathBuf::from(&explorer.folder_path), child);
    assert!(explorer.selected_node.is_none() && explorer.pending_tree_edit.is_none());
    assert_eq!(explorer.tree_root.as_ref().unwrap().path, child);

    explorer.open_parent_folder();
    assert_eq!(PathBuf::from(&explorer.folder_path), dir);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_root_level_creation_via_tree_actions() {
    let dir = fresh_dir("root_create");
    fs::create_dir(dir.join("existing")).unwrap();
    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();
    // Something else is selected: root-level creation must ignore it.
    explorer.selected_node = Some((dir.join("existing"), true));

    explorer.handle_tree_action(&ctx, &mut doc, TreeAction::StartNewFolder(dir.clone()));
    let edit = explorer.pending_tree_edit.take().unwrap();
    explorer.commit_tree_edit(&ctx, &mut doc, PendingTreeEdit { input_text: "top_dir".into(), ..edit });
    assert!(dir.join("top_dir").is_dir());

    explorer.handle_tree_action(&ctx, &mut doc, TreeAction::StartNewFile(dir.clone()));
    let edit = explorer.pending_tree_edit.take().unwrap();
    explorer.commit_tree_edit(&ctx, &mut doc, PendingTreeEdit { input_text: "top.txt".into(), ..edit });
    assert!(dir.join("top.txt").is_file());
    assert!(!dir.join("existing").join("top.txt").exists());

    let names: Vec<_> = explorer
        .tree_root
        .as_ref()
        .unwrap()
        .children
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert!(names.contains(&"top_dir".to_string()) && names.contains(&"top.txt".to_string()));

    let _ = fs::remove_dir_all(&dir);
}

fn run_sidebar_frame(
    ctx: &egui::Context,
    explorer: &mut FileExplorer,
    doc: &mut Document,
    events: Vec<egui::Event>,
    labels: &mut Vec<String>,
) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 700.0))),
        events,
        ..Default::default()
    };
    let mut out = ctx.run_ui(input, |ui| explorer.show_sidebar(ui, doc));
    out.textures_delta.clear();
    if let Some(update) = out.platform_output.accesskit_update {
        for (_, node) in update.nodes {
            if let Some(label) = node.label() {
                labels.push(label.to_string());
            }
        }
    }
}

#[test]
fn test_right_click_on_empty_sidebar_space_opens_context_menu() {
    let dir = fresh_dir("empty_menu");
    fs::write(dir.join("only.txt"), "").unwrap();
    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();
    let mut labels: Vec<String> = Vec::new();
    ctx.enable_accesskit();
    explorer.selected_node = Some((dir.join("only.txt"), false));

    run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![], &mut labels);
    run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![], &mut labels);
    assert!(!labels.iter().any(|l| l == "New File"));

    let pos = egui::pos2(60.0, 500.0); // well below the single tree row
    let click = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![egui::Event::PointerMoved(pos)], &mut labels);
    run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![click(true)], &mut labels);
    run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![click(false)], &mut labels);
    run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![], &mut labels);

    assert!(
        labels.iter().any(|l| l == "New File") && labels.iter().any(|l| l == "New Folder"),
        "context menu should open on empty explorer space, saw {labels:?}"
    );
    assert!(explorer.selected_node.is_none(), "empty-space click clears selection");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_pending_root_edit_renders_without_panicking() {
    let dir = fresh_dir("root_edit_render");
    let ctx = egui::Context::default();
    let mut explorer = FileExplorer::new(dir.clone());
    let mut doc = Document::new();
    let mut labels: Vec<String> = Vec::new();
    for action in [
        TreeAction::StartNewFolder(dir.clone()),
        TreeAction::StartNewFile(dir.clone()),
    ] {
        explorer.handle_tree_action(&ctx, &mut doc, action);
        run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![], &mut labels);
        run_sidebar_frame(&ctx, &mut explorer, &mut doc, vec![], &mut labels);
        assert!(explorer.pending_tree_edit.is_some());
    }
    let _ = fs::remove_dir_all(&dir);
}
