//! Pure Rust decision logic for Win32 API hooks, `WH_GETMESSAGE`, and host WndProc subclassing.
#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BITSPIXEL, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_TOP, HWND_TOPMOST, MSG, SET_WINDOW_POS_FLAGS, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WM_ACTIVATE, WM_ACTIVATEAPP, WM_CANCELMODE,
    WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEWHEEL,
    WM_MOVE, WM_MOVING, WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDOWN,
    WM_NCLBUTTONUP, WM_NCPAINT, WM_RBUTTONDOWN, WM_SIZE, WM_SIZING, WM_SYSCHAR, WM_SYSCOMMAND,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_WINDOWPOSCHANGED, WS_CHILD,
};

use crate::config;
use crate::win32::geometry::{
    client_to_screen, clip_cursor, cursor_pos, get_capture, map_rect, screen_to_client,
    set_cursor_pos, window_from_point, window_rect,
};
use crate::win32::hwnd::{
    bring_window_to_top, desktop_window, enum_thread_windows, foreground_window, is_child,
    is_iconic, is_visible, kill_timer, move_window, post_message, set_foreground_window,
    set_window_pos, style,
};

use super::input::{
    client_origin_in_bound, client_to_logical, hit_test_logical_child, is_mouse_message,
    is_native_modal_dialog_target, is_related_dialog_window, is_same_process_window,
    logical_to_client,
};
use super::overlay::ensure_present_on_top_if_active;
use super::{
    bound_hwnd, bound_thread_id, is_in_size_move, is_present_hwnd, is_scaling_active, logical_mode,
    present_hwnd, set_in_size_move, FLUSH_TIMER_ID, WM_APP_FLUSH,
};

const VK_TAB_CODE: usize = 0x09;
const VK_MENU_CODE: usize = 0x12;
const VK_F4_CODE: usize = 0x73;

static LAST_BOUND_ORIGIN_VALID: AtomicBool = AtomicBool::new(false);
static LAST_BOUND_ORIGIN_X: AtomicI32 = AtomicI32::new(0);
static LAST_BOUND_ORIGIN_Y: AtomicI32 = AtomicI32::new(0);

pub(super) fn reset_tracking() {
    LAST_BOUND_ORIGIN_VALID.store(false, Ordering::Relaxed);
}

fn is_game_foreground(bound: HWND) -> bool {
    let fg = foreground_window();
    !fg.is_invalid()
        && (fg == bound || is_related_dialog_window(bound, fg) || is_same_process_window(fg))
}

fn is_non_client_or_frame_message(msg: u32) -> bool {
    matches!(
        msg,
        WM_NCHITTEST | 0x00A0
            ..=0x00AF | WM_SYSCOMMAND | WM_SIZING | WM_MOVING | WM_ENTERSIZEMOVE | WM_EXITSIZEMOVE
    )
}

fn is_top_level_game_popup(bound: HWND, hwnd: HWND) -> bool {
    !is_present_hwnd(hwnd)
        && is_related_dialog_window(bound, hwnd)
        && !is_child(bound, hwnd)
        && (style(hwnd) as u32) & WS_CHILD.0 == 0
}

fn bound_client_origin(bound: HWND) -> Option<POINT> {
    let mut pt = POINT { x: 0, y: 0 };
    client_to_screen(bound, &mut pt).then_some(pt)
}

fn sync_popups_after_bound_move(bound: HWND) {
    if is_iconic(bound) {
        return;
    }
    let Some(origin) = bound_client_origin(bound) else {
        return;
    };
    let had_prev = LAST_BOUND_ORIGIN_VALID.swap(true, Ordering::Relaxed);
    let prev_x = LAST_BOUND_ORIGIN_X.swap(origin.x, Ordering::Relaxed);
    let prev_y = LAST_BOUND_ORIGIN_Y.swap(origin.y, Ordering::Relaxed);
    if !had_prev {
        return;
    }
    let dx = origin.x - prev_x;
    let dy = origin.y - prev_y;
    if dx == 0 && dy == 0 {
        return;
    }
    let tid = bound_thread_id();
    if tid == 0 {
        return;
    }
    let mut popups = Vec::new();
    enum_thread_windows(tid, |hwnd| {
        if hwnd != bound && is_visible(hwnd) && is_top_level_game_popup(bound, hwnd) {
            popups.push(hwnd);
        }
        true
    });
    for dlg in popups {
        if let Ok(rc) = window_rect(dlg) {
            let _ = set_window_pos(
                dlg,
                None,
                rc.left + dx,
                rc.top + dy,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER,
            );
        }
    }
}

