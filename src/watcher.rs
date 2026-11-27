//! Filesystem watcher for automatic file-tree and editor synchronization.
//!
//! Uses overlapped Win32 `ReadDirectoryChangesW` on Windows with short burst debouncing so
//! external modifications (e.g. from the integrated terminal or other tools) wake the UI via
//! `egui::Context::request_repaint()`. Provides a no-op implementation on non-Windows targets.

use eframe::egui;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
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

/// Strips a Windows `\\?\` or `\\?\UNC\` verbatim prefix so canonical and regular paths compare cleanly.
fn normalize_verbatim(path: &Path) -> Cow<'_, Path> {
    #[cfg(target_os = "windows")]
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Cow::Owned(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return Cow::Owned(PathBuf::from(rest));
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

/// Normalized, hashable key for a path that ignores Windows verbatim prefixes,
/// case, and separator differences. Used for O(1) deduplication in `HashSet`s.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathKey(PathBuf);

impl PathKey {
    pub fn new(path: &Path) -> Self {
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
            Self(out)
        }
        #[cfg(not(target_os = "windows"))]
        {
            Self(norm.into_owned())
        }
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl From<&Path> for PathKey {
    fn from(path: &Path) -> Self {
        Self::new(path)
    }
}

impl From<&PathBuf> for PathKey {
    fn from(path: &PathBuf) -> Self {
        Self::new(path)
    }
}

/// Windows-aware O(1) map keyed by [`PathKey`] that preserves each entry's original [`PathBuf`].
#[derive(Clone, Debug)]
pub struct PathMap<V> {
    inner: HashMap<PathKey, (PathBuf, V)>,
}

impl<V> Default for PathMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> PathMap<V> {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn insert(&mut self, path: PathBuf, value: V) -> Option<V> {
        let key = PathKey::new(&path);
        self.inner.insert(key, (path, value)).map(|(_, old)| old)
    }

    pub fn get(&self, path: &Path) -> Option<&V> {
        self.inner.get(&PathKey::new(path)).map(|(_, v)| v)
    }

    pub fn remove(&mut self, path: &Path) -> Option<V> {
        self.inner.remove(&PathKey::new(path)).map(|(_, v)| v)
    }

    pub fn contains_key(&self, path: &Path) -> bool {
        self.inner.contains_key(&PathKey::new(path))
    }

    pub fn keys(&self) -> impl Iterator<Item = &PathBuf> {
        self.inner.values().map(|(p, _)| p)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&PathBuf, &V)> {
        self.inner.values().map(|(p, v)| (p, v))
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&PathBuf, &mut V) -> bool,
    {
        self.inner.retain(|_, (p, v)| f(p, v));
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
    seen: HashSet<PathKey>,
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
                let key = PathKey::from(&p);
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
        // Drop any stale queued events from the previous root so a `rescan_all` from the old
        // watcher doesn't trigger a redundant full rescan of the newly opened folder.
        let _ = self.shared.lock().unwrap_or_else(|e| e.into_inner()).take();
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
struct OwnedHandle(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for OwnedHandle {}

#[cfg(target_os = "windows")]
unsafe impl Sync for OwnedHandle {}

#[cfg(target_os = "windows")]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
struct DirWatchGuard {
    stop_event: Arc<OwnedHandle>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(target_os = "windows")]
impl Drop for DirWatchGuard {
    fn drop(&mut self) {
        self.stopped.store(true, std::sync::atomic::Ordering::Release);
        // Wake the watcher thread without blocking the UI thread (important on SMB/network shares).
        // The thread closes `dir_handle` and `io_event` on exit, and `OwnedHandle` closes
        // `stop_event` once both the guard and the watcher thread have dropped their `Arc`.
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.stop_event.0);
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
        let stop_event = Arc::new(OwnedHandle(stop_raw));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<bool>(1);

        let stop_event_clone = Arc::clone(&stop_event);
        let stopped_clone = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("rusted-fs-watcher".to_string())
            .spawn(move || {
                run_windows_watch_loop(
                    ctx,
                    dir,
                    recursive,
                    stop_event_clone,
                    stopped_clone,
                    shared,
                    ready_tx,
                );
            })
            .ok()?;

        // Wait briefly for the watcher thread to open `dir_handle` and arm `ReadDirectoryChangesW`
        // so immediate local writes don't race ahead of registration, while bounding the wait so
        // slow or unreachable SMB/network shares cannot stall the UI thread.
        match ready_rx.recv_timeout(Duration::from_millis(150)) {
            Ok(true) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Some(Self {
                stop_event,
                stopped,
            }),
            Ok(false) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }
}

#[cfg(target_os = "windows")]
fn run_windows_watch_loop(
    ctx: egui::Context,
    dir: PathBuf,
    recursive: bool,
    stop_event: Arc<OwnedHandle>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    shared: Arc<Mutex<SharedQueue>>,
    ready_tx: std::sync::mpsc::SyncSender<bool>,
) {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::atomic::Ordering;
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
        let _ = ready_tx.send(false);
        return;
    };

    let Ok(io_event) = (unsafe { CreateEventW(None, true, false, None) }) else {
        unsafe {
            let _ = CloseHandle(dir_handle);
        }
        let _ = ready_tx.send(false);
        return;
    };

    // RAII guard ensures any in-flight overlapped `ReadDirectoryChangesW` IRP is cancelled and
    // waited to completion before `buffer` / `overlapped` are dropped—even if the loop unwinds.
    struct OverlappedDirWatch {
        dir_handle: windows::Win32::Foundation::HANDLE,
        io_event: windows::Win32::Foundation::HANDLE,
        // 32 KiB 4-byte aligned buffer works reliably on both local NTFS and SMB network shares.
        buffer: Vec<u32>,
        overlapped: Box<OVERLAPPED>,
        io_pending: bool,
    }

    impl OverlappedDirWatch {
        fn issue_read(&mut self, recursive: bool) -> bool {
            unsafe {
                let _ = ResetEvent(self.io_event);
            }
            *self.overlapped = OVERLAPPED {
                hEvent: self.io_event,
                ..Default::default()
            };
            self.io_pending = unsafe {
                ReadDirectoryChangesW(
                    self.dir_handle,
                    self.buffer.as_mut_ptr().cast(),
                    std::mem::size_of_val(self.buffer.as_slice()) as u32,
                    recursive,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    None,
                    Some(&mut *self.overlapped),
                    None,
                )
                .is_ok()
            };
            self.io_pending
        }
    }

    impl Drop for OverlappedDirWatch {
        fn drop(&mut self) {
            if self.io_pending {
                unsafe {
                    let _ = CancelIoEx(self.dir_handle, Some(&*self.overlapped));
                    let mut cancelled_bytes = 0u32;
                    let _ = GetOverlappedResult(
                        self.dir_handle,
                        &*self.overlapped,
                        &mut cancelled_bytes,
                        true,
                    );
                }
                self.io_pending = false;
            }
            unsafe {
                let _ = CloseHandle(self.io_event);
                let _ = CloseHandle(self.dir_handle);
            }
        }
    }

    let mut watch = OverlappedDirWatch {
        dir_handle,
        io_event,
        buffer: vec![0u32; 8192],
        overlapped: Box::new(OVERLAPPED {
            hEvent: io_event,
            ..Default::default()
        }),
        io_pending: false,
    };

    let armed = watch.issue_read(recursive);
    let _ = ready_tx.send(armed);
    let mut pending_paths: Vec<PathBuf> = Vec::new();
    let mut pending_seen: HashSet<PathKey> = HashSet::new();
    let mut pending_rescan = false;
    let mut first_event_at: Option<std::time::Instant> = None;
    let mut last_event_at: Option<std::time::Instant> = None;

    let flush_pending = |paths: &mut Vec<PathBuf>,
                         seen: &mut HashSet<PathKey>,
                         rescan: &mut bool,
                         first_at: &mut Option<std::time::Instant>,
                         last_at: &mut Option<std::time::Instant>| {
        if (!paths.is_empty() || *rescan) && !stopped.load(Ordering::Acquire) {
            let drained = std::mem::take(paths);
            seen.clear();
            let rescan_flag = std::mem::take(rescan);
            shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_paths(drained, rescan_flag, &ctx);
        } else {
            paths.clear();
            seen.clear();
            *rescan = false;
        }
        *first_at = None;
        *last_at = None;
    };

    let handles = [stop_event.0, watch.io_event];
    let mut stopped_by_request = false;
    while watch.io_pending {
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
                stopped_by_request = true;
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
                watch.io_pending = false;
                let mut bytes_transferred = 0u32;
                let res = unsafe {
                    GetOverlappedResult(
                        watch.dir_handle,
                        &*watch.overlapped,
                        &mut bytes_transferred,
                        false,
                    )
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
                            watch.buffer.as_ptr().cast::<u8>(),
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
                            for &chunk in
                                byte_slice[name_start..name_start + u16_count * 2]
                                    .as_chunks::<2>()
                                    .0
                            {
                                wide_name.push(u16::from_le_bytes(chunk));
                            }
                            let rel = std::ffi::OsString::from_wide(&wide_name);
                            let full_path = dir.join(rel);
                            let key = PathKey::from(&full_path);
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
                        let Some(next_off) = offset.checked_add(next_entry_offset as usize) else {
                            break;
                        };
                        offset = next_off;
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

                watch.issue_read(recursive);
            }
            _ => break,
        }
    }

    if !stopped_by_request {
        flush_pending(
            &mut pending_paths,
            &mut pending_seen,
            &mut pending_rescan,
            &mut first_event_at,
            &mut last_event_at,
        );
    }
    drop(watch);
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
