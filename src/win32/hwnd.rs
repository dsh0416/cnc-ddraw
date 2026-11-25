//! Safe HWND inspection, traversal, messaging, lifecycle, and window-class helpers.
use std::mem::size_of;
use std::sync::OnceLock;

use windows::core::{Result, BOOL, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcA, DefWindowProcW, DestroyWindow,
    EnumThreadWindows, GetClassNameW, GetDesktopWindow, GetForegroundWindow, GetParent, GetWindow,
    GetWindowLongW, GetWindowPlacement, GetWindowThreadProcessId, IsChild, IsIconic,
    IsWindowUnicode, IsWindowVisible, KillTimer, MoveWindow, PostMessageA, PostMessageW,
    RegisterClassW, SendMessageW, SetTimer, SetWindowLongW, SetWindowPlacement, SetWindowPos,
    GWL_EXSTYLE, GWL_STYLE, GW_CHILD, GW_HWNDNEXT, GW_HWNDPREV, GW_OWNER, SET_WINDOW_POS_FLAGS,
    WINDOWPLACEMENT, WINDOW_EX_STYLE, WINDOW_LONG_PTR_INDEX, WINDOW_STYLE, WNDCLASSW,
};

pub(crate) type SafeWndProc = fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

/// Opaque Win32 `HWND` wrapper that implements `Send + Sync` so higher-level window state
/// structs do not need broad `unsafe impl Send`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SendHwnd(pub(crate) HWND);

// SAFETY: A Win32 `HWND` is an opaque kernel handle integer, not a Rust pointer/reference.
// All Win32 operations on it are synchronized by our owning locks or dispatched on the UI thread.
unsafe impl Send for SendHwnd {}
// SAFETY: Sharing an opaque `HWND` value across threads carries no Rust aliasing hazard.
unsafe impl Sync for SendHwnd {}

impl SendHwnd {
    pub(crate) fn get(self) -> HWND {
        self.0
    }
}

pub(crate) fn desktop_window() -> HWND {
    // SAFETY: GetDesktopWindow takes no arguments.
    unsafe { GetDesktopWindow() }
}

pub(crate) fn foreground_window() -> HWND {
    // SAFETY: GetForegroundWindow takes no arguments.
    unsafe { GetForegroundWindow() }
}

pub(crate) fn is_visible(hwnd: HWND) -> bool {
    // SAFETY: IsWindowVisible safely handles any HWND value.
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

pub(crate) fn is_iconic(hwnd: HWND) -> bool {
    // SAFETY: IsIconic safely handles any HWND value.
    unsafe { IsIconic(hwnd).as_bool() }
}

pub(crate) fn is_child(parent: HWND, hwnd: HWND) -> bool {
    // SAFETY: IsChild safely handles any HWND values.
    unsafe { IsChild(parent, hwnd).as_bool() }
}

pub(crate) fn is_unicode(hwnd: HWND) -> bool {
    // SAFETY: IsWindowUnicode safely handles any HWND value.
    unsafe { IsWindowUnicode(hwnd).as_bool() }
}

pub(crate) fn style(hwnd: HWND) -> i32 {
    // SAFETY: GetWindowLongW safely queries window style.
    unsafe { GetWindowLongW(hwnd, GWL_STYLE) }
}

pub(crate) fn ex_style(hwnd: HWND) -> i32 {
    // SAFETY: GetWindowLongW safely queries extended window style.
    unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) }
}

pub(crate) fn set_window_long(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, value: i32) -> Result<()> {
    // SAFETY: Clearing last error distinguishes a genuine 0 previous value from failure.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        if SetWindowLongW(hwnd, index, value) == 0 {
            let error = GetLastError();
            if error != ERROR_SUCCESS {
                return Err(error.to_hresult().into());
            }
        }
    }
    Ok(())
}

pub(crate) fn window_placement(hwnd: HWND) -> Result<WINDOWPLACEMENT> {
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: `placement.length` is initialized to the exact struct size.
    unsafe { GetWindowPlacement(hwnd, &mut placement)? };
    Ok(placement)
}

pub(crate) fn set_window_placement(hwnd: HWND, placement: &WINDOWPLACEMENT) -> Result<()> {
    // SAFETY: `placement` is a valid reference to an initialized WINDOWPLACEMENT.
    unsafe { SetWindowPlacement(hwnd, placement) }
}

