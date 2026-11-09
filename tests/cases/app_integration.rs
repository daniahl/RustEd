//! Integration tests for RustEd application workflows.
//!
//! Covers Find dialog and F3 search navigation, dirty buffer asterisk tracking across file switching,
//! and file explorer tree operations (folder/file creation, F2 rename, and deletion).

use eframe::egui;
use rusted::{
    app::TextEditor,
    editor::FastEditorState,
    explorer::TreeAction,
};
use std::{fs, path::Path};

#[test]
fn test_find_dialog_ctrl_f_and_f3_find_next() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_find_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    ctx.options_mut(|o| {
        o.max_passes = std::num::NonZeroUsize::new(4).unwrap();
    });
    let mut app = TextEditor::new_headless(&temp_dir);
    app.text = "alpha beta alpha gamma alpha".to_string();
    app.text_revision = 1;

    // Render initial frame
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    assert!(!app.find_open);

    // Press Ctrl+F for the very first time in the session to open Find dialog
    let mut ctrl_f_input = egui::RawInput::default();
    ctrl_f_input.events.push(egui::Event::Key {
        key: egui::Key::F,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    });
    let _ = ctx.run(ctrl_f_input, |ctx| {
        app.update_ui(ctx);
    });
    assert!(app.find_open, "Ctrl+F should open the Find dialog");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::new("find_query_input"))),
        "Find input should have focus immediately on the first Ctrl+F"
    );

    // Type search query "alpha" and press Enter in the Find dialog
    let mut type_and_enter = egui::RawInput::default();
    type_and_enter
        .events
        .push(egui::Event::Text("alpha".to_string()));
    type_and_enter.events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let out_enter = ctx.run(type_and_enter, |ctx| {
        app.update_ui(ctx);
    });
    assert!(!app.find_open, "Pressing Enter in Find dialog should close it");
    assert_eq!(
        app.text, "alpha beta alpha gamma alpha",
        "Pressing Enter in Find dialog should not insert a newline into the editor"
    );

    let editor_id = egui::Id::new("code_editor");
    assert!(
        ctx.memory(|m| m.has_focus(editor_id)),
        "Editor should have focus after Find"
    );
    // Run the immediate follow-up repaint frame that synchronizes the AccessKit text selection
    let out_sync = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let editor_ak_id: egui::accesskit::NodeId = editor_id.value().into();
    let ak_enter = out_enter.platform_output.accesskit_update.unwrap();
    let ak_sync = out_sync.platform_output.accesskit_update.unwrap();
    assert_eq!(ak_enter.focus, editor_ak_id);
    assert_eq!(ak_sync.focus, editor_ak_id);
    let editor_node_sync = ak_sync
        .nodes
        .iter()
        .find(|(id, _)| *id == editor_ak_id)
        .map(|(_, n)| n)
        .expect("Editor node should be in AccessKit update");
    let sel_sync = editor_node_sync
        .text_selection()
        .expect("Editor should report AccessKit text selection after Find");
    // "alpha" ends at column 5, +1 for leading anchor = character_index 6
    assert_eq!(sel_sync.focus.character_index, 6);
    let get_sel = |ctx: &egui::Context| -> Option<(usize, usize)> {
        let st = ctx.data_mut(|d| d.get_temp::<FastEditorState>(editor_id))?;
        let r = st.cursor.char_range()?;
        let [min_c, max_c] = r.sorted();
        Some((min_c.index, max_c.index))
    };

    assert_eq!(get_sel(&ctx), Some((0, 5)), "Find should select first 'alpha'");

    // Press Escape in the editor -> focus and selection should remain intact
    let mut esc_input = egui::RawInput::default();
    esc_input.events.push(egui::Event::Key {
        key: egui::Key::Escape,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = ctx.run(esc_input, |ctx| {
        app.update_ui(ctx);
    });
    let focused_after_esc = ctx.memory(|m| m.focused());
    assert_eq!(
        focused_after_esc,
        Some(editor_id),
        "Pressing Escape in the editor should not drop editor focus"
    );
    assert_eq!(
        get_sel(&ctx),
        Some((0, 5)),
        "Pressing Escape in the editor should keep the selection"
    );

    let mut f3_input = egui::RawInput::default();
    f3_input.events.push(egui::Event::Key {
        key: egui::Key::F3,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });

    // Press F3 -> second occurrence at char index 11..16
    let _ = ctx.run(f3_input.clone(), |ctx| {
        app.update_ui(ctx);
    });
    assert_eq!(get_sel(&ctx), Some((11, 16)), "Second F3 should select second 'alpha'");

    // Press F3 again -> third occurrence at char index 23..28
    let _ = ctx.run(f3_input.clone(), |ctx| {
        app.update_ui(ctx);
    });
    assert_eq!(get_sel(&ctx), Some((23, 28)), "Third F3 should select third 'alpha'");

    // Press F3 again -> wraps around to first occurrence at 0..5
    let _ = ctx.run(f3_input, |ctx| {
        app.update_ui(ctx);
    });
    assert_eq!(get_sel(&ctx), Some((0, 5)), "Fourth F3 should wrap around to first 'alpha'");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_unsaved_state_asterisk_and_file_switching() {
    let temp_dir = std::env::temp_dir().join(format!("rusted_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let file_a = temp_dir.join("a.rs");
    let file_b = temp_dir.join("b.rs");
    fs::write(&file_a, "fn a() {}\n").unwrap();
    fs::write(&file_b, "fn b() {}\n").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);

    // 1. Initially no file is open -> Save button cannot save
    assert!(!app.can_save());

    // 2. Open file_a -> clean, Save button disabled
    app.open_file(&ctx, file_a.clone());
    assert_eq!(app.text, "fn a() {}\n");
    assert!(!app.can_save());
    assert!(!app.is_file_dirty(&file_a));

    // Focus editor in frame 1
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
        ctx.memory_mut(|m| m.request_focus(egui::Id::new("code_editor")));
    });

    // 3. Edit file_a in frame 2
    let mut input = egui::RawInput::default();
    input.events.push(egui::Event::Text("// edited\n".to_string()));
    let _ = ctx.run(input, |ctx| {
        app.update_ui(ctx);
    });

    assert_eq!(app.text, "// edited\nfn a() {}\n");
    assert!(app.can_save(), "Save should be enabled after editing");
    assert!(app.is_file_dirty(&file_a), "file_a should be marked dirty");
    assert!(!app.is_file_dirty(&file_b), "file_b should still be clean");

    // Verify FileNode has the asterisk dirty label
    let root = app.explorer.tree_root.as_ref().unwrap();
    let node_a = root.children.iter().find(|n| n.path == file_a).unwrap();
    assert_eq!(node_a.dirty_label, "a.rs *");

    // 4. Switch to file_b without saving file_a
    app.open_file(&ctx, file_b.clone());
    assert_eq!(app.text, "fn b() {}\n");
    assert!(!app.can_save(), "Save should be disabled on unmodified file_b");
    assert!(
        app.is_file_dirty(&file_a),
        "file_a should remain dirty in state after switching to file_b"
    );
    assert!(!app.is_file_dirty(&file_b));

    // 5. Switch back to file_a -> unsaved content is restored and Save is enabled
    app.open_file(&ctx, file_a.clone());
    assert_eq!(app.text, "// edited\nfn a() {}\n");
    assert!(app.can_save(), "Save should be enabled when returning to unsaved file_a");
    assert!(app.is_file_dirty(&file_a));

    // 6. Save file_a -> written to disk, dirty cleared, Save disabled
    app.save_current_file();
    assert!(!app.can_save(), "Save should be disabled immediately after saving");
    assert!(!app.is_file_dirty(&file_a), "file_a should no longer be dirty after saving");
    assert_eq!(fs::read_to_string(&file_a).unwrap(), "// edited\nfn a() {}\n");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_tree_new_folder_new_file_rename_and_f2() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_tree_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let sub_dir = temp_dir.join("src_dir");
    fs::create_dir_all(&sub_dir).unwrap();
    let initial_file = temp_dir.join("hello.txt");
    fs::write(&initial_file, "hello").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);

    // 1. Create a subfolder inside `src_dir` via New Folder action
    app.handle_tree_action(&ctx, TreeAction::StartNewFolder(sub_dir.clone()));
    if let Some(ref mut edit) = app.explorer.pending_tree_edit {
        edit.input_text = "nested".to_string();
    }
    let edit = app.explorer.pending_tree_edit.take().unwrap();
    app.commit_tree_edit(&ctx, edit);

    let nested_dir = sub_dir.join("nested");
    assert!(nested_dir.is_dir(), "New subfolder should exist on disk");
    assert_eq!(app.explorer.selected_node, Some((nested_dir.clone(), true)));

    // 2. Create a new file inside `nested` via New File action
    app.handle_tree_action(&ctx, TreeAction::StartNewFile(nested_dir.clone()));
    if let Some(ref mut edit) = app.explorer.pending_tree_edit {
        edit.input_text = "main.rs".to_string();
    }
    let edit = app.explorer.pending_tree_edit.take().unwrap();
    app.commit_tree_edit(&ctx, edit);

    let new_file = nested_dir.join("main.rs");
    assert!(new_file.is_file(), "New file should exist on disk");
    assert_eq!(Path::new(&app.path), new_file.as_path());
    assert_eq!(app.explorer.selected_node, Some((new_file.clone(), false)));

    // 3. Edit the open file (so it has unsaved state) and press F2 to rename it
    app.text = "fn main() {}".to_string();
    app.is_current_dirty = true;

    let mut f2_input = egui::RawInput::default();
    f2_input.events.push(egui::Event::Key {
        key: egui::Key::F2,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = ctx.run(f2_input, |ctx| {
        app.update_ui(ctx);
    });

    assert!(
        app.explorer.pending_tree_edit.is_some(),
        "Pressing F2 should start renaming the selected node"
    );
    assert_eq!(
        app.explorer.pending_tree_edit.as_ref().unwrap().input_text,
        "main.rs"
    );

    // Type new name and press Enter to commit rename
    if let Some(ref mut edit) = app.explorer.pending_tree_edit {
        edit.input_text = "lib.rs".to_string();
    }
    let mut enter_input = egui::RawInput::default();
    enter_input.events.push(egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = ctx.run(enter_input, |ctx| {
        app.update_ui(ctx);
    });

    let renamed_file = nested_dir.join("lib.rs");
    assert!(!new_file.exists());
    assert!(renamed_file.is_file());
    assert_eq!(Path::new(&app.path), renamed_file.as_path());
    assert_eq!(app.file_name, "lib.rs");
    assert!(app.is_file_dirty(&renamed_file));

    // 4. Select the parent folder `src_dir`, press F2, and rename it to `app_dir`
    app.explorer.selected_node = Some((sub_dir.clone(), true));
    app.start_rename_selected();
    if let Some(ref mut edit) = app.explorer.pending_tree_edit {
        edit.input_text = "app_dir".to_string();
    }
    let edit = app.explorer.pending_tree_edit.take().unwrap();
    app.commit_tree_edit(&ctx, edit);

    let renamed_dir = temp_dir.join("app_dir");
    let moved_file = renamed_dir.join("nested").join("lib.rs");
    assert!(renamed_dir.is_dir());
    assert!(moved_file.is_file());
    assert_eq!(
        Path::new(&app.path),
        moved_file.as_path(),
        "Active file path should update when an ancestor folder is renamed"
    );

    // 5. Delete the folder via DeleteNode action (moves to Recycle Bin / Trash)
    app.handle_tree_action(
        &ctx,
        TreeAction::DeleteNode {
            path: renamed_dir.clone(),
            is_dir: true,
        },
    );
    assert!(!renamed_dir.exists(), "Deleted folder should be moved to trash");
    assert!(
        app.path.is_empty(),
        "Active editor path should be cleared when its folder is deleted"
    );
    assert!(!app.can_save());

    let _ = fs::remove_dir_all(&temp_dir);
}
