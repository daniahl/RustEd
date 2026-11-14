//! Filesystem watcher for automatic file-tree and editor synchronization.
//!
//! Uses overlapped Win32 `ReadDirectoryChangesW` on Windows with short burst debouncing so
//! external modifications (e.g. from the integrated terminal or other tools) wake the UI via
//! `egui::Context::request_repaint()`. Provides a no-op implementation on non-Windows targets.

use eframe::egui;
use std::{
    borrow::Cow,
    collections::{HashSet, VecDeque},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const MAX_QUEUED_PATHS: usize = 512;
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(120);
const MAX_COALESCE_WINDOW: Duration = Duration::from_millis(300);

/// Coalesced filesystem change batch emitted by [`FileWatcher`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchBatch {
    pub paths: Vec<PathBuf>,
    pub rescan_all: bool,
}

impl WatchBatch {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.rescan_all
    }
}

/// Strips a Windows `\\?\` verbatim drive prefix so canonical and regular paths compare cleanly.
fn normalize_verbatim(path: &Path) -> Cow<'_, Path> {
    #[cfg(target_os = "windows")]
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") {
                return Cow::Owned(PathBuf::from(rest));
            }
        }
    }
    Cow::Borrowed(path)
}

#[inline]
fn component_eq(a: Component<'_>, b: Component<'_>) -> bool {
    #[cfg(target_os = "windows")]
    {
        let sa = a.as_os_str().to_string_lossy();
        let sb = b.as_os_str().to_string_lossy();
        if sa.is_ascii() && sb.is_ascii() {
            sa.eq_ignore_ascii_case(&sb)
        } else {
            sa.to_lowercase() == sb.to_lowercase()
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        a == b
    }
}

/// Produces a normalized key for $O(1)$ path deduplication in sets.
fn path_dedup_key(path: &Path) -> PathBuf {
    let norm = normalize_verbatim(path);
    #[cfg(target_os = "windows")]
    {
        let mut out = PathBuf::new();
        for comp in norm.components() {
            let s = comp.as_os_str().to_string_lossy();
            if s.is_ascii() {
                out.push(s.to_ascii_lowercase());
            } else {
                out.push(s.to_lowercase());
            }
        }
        out
    }
    #[cfg(not(target_os = "windows"))]
    {
        norm.into_owned()
    }
}

/// Compares two paths component-wise, ignoring Windows case and separator differences.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    let na = normalize_verbatim(a);
    let nb = normalize_verbatim(b);
    let mut ca = na.components();
    let mut cb = nb.components();
    loop {
        match (ca.next(), cb.next()) {
            (Some(x), Some(y)) => {
                if !component_eq(x, y) {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Returns true if `path` equals `base` or is a descendant of `base`, using Windows-aware matching.
pub fn path_starts_with(path: &Path, base: &Path) -> bool {
    path_strip_prefix(path, base).is_some()
}

/// If `path` equals `base` or is a descendant of `base` (using Windows-aware matching),
/// returns the relative path suffix under `base`.
pub fn path_strip_prefix(path: &Path, base: &Path) -> Option<PathBuf> {
    let np = normalize_verbatim(path);
    let nb = normalize_verbatim(base);
    let mut cp = np.components();
    for cb in nb.components() {
        match cp.next() {
            Some(c) if component_eq(c, cb) => {}
            _ => return None,
        }
    }
    Some(cp.as_path().to_path_buf())
}

#[derive(Default)]
struct SharedQueue {
    batch: WatchBatch,
    seen: HashSet<PathBuf>,
}

impl SharedQueue {
    fn push_paths(&mut self, paths: Vec<PathBuf>, rescan_all: bool, ctx: &egui::Context) {
        if paths.is_empty() && !rescan_all {
            return;
        }
        let was_empty = self.batch.is_empty();
        if rescan_all {
            self.batch.rescan_all = true;
            self.batch.paths.clear();
            self.seen.clear();
        } else if !self.batch.rescan_all {
            for p in paths {
                let key = path_dedup_key(&p);
                if self.seen.insert(key) {
                    if self.batch.paths.len() >= MAX_QUEUED_PATHS {
                        self.batch.rescan_all = true;
                        self.batch.paths.clear();
                        self.seen.clear();
                        break;
                    }
                    self.batch.paths.push(p);
                }
            }
        }
        if was_empty && !self.batch.is_empty() {
            ctx.request_repaint();
        }
    }

    fn take(&mut self) -> Option<WatchBatch> {
        if self.batch.is_empty() {
            None
        } else {
            self.seen.clear();
            Some(std::mem::take(&mut self.batch))
        }
    }
}

/// Watches the project root (recursively) and, when needed, the parent directory of an open file
/// located outside the project root.
pub struct FileWatcher {
    ctx: egui::Context,
    shared: Arc<Mutex<SharedQueue>>,
    root_dir: Option<PathBuf>,
    root_watch: Option<DirWatchGuard>,
    extra_dir: Option<PathBuf>,
    extra_watch: Option<DirWatchGuard>,
    last_extra_file: Option<PathBuf>,
    pending_retries: VecDeque<(PathBuf, std::time::Instant, u8)>,
}

impl FileWatcher {
    pub fn new(ctx: egui::Context, root: &Path) -> Self {
        let mut watcher = Self {
            ctx,
            shared: Arc::new(Mutex::new(SharedQueue::default())),
            root_dir: None,
            root_watch: None,
            extra_dir: None,
            extra_watch: None,
            last_extra_file: None,
            pending_retries: VecDeque::new(),
        };
        watcher.watch_root(root);
        watcher
    }

    /// Retargets the recursive root watch if `root` differs from the currently watched folder.
    pub fn watch_root(&mut self, root: &Path) {
        if let Some(ref cur) = self.root_dir {
            if paths_equal(cur, root) {
                return;
            }
        }
        self.root_watch = None;
        self.root_dir = Some(root.to_path_buf());
        self.last_extra_file = None;
        if root.is_dir() {
            self.root_watch = DirWatchGuard::spawn(
                self.ctx.clone(),
                root.to_path_buf(),
                true,
                Arc::clone(&self.shared),
            );
        }
    }

    /// Ensures the active file's parent directory is watched non-recursively when it lies outside
    /// the project root.
    pub fn watch_extra_file(&mut self, active_file: Option<&Path>) {
        match (&self.last_extra_file, active_file) {
            (Some(prev), Some(next)) if paths_equal(prev, next) => return,
            (None, None) if self.extra_dir.is_none() => return,
            _ => {}
        }
        self.last_extra_file = active_file.map(Path::to_path_buf);

        let needed_dir = active_file.and_then(|file| {
            let parent = file.parent()?;
            if parent.as_os_str().is_empty() {
                return None;
            }
            if let Some(ref root) = self.root_dir {
                if path_starts_with(parent, root) {
                    return None;
                }
            }
            if !parent.is_dir() {
                return None;
            }
            Some(parent.to_path_buf())
        });

        match (&self.extra_dir, &needed_dir) {
            (Some(cur), Some(next)) if paths_equal(cur, next) => return,
            (None, None) => return,
            _ => {}
        }

        self.extra_watch = None;
        self.extra_dir = needed_dir.clone();
        if let Some(dir) = needed_dir {
            self.extra_watch = DirWatchGuard::spawn(
                self.ctx.clone(),
                dir,
                false,
                Arc::clone(&self.shared),
            );
        }
    }

    /// Schedules a delayed retry for a path that failed with a transient file lock (e.g. sharing violation).
    pub fn schedule_retry(&mut self, path: PathBuf, attempt: u8) {
        if attempt >= 5 {
            return;
        }
        if self
            .pending_retries
            .iter()
            .any(|(p, _, _)| paths_equal(p, &path))
        {
            return;
        }
        self.pending_retries
            .push_back((path, std::time::Instant::now() + Duration::from_millis(120), attempt + 1));
        self.ctx.request_repaint_after(Duration::from_millis(130));
    }

    /// Drains any debounced filesystem notifications and due retry paths.
    pub fn poll(&mut self) -> Option<(WatchBatch, Vec<(PathBuf, u8)>)> {
        let mut batch = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default();
        let now = std::time::Instant::now();
        let mut due_retries = Vec::new();
        while let Some((_, when, _)) = self.pending_retries.front() {
            if *when <= now {
                let (path, _, attempt) = self.pending_retries.pop_front().unwrap();
                due_retries.push((path.clone(), attempt));
                if !batch.rescan_all
                    && !batch.paths.iter().any(|existing| paths_equal(existing, &path))
                {
                    batch.paths.push(path);
                }
            } else {
                break;
            }
        }
        if !self.pending_retries.is_empty() {
            if let Some((_, next_when, _)) = self.pending_retries.front() {
                self.ctx
                    .request_repaint_after(next_when.saturating_duration_since(now));
            }
        }
        if batch.is_empty() {
            None
        } else {
            Some((batch, due_retries))
        }
    }
}

#[cfg(target_os = "windows")]
struct DirWatchGuard {
    stop_event: SendHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
struct SendHandle(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for SendHandle {}

#[cfg(target_os = "windows")]
impl Drop for DirWatchGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.stop_event.0);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.stop_event.0);
        }
    }
}

#[cfg(target_os = "windows")]
impl DirWatchGuard {
    fn spawn(
        ctx: egui::Context,
        dir: PathBuf,
        recursive: bool,
        shared: Arc<Mutex<SharedQueue>>,
    ) -> Option<Self> {
        let stop_raw = unsafe {
            windows::Win32::System::Threading::CreateEventW(None, true, false, None)
        }
        .ok()?;
        let stop_event = SendHandle(stop_raw);
        let thread = std::thread::Builder::new()
            .name("rusted-fs-watcher".to_string())
            .spawn(move || {
                run_windows_watch_loop(ctx, dir, recursive, stop_event, shared);
            })
            .ok();
        if thread.is_none() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(stop_raw);
            }
            return None;
        }
        Some(Self { stop_event, thread })
    }
}

