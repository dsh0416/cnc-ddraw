//! Logical <-> physical coordinate conversion, Win32 dialog hit-testing, and overlay mouse routing.
use std::ffi::c_void;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, EnumThreadWindows, GetClassNameW, GetClientRect, GetParent, GetWindow,
    GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsChild, IsWindowUnicode,
    IsWindowVisible, PostMessageA, PostMessageW, SendMessageW, GWL_STYLE, GW_CHILD, GW_HWNDNEXT,
    GW_OWNER, HTCLIENT, HTTRANSPARENT, MA_NOACTIVATE, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCHITTEST, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR,
    WS_DISABLED,
};

use crate::config;

use super::overlay::keep_present_on_top;
use super::{
    bound_hwnd_raw, bound_thread_id, compute_viewport, is_present_hwnd, logical_mode,
    with_window_dpi,
};

const SS_NOTIFY: u32 = 0x0100;

pub(crate) fn client_to_logical(hwnd: HWND, px: i32, py: i32, lw: u32, lh: u32) -> (i32, i32) {
    let mut client = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err()
        || client.right <= 0
        || client.bottom <= 0
    {
        return (px.clamp(0, lw as i32 - 1), py.clamp(0, lh as i32 - 1));
    }
    let (vx, vy, vw, vh) = compute_viewport(
        lw,
        lh,
        client.right as u32,
        client.bottom as u32,
        config::get().maintas,
    );
    let rx = (px - vx as i32).clamp(0, vw as i32 - 1) as i64;
    let ry = (py - vy as i32).clamp(0, vh as i32 - 1) as i64;
    let lx = ((rx * lw as i64) / vw.max(1) as i64).clamp(0, lw as i64 - 1) as i32;
    let ly = ((ry * lh as i64) / vh.max(1) as i64).clamp(0, lh as i64 - 1) as i32;
    (lx, ly)
}

pub(crate) fn logical_to_client(hwnd: HWND, lx: i32, ly: i32, lw: u32, lh: u32) -> (i32, i32) {
    let mut client = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err()
        || client.right <= 0
        || client.bottom <= 0
    {
        return (lx, ly);
    }
    let (vx, vy, vw, vh) = compute_viewport(
        lw,
        lh,
        client.right as u32,
        client.bottom as u32,
        config::get().maintas,
    );
    let cx = vx as i32 + ((lx.clamp(0, lw as i32 - 1) as i64 * vw as i64) / lw as i64) as i32;
    let cy = vy as i32 + ((ly.clamp(0, lh as i32 - 1) as i64 * vh as i64) / lh as i64) as i32;
    (cx, cy)
}

fn window_rect_in_bound(bound: HWND, hwnd: HWND) -> Option<RECT> {
    let mut rc = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rc) }.ok()?;
    let mut origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(bound, &mut origin) }.as_bool() {
        rc.left -= origin.x;
        rc.right -= origin.x;
        rc.top -= origin.y;
        rc.bottom -= origin.y;
    }
    Some(rc)
}

pub(crate) fn client_origin_in_bound(bound: HWND, hwnd: HWND) -> (i32, i32) {
    let mut target_origin = POINT { x: 0, y: 0 };
    let mut bound_origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd, &mut target_origin) }.as_bool()
        && unsafe { ClientToScreen(bound, &mut bound_origin) }.as_bool()
    {
        (
            target_origin.x - bound_origin.x,
            target_origin.y - bound_origin.y,
        )
    } else if let Some(rc) = window_rect_in_bound(bound, hwnd) {
        (rc.left, rc.top)
    } else {
        (0, 0)
    }
}

fn is_non_interactive_static(hwnd: HWND) -> bool {
    let mut buf = [0u16; 32];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    if len == 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&buf[..len]);
    if name.eq_ignore_ascii_case("Static") {
        let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        return style & SS_NOTIFY == 0;
    }
    false
}

fn is_on_game_thread(hwnd: HWND) -> bool {
    let tid = bound_thread_id();
    if tid == 0 || hwnd.is_invalid() {
        return false;
    }
    unsafe { GetWindowThreadProcessId(hwnd, None) == tid }
}

pub(crate) fn is_same_process_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

pub(crate) fn is_related_dialog_window(bound: HWND, hwnd: HWND) -> bool {
    if hwnd == bound || hwnd.is_invalid() || is_present_hwnd(hwnd) {
        return false;
    }
    if is_on_game_thread(hwnd) {
        return true;
    }
    if unsafe { IsChild(bound, hwnd) }.as_bool() {
        return true;
    }
    let mut cur = hwnd;
    for _ in 0..8 {
        if let Ok(owner) = unsafe { GetWindow(cur, GW_OWNER) } {
            if owner == bound {
                return true;
            }
            if !owner.is_invalid() && owner != cur {
                cur = owner;
                continue;
            }
        }
        if let Ok(parent) = unsafe { GetParent(cur) } {
            if parent == bound {
                return true;
            }
            if !parent.is_invalid() && parent != cur {
                cur = parent;
                continue;
            }
        }
        break;
    }
    false
}

