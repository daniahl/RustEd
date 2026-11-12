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
    app.text = "alpha beta alpha gamma alpha".into();
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

    // Verify FileNode exists for file_a
    let root = app.explorer.tree_root.as_ref().unwrap();
    let node_a = root.children.iter().find(|n| n.path == file_a).unwrap();
    assert_eq!(node_a.name, "a.rs");

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
    app.text = "fn main() {}".into();
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

#[test]
fn test_switch_to_terminal_and_back_to_editor() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_tab_switch_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = TextEditor::new_headless(&temp_dir);

    let editor_row_id: egui::accesskit::NodeId =
        egui::Id::new("code_editor").with(0usize).value().into();

    // Frame 1: Editor tab (initial render)
    let out1 = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let u1 = out1.platform_output.accesskit_update.unwrap();
    assert!(
        u1.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Initial editor render should include active TextRun row node"
    );

    // Frame 2: Consecutive idle editor frame -> TextRun should NOT be rebuilt
    let out2 = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let u2 = out2.platform_output.accesskit_update.unwrap();
    assert!(
        !u2.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Consecutive idle editor frame should not rebuild AccessKit TextRun node"
    );

    // Frame 3: Switch to Terminal tab -> Editor removed, Terminal shown
    app.tab = rusted::app::Tab::Terminal;
    let out3 = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let u3 = out3.platform_output.accesskit_update.unwrap();

    // Frame 4: Switch back to Editor tab -> Editor re-shown after being hidden
    app.tab = rusted::app::Tab::Editor;
    let out4 = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let u4 = out4.platform_output.accesskit_update.unwrap();
    assert!(
        u4.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Switching back to editor must rebuild TextRun child so AccessKit tree stays consistent"
    );

    // Frame 5: Switch to Terminal tab again
    app.tab = rusted::app::Tab::Terminal;
    let out5 = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let u5 = out5.platform_output.accesskit_update.unwrap();

    // Frame 6: Switch back to Editor tab again
    app.tab = rusted::app::Tab::Editor;
    let out6 = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });
    let u6 = out6.platform_output.accesskit_update.unwrap();
    assert!(
        u6.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Switching back to editor again must rebuild TextRun child"
    );

    // Feed all updates into accesskit consumer Tree to ensure no panics occur
    let mut tree = accesskit_consumer::Tree::new(u1, false);
    tree.update(u2);
    tree.update(u3);
    tree.update(u4);
    tree.update(u5);
    tree.update(u6);

    let _ = fs::remove_dir_all(&temp_dir);
}