#[cfg(target_os = "windows")]
fn run_windows_watch_loop(
    ctx: egui::Context,
    dir: PathBuf,
    recursive: bool,
    stop_event: SendHandle,
    shared: Arc<Mutex<SharedQueue>>,
) {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_NOTIFY_ENUM_DIR, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, ResetEvent, WaitForMultipleObjects, INFINITE,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    const WAIT_OBJECT_1: u32 = WAIT_OBJECT_0.0 + 1;

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let Ok(dir_handle) = (unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            None,
        )
    }) else {
        return;
    };

    let Ok(io_event) = (unsafe { CreateEventW(None, true, false, None) }) else {
        unsafe {
            let _ = CloseHandle(dir_handle);
        }
        return;
    };

    // 32 KiB 4-byte aligned buffer works reliably on both local NTFS and SMB network shares.
    let mut buffer = vec![0u32; 8192];
    let mut overlapped = OVERLAPPED {
        hEvent: io_event,
        ..Default::default()
    };

    let issue_read = |buf: &mut [u32], ov: &mut OVERLAPPED| -> bool {
        unsafe {
            let _ = ResetEvent(io_event);
        }
        *ov = OVERLAPPED {
            hEvent: io_event,
            ..Default::default()
        };
        unsafe {
            ReadDirectoryChangesW(
                dir_handle,
                buf.as_mut_ptr().cast(),
                std::mem::size_of_val(buf) as u32,
                recursive,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
                None,
                Some(ov),
                None,
            )
            .is_ok()
        }
    };

    let mut io_pending = issue_read(&mut buffer, &mut overlapped);
    let mut pending_paths: Vec<PathBuf> = Vec::new();
    let mut pending_seen: HashSet<PathBuf> = HashSet::new();
    let mut pending_rescan = false;
    let mut first_event_at: Option<std::time::Instant> = None;
    let mut last_event_at: Option<std::time::Instant> = None;

    let flush_pending = |paths: &mut Vec<PathBuf>,
                         seen: &mut HashSet<PathBuf>,
                         rescan: &mut bool,
                         first_at: &mut Option<std::time::Instant>,
                         last_at: &mut Option<std::time::Instant>| {
        if !paths.is_empty() || *rescan {
            let drained = std::mem::take(paths);
            seen.clear();
            let rescan_flag = std::mem::take(rescan);
            shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_paths(drained, rescan_flag, &ctx);
        }
        *first_at = None;
        *last_at = None;
    };

    let handles = [stop_event.0, io_event];
    while io_pending {
        let wait_ms = if let (Some(first_at), Some(last_at)) = (first_event_at, last_event_at) {
            let now = std::time::Instant::now();
            let target = (last_at + DEBOUNCE_WINDOW).min(first_at + MAX_COALESCE_WINDOW);
            if target <= now {
                0
            } else {
                target.duration_since(now).as_millis().max(1) as u32
            }
        } else {
            INFINITE
        };

        let wait_res = unsafe { WaitForMultipleObjects(&handles, false, wait_ms) };
        match wait_res.0 {
            x if x == WAIT_OBJECT_0.0 => {
                unsafe {
                    let _ = CancelIoEx(dir_handle, Some(&overlapped));
                    let mut cancelled_bytes = 0u32;
                    let _ = GetOverlappedResult(dir_handle, &overlapped, &mut cancelled_bytes, true);
                }
                break;
            }
            x if x == WAIT_TIMEOUT.0 => {
                flush_pending(
                    &mut pending_paths,
                    &mut pending_seen,
                    &mut pending_rescan,
                    &mut first_event_at,
                    &mut last_event_at,
                );
            }
            WAIT_OBJECT_1 => {
                let mut bytes_transferred = 0u32;
                let res = unsafe {
                    GetOverlappedResult(dir_handle, &overlapped, &mut bytes_transferred, false)
                };
                if res.is_err() {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_NOTIFY_ENUM_DIR {
                        pending_rescan = true;
                        pending_paths.clear();
                        pending_seen.clear();
                    } else {
                        pending_rescan = true;
                        flush_pending(
                            &mut pending_paths,
                            &mut pending_seen,
                            &mut pending_rescan,
                            &mut first_event_at,
                            &mut last_event_at,
                        );
                        break;
                    }
                } else if bytes_transferred == 0 {
                    pending_rescan = true;
                    pending_paths.clear();
                    pending_seen.clear();
                } else {
                    let byte_slice = unsafe {
                        std::slice::from_raw_parts(
                            buffer.as_ptr().cast::<u8>(),
                            bytes_transferred as usize,
                        )
                    };
                    let mut offset = 0usize;
                    while offset + 12 <= byte_slice.len() && !pending_rescan {
                        let next_entry_offset = u32::from_le_bytes(
                            byte_slice[offset..offset + 4].try_into().unwrap(),
                        );
                        let file_name_bytes = u32::from_le_bytes(
                            byte_slice[offset + 8..offset + 12].try_into().unwrap(),
                        ) as usize;
                        let name_start = offset + 12;
                        let u16_count = file_name_bytes / 2;
                        if name_start + u16_count * 2 <= byte_slice.len() {
                            let mut wide_name = Vec::with_capacity(u16_count);
                            for chunk in
                                byte_slice[name_start..name_start + u16_count * 2].chunks_exact(2)
                            {
                                wide_name.push(u16::from_le_bytes([chunk[0], chunk[1]]));
                            }
                            let rel = std::ffi::OsString::from_wide(&wide_name);
                            let full_path = dir.join(rel);
                            let key = path_dedup_key(&full_path);
                            if pending_seen.insert(key) {
                                if pending_paths.len() >= MAX_QUEUED_PATHS {
                                    pending_rescan = true;
                                    pending_paths.clear();
                                    pending_seen.clear();
                                } else {
                                    pending_paths.push(full_path);
                                }
                            }
                        }
                        if next_entry_offset == 0 {
                            break;
                        }
                        offset += next_entry_offset as usize;
                    }
                }

                let now = std::time::Instant::now();
                if first_event_at.is_none() {
                    first_event_at = Some(now);
                }
                last_event_at = Some(now);
                if let Some(first_at) = first_event_at {
                    if now.duration_since(first_at) >= MAX_COALESCE_WINDOW {
                        flush_pending(
                            &mut pending_paths,
                            &mut pending_seen,
                            &mut pending_rescan,
                            &mut first_event_at,
                            &mut last_event_at,
                        );
                    }
                }

                io_pending = issue_read(&mut buffer, &mut overlapped);
            }
            _ => break,
        }
    }

    flush_pending(
        &mut pending_paths,
        &mut pending_seen,
        &mut pending_rescan,
        &mut first_event_at,
        &mut last_event_at,
    );

    unsafe {
        let _ = CloseHandle(io_event);
        let _ = CloseHandle(dir_handle);
    }
}

#[cfg(not(target_os = "windows"))]
struct DirWatchGuard;

#[cfg(not(target_os = "windows"))]
impl DirWatchGuard {
    fn spawn(
        _ctx: egui::Context,
        _dir: PathBuf,
        _recursive: bool,
        _shared: Arc<Mutex<SharedQueue>>,
    ) -> Option<Self> {
        None
    }
}