fn hit_test_tree(bound: HWND, root: HWND, lx: i32, ly: i32) -> Option<HWND> {
    if !unsafe { IsWindowVisible(root) }.as_bool() {
        return None;
    }
    let style = unsafe { GetWindowLongW(root, GWL_STYLE) } as u32;
    if style & WS_DISABLED.0 != 0 {
        return None;
    }
    let rc = window_rect_in_bound(bound, root)?;
    if lx < rc.left || lx >= rc.right || ly < rc.top || ly >= rc.bottom {
        return None;
    }
    // Descend top-to-bottom through child controls first so controls win over dialog background.
    let mut child = unsafe { GetWindow(root, GW_CHILD) }.unwrap_or_default();
    while !child.is_invalid() {
        if let Some(hit) = hit_test_tree(bound, child, lx, ly) {
            return Some(hit);
        }
        child = unsafe { GetWindow(child, GW_HWNDNEXT) }.unwrap_or_default();
    }
    if is_non_interactive_static(root) {
        return None;
    }
    Some(root)
}

unsafe extern "system" fn collect_thread_popups_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(lparam.0 as *mut (HWND, Vec<HWND>)) };
    let bound = ctx.0;
    if hwnd != bound && !is_present_hwnd(hwnd) && unsafe { IsWindowVisible(hwnd) }.as_bool() {
        ctx.1.push(hwnd);
    }
    BOOL(1)
}

pub(crate) fn hit_test_logical_child(bound: HWND, lx: i32, ly: i32) -> Option<HWND> {
    let tid = bound_thread_id();
    if tid != 0 {
        let mut popups = (bound, Vec::<HWND>::new());
        unsafe {
            let _ = EnumThreadWindows(
                tid,
                Some(collect_thread_popups_proc),
                LPARAM((&raw mut popups) as isize),
            );
        }
        for dlg in popups.1 {
            if let Some(hit) = hit_test_tree(bound, dlg, lx, ly) {
                return Some(hit);
            }
        }
    }
    let mut child = unsafe { GetWindow(bound, GW_CHILD) }.unwrap_or_default();
    while !child.is_invalid() {
        if let Some(hit) = hit_test_tree(bound, child, lx, ly) {
            return Some(hit);
        }
        child = unsafe { GetWindow(child, GW_HWNDNEXT) }.unwrap_or_default();
    }
    None
}

pub(crate) fn is_mouse_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_MBUTTONDBLCLK
            | WM_MOUSEWHEEL
    )
}

/// Window procedure for the owned wgpu presentation overlay window.
/// Routes mouse input through the viewport to the matching game dialog control (or main HWND)
/// without stealing window focus.
pub(crate) unsafe extern "system" fn present_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_NCHITTEST => {
            return if logical_mode().is_some() {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            };
        }
        WM_SETCURSOR => {
            let bound_raw = bound_hwnd_raw();
            if bound_raw != 0 {
                let bound = HWND(bound_raw as *mut c_void);
                return unsafe {
                    SendMessageW(
                        bound,
                        WM_SETCURSOR,
                        Some(WPARAM(bound.0 as usize)),
                        Some(lparam),
                    )
                };
            }
        }
        _ if is_mouse_message(msg) => {
            if let Some((bound, w, h, _)) = logical_mode() {
                if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
                    keep_present_on_top(bound, hwnd);
                }
                let raw = lparam.0 as u32;
                let mut px = (raw & 0xffff) as i16 as i32;
                let mut py = ((raw >> 16) & 0xffff) as i16 as i32;
                if msg == WM_MOUSEWHEEL {
                    let mut pt = POINT { x: px, y: py };
                    if unsafe { ScreenToClient(hwnd, &mut pt) }.as_bool() {
                        px = pt.x;
                        py = pt.y;
                    }
                }
                // Map from the Per-Monitor V2 overlay's physical client coords to logical coords,
                // then hit-test and compute child-relative offsets in `bound`'s 96-DPI context.
                let (lx, ly) = client_to_logical(hwnd, px, py, w, h);
                let (target, target_lparam) = with_window_dpi(bound, || {
                    let target = hit_test_logical_child(bound, lx, ly).unwrap_or(bound);
                    let target_lparam = if msg == WM_MOUSEWHEEL || target == bound {
                        LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize)
                    } else {
                        let (tx, ty) = client_origin_in_bound(bound, target);
                        let cx = lx - tx;
                        let cy = ly - ty;
                        LPARAM(((cy as u16 as u32) << 16 | (cx as u16 as u32)) as isize)
                    };
                    (target, target_lparam)
                });
                if unsafe { IsWindowUnicode(target) }.as_bool() {
                    let _ = unsafe { PostMessageW(Some(target), msg, wparam, target_lparam) };
                } else {
                    let _ = unsafe { PostMessageA(Some(target), msg, wparam, target_lparam) };
                }
                return LRESULT(0);
            }
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