#[test]
fn test_editor_has_focus_on_startup_when_requested() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_startup_focus_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    // Headless construction does not request focus; the real app does.
    assert!(!app.focus_editor_pending);
    app.focus_editor_pending = true;

    for _ in 0..2 {
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.update_ui(ctx));
    }
    assert!(!app.focus_editor_pending, "request is consumed once");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::new("code_editor"))),
        "editor should hold keyboard focus (active caret) after the first frames"
    );

    // Focus is not forcibly re-grabbed after the user moves it elsewhere.
    ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("code_editor")));
    for _ in 0..2 {
        let _ = ctx.run(egui::RawInput::default(), |ctx| app.update_ui(ctx));
    }
    assert!(!ctx.memory(|m| m.has_focus(egui::Id::new("code_editor"))));

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_folder_from_args_picks_first_existing_folder() {
    use rusted::app::folder_from_args;
    let dir = std::env::temp_dir().join(format!("rusted_args_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("proj")).unwrap();
    fs::write(dir.join("file.txt"), "").unwrap();
    let s = |p: std::path::PathBuf| p.to_string_lossy().into_owned();

    assert_eq!(folder_from_args(Vec::<String>::new()), None);
    assert_eq!(folder_from_args(["--flag".to_string(), s(dir.join("nope"))]), None);
    // Files are ignored; the first existing folder wins.
    assert_eq!(
        folder_from_args([s(dir.join("file.txt")), s(dir.join("proj")), s(dir.clone())]),
        Some(dir.join("proj"))
    );
    // Relative paths are made absolute.
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(folder_from_args(["src".to_string()]), Some(cwd.join("src")));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_new_with_folder_uses_given_folder() {
    let dir = std::env::temp_dir().join(format!("rusted_startdir_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let app = TextEditor::new_with_folder(egui::Context::default(), Some(dir.clone()));
    assert_eq!(app.explorer.folder_path, dir.to_string_lossy());
    assert_eq!(app.explorer.tree_root.as_ref().unwrap().path, dir);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_sidebar_lists_startup_folder_contents() {
    let dir = std::env::temp_dir().join(format!("rusted_startlist_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("unique_startup_marker.txt"), "").unwrap();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = TextEditor::new_with_folder(ctx.clone(), Some(dir.clone()));
    let mut labels = Vec::new();
    for _ in 0..2 {
        let out = ctx.run(egui::RawInput::default(), |ctx| app.update_ui(ctx));
        if let Some(u) = out.platform_output.accesskit_update {
            labels.extend(u.nodes.iter().filter_map(|(_, n)| n.label().map(str::to_string)));
        }
    }
    assert!(labels.iter().any(|l| l == "unique_startup_marker.txt"), "{labels:?}");
    assert!(!labels.iter().any(|l| l == "Cargo.toml"), "cwd listing leaked: {labels:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn test_folder_from_args_bare_drive_means_drive_root() {
    use rusted::app::folder_from_args;
    let drive = std::env::current_dir()
        .unwrap()
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap();
    // e.g. "G:" must resolve to "G:\", not the current directory on that drive.
    let resolved = folder_from_args([drive.clone()]).unwrap();
    assert_eq!(resolved, std::path::PathBuf::from(format!("{drive}\\")));
    assert!(resolved.is_absolute());
}

#[test]
fn test_ctrl_tab_switches_tabs_between_editor_and_terminal() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_ctrl_tab_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    assert_eq!(app.tab, rusted::app::Tab::Editor);

    // Initial render
    let _ = ctx.run(egui::RawInput::default(), |ctx| {
        app.update_ui(ctx);
    });

    // Press Ctrl+Tab -> Should switch to Terminal
    let mut ctrl_tab_input = egui::RawInput::default();
    ctrl_tab_input.events.push(egui::Event::Key {
        key: egui::Key::Tab,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    });
    let _ = ctx.run(ctrl_tab_input, |ctx| {
        app.update_ui(ctx);
    });
    assert_eq!(app.tab, rusted::app::Tab::Terminal, "Ctrl+Tab should switch from Editor to Terminal");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::new("conpty_terminal"))),
        "Switching to Terminal should give focus to the terminal"
    );

    // Press Ctrl+Tab again -> Should switch back to Editor
    let mut ctrl_tab_input2 = egui::RawInput::default();
    ctrl_tab_input2.events.push(egui::Event::Key {
        key: egui::Key::Tab,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    });
    let _ = ctx.run(ctrl_tab_input2, |ctx| {
        app.update_ui(ctx);
    });
    assert_eq!(app.tab, rusted::app::Tab::Editor, "Ctrl+Tab should switch from Terminal to Editor");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::new("code_editor"))),
        "Switching to Editor should give focus to the code editor"
    );

    // Press Ctrl+Shift+Tab -> Should switch to Terminal as well
    let mut ctrl_shift_tab_input = egui::RawInput::default();
    ctrl_shift_tab_input.events.push(egui::Event::Key {
        key: egui::Key::Tab,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        },
    });
    let _ = ctx.run(ctrl_shift_tab_input, |ctx| {
        app.update_ui(ctx);
    });
    assert_eq!(app.tab, rusted::app::Tab::Terminal, "Ctrl+Shift+Tab should switch between tabs");

    let _ = fs::remove_dir_all(&temp_dir);
}
