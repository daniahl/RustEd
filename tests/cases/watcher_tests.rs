//! Tests for filesystem watching, incremental tree refreshes, and document disk sync.

use eframe::egui;
use rusted::{
    app::TextEditor,
    document::{DiskSyncResult, ExternalConflict},
    editor::FastEditorState,
    piece_table::PieceTable,
    watcher::{path_starts_with, path_strip_prefix, paths_equal, FileWatcher},
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn make_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rusted_watch_{}_{}_{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_frame(ctx: &egui::Context, app: &mut TextEditor) {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(900.0, 600.0),
        )),
        ..Default::default()
    };
    ctx.run_ui(input, |ui| app.update_ui(ui)).textures_delta.clear();
}

fn pump_until<F>(ctx: &egui::Context, app: &mut TextEditor, timeout: Duration, mut pred: F) -> bool
where
    F: FnMut(&TextEditor) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        run_frame(ctx, app);
        if pred(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn test_path_matching_helpers() {
    assert!(path_starts_with(
        Path::new("project/src/main.rs"),
        Path::new("project/src")
    ));
    assert!(!path_starts_with(
        Path::new("project/src2/main.rs"),
        Path::new("project/src")
    ));

    #[cfg(target_os = "windows")]
    {
        assert!(paths_equal(
            Path::new(r"G:\RustEd\src\Main.rs"),
            Path::new(r"\\?\g:\rusted\src\main.rs")
        ));
        assert!(path_starts_with(
            Path::new(r"g:\rusted\src\main.rs"),
            Path::new(r"G:\RustEd")
        ));
    }
}

#[test]
fn test_piece_table_eq_str_multi_piece() {
    let mut pt = PieceTable::from_str("hello world");
    pt.insert(5, ",");
    pt.delete(7..12);
    pt.insert(7, "rust");
    assert_eq!(pt.build_string(), "hello, rust");
    assert!(pt.eq_str("hello, rust"));
    assert!(!pt.eq_str("hello, rusty"));
    assert!(!pt.eq_str("hello, rus"));
    assert!(!pt.eq_str("hello! rust"));
}

#[test]
fn test_refresh_watched_paths_keeps_collapsed_dirs_lazy_and_adds_new_items() {
    let root_dir = make_temp_dir("lazy");
    let lazy_sub = root_dir.join("target");
    fs::create_dir_all(lazy_sub.join("debug")).unwrap();
    fs::write(root_dir.join("initial.rs"), "fn main() {}\n").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&root_dir);

    // Simulate compiler writing deep inside collapsed `target/debug`
    let build_artifact = lazy_sub.join("debug").join("app.exe");
    fs::write(&build_artifact, "bin").unwrap();
    app.process_fs_events(&ctx, &[build_artifact], false);
    let target_node = app
        .explorer
        .tree_root
        .as_mut()
        .unwrap()
        .find_mut(&lazy_sub)
        .unwrap();
    assert!(
        !target_node.children_loaded,
        "collapsed subdirectory must stay lazy on external build churn"
    );

    // Simulate creating a new file in root
    let added = root_dir.join("added.rs");
    fs::write(&added, "// new").unwrap();
    app.process_fs_events(&ctx, std::slice::from_ref(&added), false);
    assert!(
        app.explorer
            .tree_root
            .as_ref()
            .unwrap()
            .children
            .iter()
            .any(|c| paths_equal(&c.path, &added)),
        "new root file should appear in explorer"
    );

    let _ = fs::remove_dir_all(&root_dir);
}

#[test]
fn test_clean_reload_preserves_clamped_caret_and_dirty_conflict_protects_edits() {
    let root_dir = make_temp_dir("doc_sync");
    let file_path = root_dir.join("notes.txt");
    fs::write(&file_path, "first line\nsecond line").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&root_dir);
    app.open_file(&ctx, file_path.clone());

    // Put caret at char index 8 inside clean buffer
    let editor_id = egui::Id::unique("code_editor");
    ctx.data_mut(|d| {
        let mut st = d.get_temp::<FastEditorState>(editor_id).unwrap_or_default();
        st.cursor
            .set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(8))));
        d.insert_temp(editor_id, st);
    });

    // 1. External edit to clean file -> auto-reloads and clamps caret cleanly
    fs::write(&file_path, "short").unwrap();
    app.process_fs_events(&ctx, std::slice::from_ref(&file_path), false);
    assert_eq!(app.doc.text.build_string(), "short");
    assert!(!app.doc.is_current_dirty);
    assert_eq!(app.doc.external_conflict, None);
    let caret_after = ctx.data_mut(|d| {
        d.get_temp::<FastEditorState>(editor_id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| r.primary.index.0)
    });
    assert_eq!(caret_after, Some(5));

    // 2. Dirty edits must NOT be overwritten when file changes on disk
    app.doc.set_text("unsaved local edits");
    app.doc.is_current_dirty = true;
    fs::write(&file_path, "external replacement").unwrap();
    app.process_fs_events(&ctx, std::slice::from_ref(&file_path), false);
    assert_eq!(app.doc.text.build_string(), "unsaved local edits");
    assert_eq!(
        app.doc.external_conflict,
        Some(ExternalConflict::ModifiedOnDisk)
    );

    // Forced reload applies disk content and clears conflict
    assert_eq!(app.doc.sync_with_disk(&ctx, true), DiskSyncResult::Reloaded);
    assert_eq!(app.doc.text.build_string(), "external replacement");
    assert!(!app.doc.is_current_dirty);
    assert_eq!(app.doc.external_conflict, None);

    // 3. Deleting a dirty file sets DeletedOnDisk without discarding edits; deleting a clean file closes it
    app.doc.set_text("unsaved before delete");
    app.doc.is_current_dirty = true;
    fs::remove_file(&file_path).unwrap();
    app.process_fs_events(&ctx, std::slice::from_ref(&file_path), false);
    assert_eq!(app.doc.text.build_string(), "unsaved before delete");
    assert_eq!(
        app.doc.external_conflict,
        Some(ExternalConflict::DeletedOnDisk)
    );

    fs::write(&file_path, "recreated").unwrap();
    assert_eq!(app.doc.sync_with_disk(&ctx, true), DiskSyncResult::Reloaded);
    assert!(!app.doc.is_current_dirty);
    fs::remove_file(&file_path).unwrap();
    app.process_fs_events(&ctx, &[file_path], false);
    assert!(app.doc.path.is_empty());

    let _ = fs::remove_dir_all(&root_dir);
}

