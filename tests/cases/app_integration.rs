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
    app.doc.text = "alpha beta alpha gamma alpha".into();
    app.doc.text_revision = 1;

    // Render initial frame
    let _ = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    assert!(!app.doc.find_open);

    // Press Ctrl+F for the very first time in the session to open Find dialog
    let mut ctrl_f_input = egui::RawInput::default();
    ctrl_f_input.events.push(egui::Event::Key {
        key: egui::Key::F,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    });
    let _ = run_ui(&ctx, ctrl_f_input, |ui| {
        app.update_ui(ui);
    });
    assert!(app.doc.find_open, "Ctrl+F should open the Find dialog");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::unique("find_query_input"))),
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
    let out_enter = run_ui(&ctx, type_and_enter, |ui| {
        app.update_ui(ui);
    });
    assert!(!app.doc.find_open, "Pressing Enter in Find dialog should close it");
    assert_eq!(
        app.doc.text, "alpha beta alpha gamma alpha",
        "Pressing Enter in Find dialog should not insert a newline into the editor"
    );

    let editor_id = egui::Id::unique("code_editor");
    assert!(
        ctx.memory(|m| m.has_focus(editor_id)),
        "Editor should have focus after Find"
    );
    // Run the immediate follow-up repaint frame that synchronizes the AccessKit text selection
    let out_sync = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
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
        let [min_c, max_c] = r.sorted_cursors();
        Some((min_c.index.0, max_c.index.0))
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
    let _ = run_ui(&ctx, esc_input, |ui| {
        app.update_ui(ui);
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
    let _ = run_ui(&ctx, f3_input.clone(), |ui| {
        app.update_ui(ui);
    });
    assert_eq!(get_sel(&ctx), Some((11, 16)), "Second F3 should select second 'alpha'");

    // Press F3 again -> third occurrence at char index 23..28
    let _ = run_ui(&ctx, f3_input.clone(), |ui| {
        app.update_ui(ui);
    });
    assert_eq!(get_sel(&ctx), Some((23, 28)), "Third F3 should select third 'alpha'");

    // Press F3 again -> wraps around to first occurrence at 0..5
    let _ = run_ui(&ctx, f3_input, |ui| {
        app.update_ui(ui);
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
    assert!(!app.doc.can_save());

    // 2. Open file_a -> clean, Save button disabled
    app.open_file(&ctx, file_a.clone());
    assert_eq!(app.doc.text, "fn a() {}\n");
    assert!(!app.doc.can_save());
    assert!(!app.doc.is_file_dirty(&file_a));

    // Focus editor in frame 1
    let _ = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
        ui.ctx().memory_mut(|m| m.request_focus(egui::Id::unique("code_editor")));
    });

    // 3. Edit file_a in frame 2
    let mut input = egui::RawInput::default();
    input.events.push(egui::Event::Text("// edited\n".to_string()));
    let _ = run_ui(&ctx, input, |ui| {
        app.update_ui(ui);
    });

    assert_eq!(app.doc.text, "// edited\nfn a() {}\n");
    assert!(app.doc.can_save(), "Save should be enabled after editing");
    assert!(app.doc.is_file_dirty(&file_a), "file_a should be marked dirty");
    assert!(!app.doc.is_file_dirty(&file_b), "file_b should still be clean");

    // Verify FileNode exists for file_a
    let root = app.explorer.tree_root.as_ref().unwrap();
    let node_a = root.children.iter().find(|n| n.path == file_a).unwrap();
    assert_eq!(node_a.name, "a.rs");

    // 4. Switch to file_b without saving file_a
    app.open_file(&ctx, file_b.clone());
    assert_eq!(app.doc.text, "fn b() {}\n");
    assert!(!app.doc.can_save(), "Save should be disabled on unmodified file_b");
    assert!(
        app.doc.is_file_dirty(&file_a),
        "file_a should remain dirty in state after switching to file_b"
    );
    assert!(!app.doc.is_file_dirty(&file_b));

    // 5. Switch back to file_a -> unsaved content is restored and Save is enabled
    app.open_file(&ctx, file_a.clone());
    assert_eq!(app.doc.text, "// edited\nfn a() {}\n");
    assert!(app.doc.can_save(), "Save should be enabled when returning to unsaved file_a");
    assert!(app.doc.is_file_dirty(&file_a));

    // 6. Save file_a -> written to disk, dirty cleared, Save disabled
    app.doc.save_current_file();
    assert!(!app.doc.can_save(), "Save should be disabled immediately after saving");
    assert!(!app.doc.is_file_dirty(&file_a), "file_a should no longer be dirty after saving");
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
    assert_eq!(Path::new(&app.doc.path), new_file.as_path());
    assert_eq!(app.explorer.selected_node, Some((new_file.clone(), false)));

    // 3. Edit the open file (so it has unsaved state) and press F2 to rename it
    app.doc.text = "fn main() {}".into();
    app.doc.is_current_dirty = true;

    let mut f2_input = egui::RawInput::default();
    f2_input.events.push(egui::Event::Key {
        key: egui::Key::F2,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = run_ui(&ctx, f2_input, |ui| {
        app.update_ui(ui);
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
    let _ = run_ui(&ctx, enter_input, |ui| {
        app.update_ui(ui);
    });

    let renamed_file = nested_dir.join("lib.rs");
    assert!(!new_file.exists());
    assert!(renamed_file.is_file());
    assert_eq!(Path::new(&app.doc.path), renamed_file.as_path());
    assert_eq!(app.doc.file_name, "lib.rs");
    assert!(app.doc.is_file_dirty(&renamed_file));

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
        Path::new(&app.doc.path),
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
        app.doc.path.as_os_str().is_empty(),
        "Active editor path should be cleared when its folder is deleted"
    );
    assert!(!app.doc.can_save());

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
        egui::Id::unique("code_editor").with(0usize).value().into();

    // Frame 1: Editor tab (initial render)
    let out1 = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    let u1 = out1.platform_output.accesskit_update.unwrap();
    assert!(
        u1.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Initial editor render should include active TextRun row node"
    );

    // Frame 2: Consecutive idle editor frame -> TextRun should NOT be rebuilt
    let out2 = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    let u2 = out2.platform_output.accesskit_update.unwrap();
    assert!(
        !u2.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Consecutive idle editor frame should not rebuild AccessKit TextRun node"
    );

    // Frame 3: Switch to Terminal tab -> Editor removed, Terminal shown
    app.tab = rusted::app::Tab::Terminal;
    let out3 = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    let u3 = out3.platform_output.accesskit_update.unwrap();

    // Frame 4: Switch back to Editor tab -> Editor re-shown after being hidden
    app.tab = rusted::app::Tab::Editor;
    let out4 = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    let u4 = out4.platform_output.accesskit_update.unwrap();
    assert!(
        u4.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Switching back to editor must rebuild TextRun child so AccessKit tree stays consistent"
    );

    // Frame 5: Switch to Terminal tab again
    app.tab = rusted::app::Tab::Terminal;
    let out5 = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    let u5 = out5.platform_output.accesskit_update.unwrap();

    // Frame 6: Switch back to Editor tab again
    app.tab = rusted::app::Tab::Editor;
    let out6 = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });
    let u6 = out6.platform_output.accesskit_update.unwrap();
    assert!(
        u6.nodes.iter().any(|(id, _)| *id == editor_row_id),
        "Switching back to editor again must rebuild TextRun child"
    );

    // Feed all updates into accesskit consumer Tree to ensure no panics occur
    struct NoOpHandler;
    impl accesskit_consumer::TreeChangeHandler for NoOpHandler {
        fn node_added(&mut self, _node: &accesskit_consumer::NodeRef) {}
        fn node_updated(
            &mut self,
            _old_node: &accesskit_consumer::NodeRef,
            _new_node: &accesskit_consumer::NodeRef,
        ) {
        }
        fn focus_moved(
            &mut self,
            _old_node: Option<&accesskit_consumer::NodeRef>,
            _new_node: Option<&accesskit_consumer::NodeRef>,
        ) {
        }
        fn node_removed(&mut self, _node: &accesskit_consumer::NodeRef) {}
    }
    let mut tree = accesskit_consumer::Tree::new(u1, false);
    tree.update_and_process_changes(u2, &mut NoOpHandler);
    tree.update_and_process_changes(u3, &mut NoOpHandler);
    tree.update_and_process_changes(u4, &mut NoOpHandler);
    tree.update_and_process_changes(u5, &mut NoOpHandler);
    tree.update_and_process_changes(u6, &mut NoOpHandler);

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
        let _ = run_ui(&ctx, egui::RawInput::default(), |ui| app.update_ui(ui));
    }
    assert!(!app.focus_editor_pending, "request is consumed once");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::unique("code_editor"))),
        "editor should hold keyboard focus (active caret) after the first frames"
    );

    // Focus is not forcibly re-grabbed after the user moves it elsewhere.
    ctx.memory_mut(|m| m.surrender_focus(egui::Id::unique("code_editor")));
    for _ in 0..2 {
        let _ = run_ui(&ctx, egui::RawInput::default(), |ui| app.update_ui(ui));
    }
    assert!(!ctx.memory(|m| m.has_focus(egui::Id::unique("code_editor"))));

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
    assert_eq!(app.explorer.folder_path, dir);
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
        let out = run_ui(&ctx, egui::RawInput::default(), |ui| app.update_ui(ui));
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
    let _ = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
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
    let _ = run_ui(&ctx, ctrl_tab_input, |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.tab, rusted::app::Tab::Terminal, "Ctrl+Tab should switch from Editor to Terminal");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::unique("conpty_terminal"))),
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
    let _ = run_ui(&ctx, ctrl_tab_input2, |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.tab, rusted::app::Tab::Editor, "Ctrl+Tab should switch from Terminal to Editor");
    assert!(
        ctx.memory(|m| m.has_focus(egui::Id::unique("code_editor"))),
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
    let _ = run_ui(&ctx, ctrl_shift_tab_input, |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.tab, rusted::app::Tab::Terminal, "Ctrl+Shift+Tab should switch between tabs");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_app_icon_assets_valid() {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png"))
        .expect("assets/icon.png must decode cleanly");
    assert_eq!((icon.width, icon.height), (256, 256));
    assert_eq!(icon.rgba.len(), 256 * 256 * 4);

    let ico = include_bytes!("../../assets/rusted.ico");
    assert!(ico.len() > 22);
    assert_eq!(&ico[0..6], &[0, 0, 1, 0, 4, 0]);
}

#[test]
fn test_ctrl_s_saves_dirty_file_in_editor() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_ctrl_s_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("notes.txt");
    fs::write(&file_path, "initial").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.doc.open_file(&ctx, file_path.clone());

    // Edit the file so it becomes dirty
    app.doc.text = "updated content".into();
    app.doc.text_revision = app.doc.text_revision.wrapping_add(1);
    app.doc.is_current_dirty = true;
    assert!(app.doc.can_save());

    // Render one frame, then press Ctrl+S
    let _ = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });

    let mut ctrl_s_input = egui::RawInput::default();
    ctrl_s_input.events.push(egui::Event::Key {
        key: egui::Key::S,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::CTRL,
    });
    let _ = run_ui(&ctx, ctrl_s_input, |ui| {
        app.update_ui(ui);
    });

    assert!(!app.doc.is_current_dirty, "Ctrl+S should clear dirty state");
    assert!(!app.doc.can_save(), "Ctrl+S should disable Save after saving");
    assert_eq!(
        fs::read_to_string(&file_path).unwrap(),
        "updated content",
        "Ctrl+S should write updated buffer to disk"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_undo_back_to_saved_state_clears_dirty() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_undo_dirty_test_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("notes.txt");
    fs::write(&file_path, "initial").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.doc.open_file(&ctx, file_path.clone());
    assert!(!app.doc.is_current_dirty);
    assert!(!app.doc.can_save());

    // Render initial frame and focus editor
    rusted::editor::request_editor_focus(&ctx, rusted::editor::code_editor_id());
    let _ = run_ui(&ctx, egui::RawInput::default(), |ui| {
        app.update_ui(ui);
    });

    // Type one character at end of buffer -> becomes dirty
    let edit_input = egui::RawInput {
        time: Some(1.0),
        events: vec![
            egui::Event::Key {
                key: egui::Key::End,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Text("!".to_string()),
        ],
        ..Default::default()
    };
    let _ = run_ui(&ctx, edit_input, |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.doc.text, "initial!");
    assert!(app.doc.is_current_dirty);
    assert!(app.doc.can_save());

    // Undo back to saved content -> dirty flag and can_save() should clear
    let undo_input = egui::RawInput {
        time: Some(2.5),
        events: vec![egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }],
        ..Default::default()
    };
    let _ = run_ui(&ctx, undo_input, |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.doc.text, "initial");
    assert!(
        !app.doc.is_current_dirty,
        "Undoing back to saved state should clear is_current_dirty"
    );
    assert!(
        !app.doc.can_save(),
        "Undoing back to saved state should disable can_save()"
    );

    // Redo makes it dirty again
    let redo_input = egui::RawInput {
        time: Some(3.0),
        events: vec![egui::Event::Key {
            key: egui::Key::Y,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }],
        ..Default::default()
    };
    let _ = run_ui(&ctx, redo_input, |ui| {
        app.update_ui(ui);
    });
    assert_eq!(app.doc.text, "initial!");
    assert!(app.doc.is_current_dirty);
    assert!(app.doc.can_save());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_close_requested_with_unsaved_changes_opens_confirmation_and_cancels_close() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_close_guard_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("dirty.txt");
    fs::write(&file_path, "orig").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.open_file(&ctx, file_path.clone());
    app.doc.set_text("modified");
    app.doc.is_current_dirty = true;
    assert!(app.doc.has_unsaved_changes());

    let mut raw = egui::RawInput::default();
    raw.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
    let out = run_ui(&ctx, raw, |ui| app.update_ui(ui));

    assert!(
        app.confirm_close_open,
        "Close request with unsaved edits must open confirmation dialog"
    );
    let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
    assert!(
        cmds.contains(&egui::ViewportCommand::CancelClose),
        "Close request with unsaved edits must send CancelClose"
    );

    assert!(app.doc.save_all());
    assert!(!app.doc.has_unsaved_changes());
    assert_eq!(fs::read_to_string(&file_path).unwrap(), "modified");

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_switching_back_to_stashed_dirty_file_detects_external_modification() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_stash_conflict_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let file_a = temp_dir.join("a.txt");
    let file_b = temp_dir.join("b.txt");
    fs::write(&file_a, "v1").unwrap();
    fs::write(&file_b, "b").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    app.open_file(&ctx, file_a.clone());
    app.doc.set_text("unsaved edit");
    app.doc.is_current_dirty = true;

    // Switch to file_b, stashing dirty file_a
    app.open_file(&ctx, file_b);
    assert!(app.doc.unsaved_files.contains_key(&file_a));

    // Modify file_a externally on disk (different length guarantees different DiskStamp)
    fs::write(&file_a, "external disk change!").unwrap();

    // Switch back to file_a -> must flag ModifiedOnDisk conflict
    app.open_file(&ctx, file_a);
    assert_eq!(
        app.doc.external_conflict,
        Some(rusted::document::ExternalConflict::ModifiedOnDisk)
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_terminal_read_only_prevents_direct_text_mutation_on_focus_click() {
    let ctx = egui::Context::default();
    let mut pane = rusted::terminal::TerminalPane::new_headless();
    {
        let mut lock = pane.term.lock().unwrap();
        lock.parser.process(b"C:\\>");
        lock.pty_updated = true;
    }

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(600.0, 400.0));
    // Run two warm-up frames so the initial resize settles to the viewport grid size.
    for _ in 0..2 {
        let _ = run_ui(
            &ctx,
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| pane.show(ui, egui::Frame::default()),
        );
    }
    let before = pane.term_screen_text.clone();

    // Click to focus AND send a Text event on the very same frame
    let click_pos = egui::Pos2::new(20.0, 20.0);
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::Text("INJECTED".to_string()),
            ],
            ..Default::default()
        },
        |ui| pane.show(ui, egui::Frame::default()),
    );
    assert_eq!(
        pane.term_screen_text, before,
        "Terminal screen text must remain read-only even on the click-to-focus frame"
    );
}