pub(super) fn virtual_window_rect(hwnd: HWND, include_dialogs: bool) -> Option<RECT> {
    let (bound, w, h, _) = logical_mode()?;
    if hwnd == bound || hwnd == desktop_window() {
        return Some(RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        });
    }
    if include_dialogs && is_related_dialog_window(bound, hwnd) {
        return Some(map_rect(None, Some(bound), window_rect(hwnd).ok()?));
    }
    None
}

/// Maps a `CB_GETDROPPEDCONTROLRECT` screen-space rectangle into `bound`'s virtual client space,
/// matching `virtual_window_rect` so dialog scalers that subtract `GetWindowRect(parent)` from
/// `SendMessage(combo, CB_GETDROPPEDCONTROLRECT, ...)` do not shift ComboBoxes by the host
/// window's desktop position in windowed mode.
pub(super) fn map_dropped_control_rect(hwnd: HWND, rect: &mut RECT) {
    let Some((bound, _, _, _)) = logical_mode() else {
        return;
    };
    map_dropped_control_rect_for_bound(bound, hwnd, rect);
}

pub(super) fn map_dropped_control_rect_for_bound(bound: HWND, hwnd: HWND, rect: &mut RECT) {
    if hwnd == bound || is_related_dialog_window(bound, hwnd) {
        *rect = map_rect(None, Some(bound), *rect);
    }
}

pub(super) fn virtual_system_metrics(index: i32) -> Option<i32> {
    let (_, w, h, _) = logical_mode()?;
    if index == SM_CXSCREEN.0 {
        return Some(w as i32);
    }
    if index == SM_CYSCREEN.0 {
        return Some(h as i32);
    }
    None
}

pub(super) fn virtual_client_to_screen(hwnd: HWND, pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    if hwnd == bound {
        return Some(true);
    }
    if is_related_dialog_window(bound, hwnd) {
        return Some(client_to_screen(hwnd, pt) && screen_to_client(bound, pt));
    }
    None
}

pub(super) fn virtual_screen_to_client(hwnd: HWND, pt: &mut POINT) -> Option<bool> {
    let (bound, _, _, _) = logical_mode()?;
    if hwnd == bound {
        return Some(true);
    }
    if is_related_dialog_window(bound, hwnd) {
        return Some(client_to_screen(bound, pt) && screen_to_client(hwnd, pt));
    }
    None
}

pub(super) fn virtual_cursor_pos() -> Option<POINT> {
    let mut raw = cursor_pos().ok()?;
    if let Some((bound, w, h, _)) = logical_mode() {
        if screen_to_client(bound, &mut raw) {
            let (lx, ly) = client_to_logical(bound, raw.x, raw.y, w, h);
            raw = POINT { x: lx, y: ly };
        }
    }
    Some(raw)
}

pub(super) fn set_virtual_cursor_pos(x: i32, y: i32) -> bool {
    if let Some((bound, w, h, _)) = logical_mode() {
        // Never yank the user's cursor while another application is foreground or while
        // the user is dragging/resizing the game window.
        if !is_game_foreground(bound) || is_in_size_move() {
            return true;
        }
        let (cx, cy) = logical_to_client(bound, x, y, w, h);
        let mut pt = POINT { x: cx, y: cy };
        if client_to_screen(bound, &mut pt) {
            return set_cursor_pos(pt.x, pt.y);
        }
    }
    set_cursor_pos(x, y)
}

pub(super) fn clip_virtual_cursor(rect: Option<&RECT>) -> bool {
    if let (Some((bound, w, h, _)), Some(r)) = (logical_mode(), rect) {
        if !is_game_foreground(bound) || is_in_size_move() {
            return true;
        }
        let (l, t) = logical_to_client(bound, r.left, r.top, w, h);
        let (ri, b) = logical_to_client(
            bound,
            r.right.saturating_sub(1),
            r.bottom.saturating_sub(1),
            w,
            h,
        );
        let mapped = map_rect(
            Some(bound),
            None,
            RECT {
                left: l,
                top: t,
                right: ri + 1,
                bottom: b + 1,
            },
        );
        return clip_cursor(Some(&mapped));
    }
    clip_cursor(rect)
}