pub(crate) fn class_name(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is a writable UTF-16 buffer; GetClassNameW writes at most `buf.len()`.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    (len > 0 && len <= buf.len()).then(|| String::from_utf16_lossy(&buf[..len]))
}

pub(crate) fn window_thread_id(hwnd: HWND) -> u32 {
    // SAFETY: Querying thread ID with `None` for process ID does not dereference any pointer.
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

pub(crate) fn window_process_id(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid mutable u32.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

pub(crate) fn owner(hwnd: HWND) -> Option<HWND> {
    // SAFETY: GetWindow(GW_OWNER) safely queries the owner handle.
    unsafe { GetWindow(hwnd, GW_OWNER) }
        .ok()
        .filter(|h| !h.is_invalid())
}

pub(crate) fn parent(hwnd: HWND) -> Option<HWND> {
    // SAFETY: GetParent safely queries the parent/owner handle.
    unsafe { GetParent(hwnd) }.ok().filter(|h| !h.is_invalid())
}

/// Lazily iterates direct child windows in top-to-bottom Z-order.
pub(crate) fn children(hwnd: HWND) -> impl Iterator<Item = HWND> {
    // SAFETY: GetWindow(GW_CHILD) returns the first child or an error.
    let first = unsafe { GetWindow(hwnd, GW_CHILD) }
        .ok()
        .filter(|h| !h.is_invalid());
    std::iter::successors(first, |&cur| {
        // SAFETY: GetWindow(GW_HWNDNEXT) advances to the next sibling.
        unsafe { GetWindow(cur, GW_HWNDNEXT) }
            .ok()
            .filter(|h| !h.is_invalid())
    })
}

/// Lazily iterates preceding sibling windows in bottom-to-top Z-order, pre-fetching the next
/// handle before yielding so callers may safely change the yielded window's Z-order.
pub(crate) fn prev_siblings(hwnd: HWND) -> PrevSiblings {
    // SAFETY: GetWindow(GW_HWNDPREV) returns the previous sibling or an error.
    let next = unsafe { GetWindow(hwnd, GW_HWNDPREV) }
        .ok()
        .filter(|h| !h.is_invalid());
    PrevSiblings { next }
}

pub(crate) struct PrevSiblings {
    next: Option<HWND>,
}

impl Iterator for PrevSiblings {
    type Item = HWND;

    fn next(&mut self) -> Option<Self::Item> {
        let cur = self.next?;
        // SAFETY: Pre-fetch the next sibling before yielding `cur`.
        self.next = unsafe { GetWindow(cur, GW_HWNDPREV) }
            .ok()
            .filter(|h| !h.is_invalid());
        Some(cur)
    }
}

/// Safely enumerates top-level windows belonging to `tid` without exposing raw `LPARAM` casts.
pub(crate) fn enum_thread_windows<F: FnMut(HWND) -> bool>(tid: u32, mut f: F) {
    unsafe extern "system" fn trampoline<F: FnMut(HWND) -> bool>(
        hwnd: HWND,
        lparam: LPARAM,
    ) -> BOOL {
        // SAFETY: `lparam` points to the live stack-allocated `F` for the duration of
        // synchronous `EnumThreadWindows`.
        let cb = unsafe { &mut *(lparam.0 as *mut F) };
        BOOL::from(cb(hwnd))
    }

    // SAFETY: `EnumThreadWindows` invokes `trampoline` synchronously before returning, so `&mut f`
    // remains exclusively valid for every callback invocation.
    unsafe {
        let _ = EnumThreadWindows(tid, Some(trampoline::<F>), LPARAM((&raw mut f) as isize));
    }
}

pub(crate) fn set_window_pos(
    hwnd: HWND,
    insert_after: Option<HWND>,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    flags: SET_WINDOW_POS_FLAGS,
) -> Result<()> {
    // SAFETY: SetWindowPos validates HWND arguments internally.
    unsafe { SetWindowPos(hwnd, insert_after, x, y, cx, cy, flags) }
}

pub(crate) fn bring_window_to_top(hwnd: HWND) -> bool {
    // SAFETY: BringWindowToTop validates the HWND internally.
    unsafe { BringWindowToTop(hwnd).is_ok() }
}

pub(crate) fn move_window(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    repaint: bool,
) -> bool {
    // SAFETY: MoveWindow validates the HWND internally.
    unsafe { MoveWindow(hwnd, x, y, width, height, repaint).is_ok() }
}

pub(crate) fn destroy_window(hwnd: HWND) {
    // SAFETY: DestroyWindow validates the HWND internally.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

pub(crate) fn post_message_w(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    // SAFETY: PostMessageW posts a message asynchronously without dereferencing caller pointers.
    unsafe { PostMessageW(Some(hwnd), msg, wparam, lparam).is_ok() }
}

/// Posts a message using Unicode or ANSI dispatch matching the target window's encoding.
pub(crate) fn post_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    if is_unicode(hwnd) {
        post_message_w(hwnd, msg, wparam, lparam)
    } else {
        // SAFETY: PostMessageA posts a message asynchronously to the target window.
        unsafe { PostMessageA(Some(hwnd), msg, wparam, lparam).is_ok() }
    }
}

pub(crate) fn send_message_w(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: Callers pass only scalar/handle WPARAM and LPARAM values.
    unsafe { SendMessageW(hwnd, msg, Some(wparam), Some(lparam)) }
}

pub(crate) fn def_window_proc_w(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: DefWindowProcW forwards unhandled window messages to the default Win32 handler.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub(crate) fn def_window_proc_a(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: DefWindowProcA forwards unhandled window messages to the default ANSI Win32 handler.
    unsafe { DefWindowProcA(hwnd, msg, wparam, lparam) }
}

pub(crate) fn set_timer(hwnd: HWND, id: usize, elapse_ms: u32) {
    // SAFETY: Passing None for the timer proc posts WM_TIMER to the window message queue.
    unsafe {
        let _ = SetTimer(Some(hwnd), id, elapse_ms, None);
    }
}

pub(crate) fn kill_timer(hwnd: HWND, id: usize) {
    // SAFETY: KillTimer safely ignores unknown timer IDs.
    unsafe {
        let _ = KillTimer(Some(hwnd), id);
    }
}

static OVERLAY_WNDPROC: OnceLock<SafeWndProc> = OnceLock::new();

unsafe extern "system" fn overlay_wndproc_trampoline(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(&handler) = OVERLAY_WNDPROC.get() {
        handler(hwnd, msg, wparam, lparam)
    } else {
        def_window_proc_w(hwnd, msg, wparam, lparam)
    }
}

/// Registers the presentation overlay window class once with a safe Rust window procedure.
pub(crate) fn register_overlay_class(
    class_name: PCWSTR,
    hinstance: HINSTANCE,
    handler: SafeWndProc,
) {
    let _ = OVERLAY_WNDPROC.set(handler);
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(overlay_wndproc_trampoline),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        // SAFETY: `class_name` is a static wide string and `wc` is fully initialized.
        unsafe {
            let _ = RegisterClassW(&wc);
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_window_ex_w(
    ex_style: WINDOW_EX_STYLE,
    class_name: PCWSTR,
    title: PCWSTR,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: Option<HWND>,
    hinstance: Option<HINSTANCE>,
) -> Result<HWND> {
    // SAFETY: String arguments are valid NUL-terminated wide strings and lpParam is None.
    unsafe {
        CreateWindowExW(
            ex_style, class_name, title, style, x, y, width, height, parent, None, hinstance, None,
        )
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, MSG, PM_REMOVE};

    static TEST_WNDPROC: OnceLock<SafeWndProc> = OnceLock::new();

    unsafe extern "system" fn test_wndproc_trampoline(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if let Some(&handler) = TEST_WNDPROC.get() {
            handler(hwnd, msg, wparam, lparam)
        } else {
            def_window_proc_w(hwnd, msg, wparam, lparam)
        }
    }

    pub(crate) fn register_test_class(class_name: PCWSTR, handler: SafeWndProc) {
        let _ = TEST_WNDPROC.set(handler);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(test_wndproc_trampoline),
            lpszClassName: class_name,
            ..Default::default()
        };
        // SAFETY: `class_name` is a static wide string and `wc` is initialized.
        unsafe {
            let _ = RegisterClassW(&wc);
        }
    }

    pub(crate) fn consume_paint(hwnd: HWND) {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: BeginPaint/EndPaint are paired on the same HWND inside WM_PAINT.
        unsafe {
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
        }
    }

    pub(crate) fn invalidate_window(hwnd: HWND) {
        // SAFETY: Passing None invalidates the entire client area.
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    pub(crate) fn pump_thread_messages() {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid writable MSG; PeekMessageW/DispatchMessageW run on this thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
        }
    }
}