#[cfg(target_os = "windows")]
#[test]
fn test_live_file_watcher_syncs_tree_editor_retarget_and_extra_file() {
    let root_a = make_temp_dir("live_a");
    let root_b = make_temp_dir("live_b");
    let outside_dir = make_temp_dir("live_outside");

    let file_a = root_a.join("alpha.txt");
    let outside_file = outside_dir.join("external.txt");
    fs::write(&file_a, "initial alpha").unwrap();
    fs::write(&outside_file, "outside v1").unwrap();

    let ctx = egui::Context::default();
    let mut app = TextEditor::new_headless(&root_a);
    app.watcher = Some(FileWatcher::new(ctx.clone(), &root_a));
    app.open_file(&ctx, file_a.clone());
    run_frame(&ctx, &mut app);

    // 1. External file creation appears in explorer tree
    let created = root_a.join("from_terminal.rs");
    fs::write(&created, "pub fn hi() {}\n").unwrap();
    assert!(
        pump_until(&ctx, &mut app, Duration::from_secs(3), |app| {
            app.explorer
                .tree_root
                .as_ref()
                .map(|r| r.children.iter().any(|c| paths_equal(&c.path, &created)))
                .unwrap_or(false)
        }),
        "live watcher should pick up externally created file"
    );

    // 2. External modification to open clean file updates editor buffer
    fs::write(&file_a, "updated from terminal").unwrap();
    assert!(
        pump_until(&ctx, &mut app, Duration::from_secs(3), |app| {
            app.doc.text.eq_str("updated from terminal")
        }),
        "live watcher should reload externally edited open file"
    );
    assert!(!app.doc.is_current_dirty);

    // 3. Switching folders retargets the watcher to root_b
    app.explorer.open_folder(root_b.clone());
    run_frame(&ctx, &mut app);
    let created_b = root_b.join("inside_b.txt");
    fs::write(&created_b, "hello b").unwrap();
    assert!(
        pump_until(&ctx, &mut app, Duration::from_secs(3), |app| {
            app.explorer
                .tree_root
                .as_ref()
                .map(|r| r.children.iter().any(|c| paths_equal(&c.path, &created_b)))
                .unwrap_or(false)
        }),
        "watcher should track new folder after open_folder"
    );

    // 4. Open file outside root is also watched and synced on external edit
    app.open_file(&ctx, outside_file.clone());
    run_frame(&ctx, &mut app);
    fs::write(&outside_file, "outside v2").unwrap();
    assert!(
        pump_until(&ctx, &mut app, Duration::from_secs(3), |app| {
            app.doc.text.eq_str("outside v2")
        }),
        "watcher should sync open file located outside project root"
    );

    drop(app);
    let _ = fs::remove_dir_all(&root_a);
    let _ = fs::remove_dir_all(&root_b);
    let _ = fs::remove_dir_all(&outside_dir);
}

#[test]
fn test_path_strip_prefix_windows_aware() {
    assert_eq!(
        path_strip_prefix(Path::new(r"C:\Proj\src\main.rs"), Path::new(r"C:\Proj")),
        Some(PathBuf::from("src").join("main.rs"))
    );
    assert_eq!(
        path_strip_prefix(Path::new(r"C:\Proj\src"), Path::new(r"C:\Proj\src")),
        Some(PathBuf::new())
    );
    assert_eq!(
        path_strip_prefix(Path::new(r"C:\ProjTwo\src"), Path::new(r"C:\Proj")),
        None
    );

    #[cfg(target_os = "windows")]
    {
        assert_eq!(
            path_strip_prefix(Path::new(r"C:\Proj\SRC\main.rs"), Path::new(r"c:\proj\")),
            Some(PathBuf::from("SRC").join("main.rs"))
        );
        assert_eq!(
            path_strip_prefix(Path::new(r"\\?\C:\Proj\src\lib.rs"), Path::new(r"C:\Proj")),
            Some(PathBuf::from("src").join("lib.rs"))
        );
        assert!(paths_equal(Path::new(r"c:/proj/src"), Path::new(r"C:\Proj\SRC")));
        assert!(path_starts_with(
            Path::new(r"c:/proj/src/lib.rs"),
            Path::new(r"C:\Proj")
        ));
    }
}
