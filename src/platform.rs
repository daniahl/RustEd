//! Platform-specific window hooks (Windows Alt-menu suppression, Win32 caret, and AccessKit focus sync).

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HBITMAP, HGDIOBJ};
    use windows::Win32::UI::Accessibility::NotifyWinEvent;
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateCaret, DestroyCaret, GetForegroundWindow, SetCaretPos, ShowCaret, CHILDID_SELF,
        EVENT_OBJECT_LOCATIONCHANGE, OBJID_CARET, SC_KEYMENU, WM_ACTIVATE, WM_ENTERMENULOOP,
        WM_EXITMENULOOP, WM_KILLFOCUS, WM_NCDESTROY, WM_SETFOCUS, WM_SYSCOMMAND,
    };

    const SUBCLASS_ID: usize = 0x5253_5445;
    const WM_SYSTIMER: u32 = 0x0118;
    const ID_CARET_TIMER: usize = 0xFFFF;

    #[derive(Default)]
    struct CaretState {
        hwnd: isize,
        bitmap: isize,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        created_w: i32,
        created_h: i32,
        window_focused: bool,
        caret_created: bool,
        pre_activated: bool,
    }

    // Win32 window subclassing and caret APIs are strictly main-thread-affine; `thread_local!`
    // with `try_borrow_mut()` avoids atomic locking on every frame and cannot deadlock if an
    // accessibility hook dispatches a synchronous message on the UI thread.
    std::thread_local! {
        static CARET: RefCell<CaretState> = RefCell::new(CaretState::default());
    }

    #[inline]
    fn with_caret<R>(f: impl FnOnce(&mut CaretState) -> R) -> Option<R> {
        CARET
            .try_with(|cell| cell.try_borrow_mut().ok().map(|mut s| f(&mut s)))
            .ok()
            .flatten()
    }

    unsafe fn apply_caret(state: &mut CaretState) -> Option<HWND> {
        if state.hwnd == 0 || !state.window_focused || state.w <= 0 || state.h <= 0 {
            return None;
        }
        let hwnd = HWND(state.hwnd as *mut _);
        let size_changed = state.created_w != state.w || state.created_h != state.h;
        if !state.caret_created || size_changed {
            let (hbmp, created_new) = if !size_changed && state.bitmap != 0 {
                (HBITMAP(state.bitmap as *mut _), false)
            } else {
                let stride_words = ((state.w + 15) / 16) as usize;
                let words = vec![0u16; stride_words * (state.h as usize)];
                let bmp = unsafe {
                    CreateBitmap(state.w, state.h, 1, 1, Some(words.as_ptr() as *const _))
                };
                if bmp.is_invalid() {
                    return None;
                }
                (bmp, true)
            };
            if unsafe { CreateCaret(hwnd, Some(hbmp), state.w, state.h) }.is_err() {
                if created_new {
                    let _ = unsafe { DeleteObject(HGDIOBJ(hbmp.0)) };
                }
                return None;
            }
            if created_new {
                if state.bitmap != 0 {
                    let _ = unsafe { DeleteObject(HGDIOBJ(state.bitmap as *mut _)) };
                }
                state.bitmap = hbmp.0 as isize;
                state.created_w = state.w;
                state.created_h = state.h;
            }
            state.caret_created = true;
            let _ = unsafe { SetCaretPos(state.x, state.y) };
            let _ = unsafe { ShowCaret(Some(hwnd)) };
        } else {
            let _ = unsafe { SetCaretPos(state.x, state.y) };
        }
        Some(hwnd)
    }

    unsafe fn notify_caret(hwnd: HWND) {
        unsafe {
            NotifyWinEvent(
                EVENT_OBJECT_LOCATIONCHANGE,
                hwnd,
                OBJID_CARET.0,
                CHILDID_SELF as i32,
            );
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) == SC_KEYMENU as usize && lparam.0 == 0 {
            return LRESULT(0);
        }
        if msg == WM_SYSTIMER && wparam.0 == ID_CARET_TIMER {
            return LRESULT(0);
        }
        if msg == WM_ACTIVATE && (wparam.0 & 0xFFFF) != 0 {
            // `DefWindowProcW(WM_ACTIVATE)` fires `EVENT_SYSTEM_FOREGROUND` before sending
            // `WM_SETFOCUS`. Pre-notify `accesskit_windows` via `WM_EXITMENULOOP` so its tree
            // reports `is_host_focused = true` when UIA queries `GetFocus()` during foreground
            // activation.
            with_caret(|state| {
                state.pre_activated = true;
            });
            let _ = unsafe { DefSubclassProc(hwnd, WM_EXITMENULOOP, WPARAM(0), LPARAM(0)) };
        }
        if msg == WM_SETFOCUS {
            let was_pre_activated =
                with_caret(|state| std::mem::replace(&mut state.pre_activated, false))
                    .unwrap_or(false);
            if was_pre_activated {
                // Balance the synthetic `WM_EXITMENULOOP` above and reset `accesskit_windows`
                // focus state to `false` first so the real `WM_SETFOCUS` transitions
                // `false -> true` and raises `UIA_AutomationFocusChangedEventId`.
                let _ = unsafe { DefSubclassProc(hwnd, WM_ENTERMENULOOP, WPARAM(0), LPARAM(0)) };
            }
        }
        if msg == WM_NCDESTROY {
            with_caret(|state| {
                if state.caret_created {
                    let _ = unsafe { DestroyCaret() };
                }
                if state.bitmap != 0 {
                    let _ = unsafe { DeleteObject(HGDIOBJ(state.bitmap as *mut _)) };
                }
                *state = CaretState::default();
            });
            let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
        }
        let res = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        if msg == WM_SETFOCUS {
            let notify_hwnd = with_caret(|state| {
                state.window_focused = true;
                state.caret_created = false;
                unsafe { apply_caret(state) }
            })
            .flatten();
            if let Some(h) = notify_hwnd {
                unsafe { notify_caret(h) };
            }
        } else if msg == WM_KILLFOCUS {
            with_caret(|state| {
                state.window_focused = false;
                state.pre_activated = false;
                if state.caret_created {
                    let _ = unsafe { DestroyCaret() };
                    state.caret_created = false;
                }
            });
        }
        res
    }

    pub fn install(handle: raw_window_handle::RawWindowHandle) {
        if let raw_window_handle::RawWindowHandle::Win32(h) = handle {
            let hwnd = HWND(h.hwnd.get() as *mut _);
            let focused = unsafe { GetForegroundWindow() == hwnd };
            with_caret(|state| {
                state.hwnd = hwnd.0 as isize;
                state.window_focused = focused;
            });
            unsafe {
                let _ = SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0);
            }
        }
    }

    pub fn update_caret(x: i32, y: i32, w: i32, h: i32, force_notify: bool) {
        let notify_hwnd = with_caret(|state| {
            let changed = force_notify
                || !state.caret_created
                || state.x != x
                || state.y != y
                || state.w != w
                || state.h != h;
            state.x = x;
            state.y = y;
            state.w = w;
            state.h = h;
            if changed && state.window_focused {
                unsafe { apply_caret(state) }
            } else {
                None
            }
        })
        .flatten();
        if let Some(h) = notify_hwnd {
            unsafe { notify_caret(h) };
        }
    }
}

#[cfg(windows)]
pub fn install_window_hooks(handle: raw_window_handle::RawWindowHandle) {
    win::install(handle);
}

#[cfg(not(windows))]
pub fn install_window_hooks(_handle: raw_window_handle::RawWindowHandle) {}

#[cfg(windows)]
pub fn update_caret(x: i32, y: i32, w: i32, h: i32, force_notify: bool) {
    win::update_caret(x, y, w, h, force_notify);
}

#[cfg(not(windows))]
pub fn update_caret(_x: i32, _y: i32, _w: i32, _h: i32, _force_notify: bool) {}
