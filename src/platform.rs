//! Platform-specific window tweaks.

#[cfg(windows)]
mod win {
    use std::sync::Mutex;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
    use windows::Win32::UI::Accessibility::NotifyWinEvent;
    use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateCaret, DestroyCaret, SetCaretPos, ShowCaret, CHILDID_SELF,
        EVENT_OBJECT_LOCATIONCHANGE, OBJID_CARET, SC_KEYMENU, WM_ACTIVATE, WM_ENTERMENULOOP,
        WM_EXITMENULOOP, WM_KILLFOCUS, WM_SETFOCUS, WM_SYSCOMMAND,
    };

    const WM_SYSTIMER: u32 = 0x0118;

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
    }

    static CARET: Mutex<CaretState> = Mutex::new(CaretState {
        hwnd: 0,
        bitmap: 0,
        x: 0,
        y: 0,
        w: 0,
        h: 0,
        created_w: 0,
        created_h: 0,
        window_focused: false,
        caret_created: false,
    });

    unsafe fn apply_caret(state: &mut CaretState) -> Option<HWND> {
        if state.hwnd == 0 || !state.window_focused || state.w <= 0 || state.h <= 0 {
            return None;
        }
        let hwnd = HWND(state.hwnd as *mut _);
        if !state.caret_created || state.created_w != state.w || state.created_h != state.h {
            let stride_words = ((state.w + 15) / 16) as usize;
            let words = vec![0u16; stride_words * (state.h as usize)];
            let hbmp =
                unsafe { CreateBitmap(state.w, state.h, 1, 1, Some(words.as_ptr() as *const _)) };
            let _ = unsafe { CreateCaret(hwnd, Some(hbmp), state.w, state.h) };
            if state.bitmap != 0 {
                let _ = unsafe { DeleteObject(HGDIOBJ(state.bitmap as *mut _)) };
            }
            state.bitmap = hbmp.0 as isize;
            state.created_w = state.w;
            state.created_h = state.h;
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
        if msg == WM_SYSTIMER {
            return LRESULT(0);
        }
        if msg == WM_ACTIVATE && (wparam.0 & 0xFFFF) != 0 {
            // `DefWindowProcW(WM_ACTIVATE)` fires `EVENT_SYSTEM_FOREGROUND` before sending
            // `WM_SETFOCUS`. Pre-notify `accesskit_windows` via `WM_EXITMENULOOP` so its tree
            // reports `is_host_focused = true` when UIA queries `GetFocus()` during foreground
            // activation.
            let _ = unsafe { DefSubclassProc(hwnd, WM_EXITMENULOOP, WPARAM(0), LPARAM(0)) };
        }
        if msg == WM_SETFOCUS {
            // Reset `accesskit_windows` focus state to `false` first so the real `WM_SETFOCUS`
            // still transitions `false -> true` and raises `UIA_AutomationFocusChangedEventId`.
            let _ = unsafe { DefSubclassProc(hwnd, WM_ENTERMENULOOP, WPARAM(0), LPARAM(0)) };
        }
        let res = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
        if msg == WM_SETFOCUS {
            let notify_hwnd = CARET.lock().ok().and_then(|mut state| {
                state.window_focused = true;
                state.caret_created = false;
                unsafe { apply_caret(&mut state) }
            });
            if let Some(h) = notify_hwnd {
                unsafe { notify_caret(h) };
            }
        } else if msg == WM_KILLFOCUS {
            if let Ok(mut state) = CARET.lock() {
                state.window_focused = false;
                if state.caret_created {
                    let _ = unsafe { DestroyCaret() };
                    state.caret_created = false;
                }
            }
        }
        res
    }

    pub fn install(handle: raw_window_handle::RawWindowHandle) {
        if let raw_window_handle::RawWindowHandle::Win32(h) = handle {
            let hwnd = HWND(h.hwnd.get() as *mut _);
            if let Ok(mut state) = CARET.lock() {
                state.hwnd = hwnd.0 as isize;
                state.window_focused = true;
            }
            unsafe {
                let _ = SetWindowSubclass(hwnd, Some(subclass_proc), 0x5253_5445, 0);
            }
        }
    }

    pub fn update_caret(x: i32, y: i32, w: i32, h: i32, force_notify: bool) {
        let notify_hwnd = CARET.lock().ok().and_then(|mut state| {
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
                unsafe { apply_caret(&mut state) }
            } else {
                None
            }
        });
        if let Some(h) = notify_hwnd {
            unsafe { notify_caret(h) };
        }
    }
}

#[cfg(windows)]
pub fn suppress_alt_menu_mode(handle: raw_window_handle::RawWindowHandle) {
    win::install(handle);
}

#[cfg(not(windows))]
pub fn suppress_alt_menu_mode(_handle: ()) {}

#[cfg(windows)]
pub fn update_caret(x: i32, y: i32, w: i32, h: i32, force_notify: bool) {
    win::update_caret(x, y, w, h, force_notify);
}

#[cfg(not(windows))]
pub fn update_caret(_x: i32, _y: i32, _w: i32, _h: i32, _force_notify: bool) {}