pub(super) fn virtual_clip_cursor() -> Option<RECT> {
    let (_, w, h, _) = logical_mode()?;
    Some(RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    })
}

pub(super) fn virtual_window_from_point(point: POINT) -> HWND {
    if let Some((bound, _, _, _)) = logical_mode() {
        return hit_test_logical_child(bound, point.x, point.y).unwrap_or(bound);
    }
    window_from_point(point)
}

pub(super) fn handle_set_window_pos(
    hwnd: HWND,
    mut insert_after: HWND,
    mut x: i32,
    mut y: i32,
    cx: i32,
    cy: i32,
    flags: u32,
) -> bool {
    if is_scaling_active() && bound_hwnd() == Some(hwnd) {
        return true;
    }
    let mode = logical_mode();
    if let Some((bound, _, _, _)) = mode {
        if insert_after == HWND_TOPMOST && (hwnd == bound || is_related_dialog_window(bound, hwnd))
        {
            insert_after = HWND_TOP;
        }
        if flags & SWP_NOMOVE.0 == 0 && is_top_level_game_popup(bound, hwnd) {
            if let Some(origin) = bound_client_origin(bound) {
                x += origin.x;
                y += origin.y;
            }
        }
    }
    let ok = set_window_pos(
        hwnd,
        Some(insert_after),
        x,
        y,
        cx,
        cy,
        SET_WINDOW_POS_FLAGS(flags),
    )
    .is_ok();
    if let Some((bound, _, _, _)) = mode {
        if !is_present_hwnd(hwnd) && is_related_dialog_window(bound, hwnd) {
            ensure_present_on_top_if_active(bound);
        }
    }
    ok
}

pub(super) fn handle_bring_window_to_top(hwnd: HWND) -> bool {
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_game_foreground(bound) && (hwnd == bound || is_related_dialog_window(bound, hwnd)) {
            return true;
        }
    }
    let ok = bring_window_to_top(hwnd);
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && (hwnd == bound || is_related_dialog_window(bound, hwnd)) {
            ensure_present_on_top_if_active(bound);
        }
    }
    ok
}

pub(super) fn handle_set_foreground_window(hwnd: HWND) -> bool {
    if let Some((bound, _, _, _)) = logical_mode() {
        // When the user Alt+Tabs away, games shielded by `noactivateapp` still believe they own
        // focus and may call `SetForegroundWindow` on menu transitions; never steal focus back.
        if !is_game_foreground(bound) && (hwnd == bound || is_related_dialog_window(bound, hwnd)) {
            return true;
        }
    }
    set_foreground_window(hwnd)
}

pub(super) fn handle_move_window(
    hwnd: HWND,
    mut x: i32,
    mut y: i32,
    width: i32,
    height: i32,
    repaint: bool,
) -> bool {
    if is_scaling_active() && bound_hwnd() == Some(hwnd) {
        return true;
    }
    if let Some((bound, _, _, _)) = logical_mode() {
        if is_top_level_game_popup(bound, hwnd) {
            if let Some(origin) = bound_client_origin(bound) {
                x += origin.x;
                y += origin.y;
            }
        }
    }
    move_window(hwnd, x, y, width, height, repaint)
}

pub(super) fn virtual_device_caps(index: i32) -> Option<i32> {
    if bound_hwnd().is_some() && (index == LOGPIXELSX.0 as i32 || index == LOGPIXELSY.0 as i32) {
        return Some(96);
    }
    let (_, w, h, bpp) = logical_mode()?;
    if index == BITSPIXEL.0 as i32 && bpp > 0 {
        return Some(bpp as i32);
    }
    if index == HORZRES.0 as i32 {
        return Some(w as i32);
    }
    if index == VERTRES.0 as i32 {
        return Some(h as i32);
    }
    None
}