#[test]
fn test_ai_explain_shortcut_alt_e_and_button() {
    use std::sync::mpsc;

    let temp_dir =
        std::env::temp_dir().join(format!("rusted_ai_explain_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    let src_dir = temp_dir.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let file_path = src_dir.join("main.rs");
    fs::write(&file_path, "fn main() {\n    let x = 42;\n    println!(\"{x}\");\n}\n").unwrap();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = TextEditor::new_headless(&temp_dir);
    let (tx, rx) = mpsc::channel();
    app.terminal.pty_writer = Some(tx);
    app.open_file(&ctx, file_path.clone());

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(1000.0, 600.0));
    let take_sent = || {
        let mut out = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        String::from_utf8(out).unwrap()
    };

    // Frame 1: no selection -> AI Explain button is disabled and Alt+E does nothing
    let out1 = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    let ak1 = out1.platform_output.accesskit_update.unwrap();
    let explain_node_disabled = ak1
        .nodes
        .iter()
        .find(|(_, n)| n.label() == Some("AI Explain"))
        .map(|(_, n)| n)
        .expect("Top bar should render an 'AI Explain' button");
    assert!(
        explain_node_disabled.is_disabled(),
        "AI Explain button should be disabled when no text is selected"
    );

    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::Key {
                key: egui::Key::E,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::ALT,
            }],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    assert_eq!(app.tab, rusted::app::Tab::Editor);
    assert!(take_sent().is_empty());

    // Select lines 2-3 ("    let x = 42;\n    println!(\"{x}\");")
    let editor_id = rusted::editor::code_editor_id();
    let set_selection = |ctx: &egui::Context, start: usize, end: usize| {
        ctx.data_mut(|d| {
            let mut st = d.get_temp::<FastEditorState>(editor_id).unwrap_or_default();
            st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(start),
                egui::text::CCursor::new(end),
            )));
            d.insert_temp(editor_id, st);
        });
    };
    let start_idx = "fn main() {\n".chars().count();
    let selected_slice = "    let x = 42;\n    println!(\"{x}\");";
    let end_idx = start_idx + selected_slice.chars().count();
    set_selection(&ctx, start_idx, end_idx);

    // Press Alt+E -> switches to Terminal tab, focuses terminal, sends formatted prompt + Enter
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::Key {
                key: egui::Key::E,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::ALT,
            }],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    assert_eq!(app.tab, rusted::app::Tab::Terminal);
    assert!(
        ctx.memory(|m| m.has_focus(rusted::editor::terminal_id())),
        "AI Explain should focus the terminal"
    );
    assert_eq!(
        take_sent(),
        format!("Explain src/main.rs:2-3: {selected_slice}\r")
    );

    // Switch back to Editor, enable bracketed paste on terminal (like agy), and click the AI Explain button
    app.tab = rusted::app::Tab::Editor;
    {
        let mut lock = app.terminal.term.lock().unwrap();
        lock.parser.process(b"\x1b[?2004h");
    }
    set_selection(&ctx, 0, "fn main()".chars().count());

    let out_enabled = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    let ak_enabled = out_enabled.platform_output.accesskit_update.unwrap();
    let explain_node = ak_enabled
        .nodes
        .iter()
        .find(|(_, n)| n.label() == Some("AI Explain"))
        .map(|(_, n)| n)
        .expect("AI Explain button should be present");
    assert!(
        !explain_node.is_disabled(),
        "AI Explain button should be enabled when text is selected"
    );
    let bounds = explain_node.bounds().expect("AI Explain button should have bounds");
    let click_pos = egui::pos2(
        ((bounds.x0 + bounds.x1) * 0.5) as f32,
        ((bounds.y0 + bounds.y1) * 0.5) as f32,
    );

    // Press and release pointer on the AI Explain button
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::PointerButton {
                pos: click_pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );

    assert_eq!(app.tab, rusted::app::Tab::Terminal);
    assert!(
        ctx.memory(|m| m.has_focus(rusted::editor::terminal_id())),
        "Clicking AI Explain should focus the terminal"
    );
    assert_eq!(
        take_sent(),
        "\x1b[200~Explain src/main.rs:1-1: fn main()\x1b[201~\r"
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_ai_explain_from_scrolled_long_file_does_not_panic() {
    use std::fmt::Write;
    use std::sync::mpsc;

    let temp_dir =
        std::env::temp_dir().join(format!("rusted_ai_explain_scroll_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();
    let file_path = temp_dir.join("long.rs");
    let mut content = String::new();
    for i in 1..=250 {
        let _ = writeln!(content, "let line_{i} = {i};");
    }
    fs::write(&file_path, &content).unwrap();

    let ctx = egui::Context::default();
    ctx.enable_accesskit();
    let mut app = TextEditor::new_headless(&temp_dir);
    let (tx, rx) = mpsc::channel();
    app.terminal.pty_writer = Some(tx);
    app.open_file(&ctx, file_path.clone());

    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(1000.0, 500.0));
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );

    // Select line 200 and scroll the editor far down (past the terminal's total height)
    let target_line = "let line_200 = 200;";
    let start_char = content.find(target_line).unwrap();
    let end_char = start_char + target_line.len();
    let editor_id = rusted::editor::code_editor_id();
    ctx.data_mut(|d| {
        let mut st = d.get_temp::<FastEditorState>(editor_id).unwrap_or_default();
        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(start_char),
            egui::text::CCursor::new(end_char),
        )));
        st.scroll_to_cursor = true;
        d.insert_temp(editor_id, st);
    });

    // Run a couple frames so the editor ScrollArea actually scrolls down to line 200
    for _ in 0..3 {
        let _ = run_ui(
            &ctx,
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| app.update_ui(ui),
        );
    }

    // Trigger AI Explain while scrolled far down -> must not panic on inverted clip rect
    let out = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::Key {
                key: egui::Key::E,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::ALT,
            }],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    assert!(out.platform_output.accesskit_update.is_some());
    assert_eq!(app.tab, rusted::app::Tab::Terminal);

    let mut sent = Vec::new();
    while let Ok(chunk) = rx.try_recv() {
        sent.extend_from_slice(&chunk);
    }
    assert_eq!(
        String::from_utf8(sent).unwrap(),
        format!("Explain long.rs:200-200: {target_line}\r")
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_untitled_dirty_tracking_save_as_and_undo_across_disk_reload() {
    let temp_dir =
        std::env::temp_dir().join(format!("rusted_untitled_reload_{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir).unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&temp_dir);
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(900.0, 600.0));

    // Focus Untitled editor and type text -> becomes dirty & saveable
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        },
        |ui| {
            app.update_ui(ui);
            ui.ctx()
                .memory_mut(|m| m.request_focus(rusted::editor::code_editor_id()));
        },
    );
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::Text("draft note".to_string())],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    assert!(app.doc.is_untitled_dirty());
    assert!(app.doc.can_save());
    assert!(app.doc.has_unsaved_changes());

    // Save As writes to disk and clears dirty state
    let dest = temp_dir.join("saved_draft.txt");
    assert!(app.doc.save_current_file_as(dest.clone()));
    assert!(!app.doc.is_current_dirty);
    assert_eq!(app.doc.file_name, "saved_draft.txt");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "draft note");

    // External disk modification triggers reload; Ctrl+Z cleanly restores pre-reload text
    fs::write(&dest, "external overwrite from disk").unwrap();
    assert_eq!(
        app.doc.sync_with_disk(&ctx, false),
        rusted::document::DiskSyncResult::Reloaded
    );
    assert_eq!(app.doc.text, "external overwrite from disk");

    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    assert_eq!(
        app.doc.text, "draft note",
        "Ctrl+Z after disk reload should restore pre-reload buffer cleanly"
    );

    // Ctrl+Y redoes back to the reloaded disk content
    let _ = run_ui(
        &ctx,
        egui::RawInput {
            screen_rect: Some(screen),
            events: vec![egui::Event::Key {
                key: egui::Key::Y,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            ..Default::default()
        },
        |ui| app.update_ui(ui),
    );
    assert_eq!(app.doc.text, "external overwrite from disk");

    let _ = fs::remove_dir_all(&temp_dir);
}