pub(super) fn rewrite_getmsg(msg: &mut MSG) {
    // Never rewrite `msg.pt` or `msg.lParam` during Win32's modal move/resize loop or for
    // non-client chrome messages; `DefWindowProc`'s SC_MOVE/SC_SIZE loop reads physical screen
    // coordinates from `msg.pt`, and clobbering them makes dragged windows teleport wildly.
    if is_in_size_move() || is_non_client_or_frame_message(msg.message) {
        return;
    }
    let Some((bound, w, h, _)) = logical_mode() else {
        return;
    };
    let is_bound = msg.hwnd == bound;
    let is_dialog = is_related_dialog_window(bound, msg.hwnd);
    // Native modal controls already receive native client coordinates from Windows.
    if !is_bound && (!is_dialog || is_native_modal_dialog_target(bound, msg.hwnd)) {
        return;
    }
    let Ok(mut cursor) = cursor_pos() else {
        return;
    };
    if !screen_to_client(bound, &mut cursor) {
        return;
    }
    let (lx, ly) = client_to_logical(bound, cursor.x, cursor.y, w, h);
    msg.pt = POINT { x: lx, y: ly };
    if !is_mouse_message(msg.message) {
        return;
    }
    if matches!(
        msg.message,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
    ) {
        ensure_present_on_top_if_active(bound);
    }
    let captured = get_capture();
    let target = if captured == msg.hwnd || is_bound {
        msg.hwnd
    } else {
        hit_test_logical_child(bound, lx, ly).unwrap_or(bound)
    };
    msg.hwnd = target;
    if msg.message == WM_MOUSEWHEEL || target == bound {
        msg.lParam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
    } else {
        let (tx, ty) = client_origin_in_bound(bound, target);
        let cx = lx - tx;
        let cy = ly - ty;
        msg.lParam = LPARAM(((cy as u16 as u32) << 16 | (cx as u16 as u32)) as isize);
    }
}

pub(super) enum SubclassAction {
    Return(LRESULT),
    DefWindowProc,
    Forward(LPARAM),
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_subclass_message(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    mut lparam: LPARAM,
    activate_passed: &AtomicBool,
    activateapp_passed: &AtomicBool,
    alt_was_down: &AtomicBool,
) -> SubclassAction {
    if msg == WM_APP_FLUSH {
        if let Some(cb) = super::host_callbacks() {
            cb.flush_pending_present(false);
        }
        return SubclassAction::Return(LRESULT(0));
    }
    if msg == WM_TIMER && wparam.0 == FLUSH_TIMER_ID {
        kill_timer(hwnd, FLUSH_TIMER_ID);
        if let Some(cb) = super::host_callbacks() {
            cb.flush_pending_present(true);
        }
        return SubclassAction::Return(LRESULT(0));
    }
    let is_bound = bound_hwnd() == Some(hwnd);
    if is_scaling_active() && is_bound {
        match msg {
            WM_ENTERSIZEMOVE => {
                set_in_size_move(true);
                let _ = clip_cursor(None);
                return SubclassAction::DefWindowProc;
            }
            WM_EXITSIZEMOVE => {
                set_in_size_move(false);
                sync_popups_after_bound_move(hwnd);
                if let Some(cb) = super::host_callbacks() {
                    cb.on_host_window_changed();
                }
                return SubclassAction::DefWindowProc;
            }
            WM_CANCELMODE => {
                set_in_size_move(false);
                return SubclassAction::DefWindowProc;
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONUP | WM_NCPAINT | WM_NCCALCSIZE => {
                return SubclassAction::DefWindowProc;
            }
            _ => {}
        }
    }
    let toggle_vk = config::get().toggle_fullscreen_vk;
    if msg == WM_SYSKEYDOWN {
        let alt_down = (lparam.0 as u32) & (1 << 29) != 0;
        let was_down = (lparam.0 as u32) & (1 << 30) != 0;
        if wparam.0 == VK_MENU_CODE || alt_down {
            alt_was_down.store(true, Ordering::Relaxed);
        }
        if wparam.0 == VK_F4_CODE {
            return SubclassAction::DefWindowProc;
        }
        if toggle_vk != 0 && wparam.0 as u32 == toggle_vk && alt_down {
            if !was_down {
                if let Some(cb) = super::host_callbacks() {
                    cb.toggle_fullscreen_hotkey();
                }
            }
            return SubclassAction::Return(LRESULT(0));
        }
    }
    if msg == WM_SYSKEYUP {
        if wparam.0 == VK_MENU_CODE {
            alt_was_down.store(false, Ordering::Relaxed);
        }
        if wparam.0 == VK_TAB_CODE || (toggle_vk != 0 && wparam.0 as u32 == toggle_vk) {
            return SubclassAction::DefWindowProc;
        }
    }
    if msg == WM_SYSCHAR && toggle_vk != 0 {
        let alt_down = (lparam.0 as u32) & (1 << 29) != 0;
        if wparam.0 as u32 == toggle_vk && alt_down {
            // Suppress default Windows Alt+Enter system beep.
            return SubclassAction::Return(LRESULT(0));
        }
    }
    let is_fullscreen = super::host_callbacks().is_some_and(|cb| cb.is_fullscreen_active());
    let shield_focus = is_scaling_active() && (config::get().noactivateapp || !is_fullscreen);
    if shield_focus && matches!(msg, WM_KILLFOCUS | WM_NCACTIVATE) {
        return SubclassAction::DefWindowProc;
    }
    if msg == WM_ACTIVATE && shield_focus {
        let active = (wparam.0 & 0xffff) != 0;
        if !activate_passed.swap(true, Ordering::Relaxed) && active {
            // Allow the initial activation through once so the game initializes its focus flag.
        } else {
            return SubclassAction::DefWindowProc;
        }
    }
    if msg == WM_ACTIVATEAPP {
        if wparam.0 == 0 {
            set_in_size_move(false);
            let _ = clip_cursor(None);
        } else {
            if let Some(cb) = super::host_callbacks() {
                cb.on_host_window_changed();
            }
            if shield_focus && alt_was_down.swap(false, Ordering::Relaxed) {
                let _ = post_message(hwnd, WM_SYSKEYUP, WPARAM(VK_MENU_CODE), LPARAM(0));
            }
        }
        if shield_focus {
            if !activateapp_passed.swap(true, Ordering::Relaxed) && wparam.0 != 0 {
                // Allow initial activation once.
            } else {
                return SubclassAction::DefWindowProc;
            }
        }
    }
    if matches!(msg, WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGED) && is_bound {
        sync_popups_after_bound_move(hwnd);
        if let Some(cb) = super::host_callbacks() {
            cb.on_host_window_changed();
        }
    }
    // Only remap physical coordinates on `bound` when no owned presentation overlay exists;
    // when `PRESENT_HWND` is active, `present_wndproc` and `getmsg_hook` deliver logical coords.
    if is_mouse_message(msg) && !is_in_size_move() && present_hwnd().is_none() {
        if let Some((bound, w, h, _)) = logical_mode() {
            if hwnd == bound {
                let raw = lparam.0 as u32;
                let mut px = (raw & 0xffff) as i16 as i32;
                let mut py = ((raw >> 16) & 0xffff) as i16 as i32;
                if msg == WM_MOUSEWHEEL {
                    let mut pt = POINT { x: px, y: py };
                    if screen_to_client(bound, &mut pt) {
                        px = pt.x;
                        py = pt.y;
                    }
                }
                let (lx, ly) = client_to_logical(bound, px, py, w, h);
                lparam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
            }
        }
    }
    SubclassAction::Forward(lparam)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE;

    #[test]
    fn non_client_and_size_move_messages_preserve_screen_coordinates() {
        let mut nc_msg = MSG {
            hwnd: HWND(0x1234 as *mut c_void),
            message: WM_NCLBUTTONDOWN,
            pt: POINT { x: 1800, y: 420 },
            lParam: LPARAM((420 << 16) | 1800),
            ..Default::default()
        };
        rewrite_getmsg(&mut nc_msg);
        assert_eq!(nc_msg.pt, POINT { x: 1800, y: 420 });
        assert_eq!(nc_msg.lParam, LPARAM((420 << 16) | 1800));

        set_in_size_move(true);
        let mut drag_msg = MSG {
            hwnd: HWND(0x1234 as *mut c_void),
            message: WM_MOUSEMOVE,
            pt: POINT { x: 2100, y: 640 },
            lParam: LPARAM((640 << 16) | 2100),
            ..Default::default()
        };
        rewrite_getmsg(&mut drag_msg);
        set_in_size_move(false);
        assert_eq!(drag_msg.pt, POINT { x: 2100, y: 640 });
        assert_eq!(drag_msg.lParam, LPARAM((640 << 16) | 2100));
    }
}
