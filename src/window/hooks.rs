//! Thin Win32 hooking layer: PE32 IAT patching, host WndProc subclassing, and WH_GETMESSAGE.
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{BOOL, PCSTR};
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetDC, GetDeviceCaps, MapWindowPoints, ReleaseDC, ScreenToClient, BITSPIXEL,
    GET_DEVICE_CAPS_INDEX, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY, VERTRES,
};
use windows::Win32::System::LibraryLoader::{
    FindResourceA, GetModuleHandleW, LoadResource, LockResource, SizeofResource,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::UI::Input::KeyboardAndMouse::GetCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, CallWindowProcA, CallWindowProcW, ClipCursor,
    CreateDialogIndirectParamA, CreateDialogParamA, DefWindowProcA, DefWindowProcW,
    DialogBoxIndirectParamA, DialogBoxParamA, GetClientRect, GetClipCursor, GetCursorPos,
    GetDesktopWindow, GetSystemMetrics, GetWindowLongA, GetWindowLongW, GetWindowRect, IsWindow,
    IsWindowUnicode, KillTimer, MoveWindow, SetCursorPos, SetWindowLongA, SetWindowLongW,
    SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, DLGPROC, DLGTEMPLATE,
    GWLP_WNDPROC, HHOOK, MSG, RT_DIALOG, SET_WINDOW_POS_FLAGS, SM_CXSCREEN, SM_CYSCREEN,
    SYSTEM_METRICS_INDEX, WH_GETMESSAGE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_MOVE, WM_NCACTIVATE, WM_RBUTTONDOWN, WM_SIZE, WM_SYSCHAR,
    WM_SYSKEYDOWN, WM_TIMER, WM_WINDOWPOSCHANGED, WNDPROC,
};

use crate::config;

use super::input::{
    client_origin_in_bound, client_to_logical, hit_test_logical_child, is_mouse_message,
    is_related_dialog_window, logical_to_client,
};
use super::overlay::ensure_present_on_top_if_active;
use super::{
    bound_hwnd_raw, is_present_hwnd, is_scaling_active, logical_mode, present_hwnd_raw,
    FLUSH_TIMER_ID, WM_APP_FLUSH,
};

static ORIG_WNDPROC: AtomicIsize = AtomicIsize::new(0);
static SUBCLASSED_HWND: AtomicIsize = AtomicIsize::new(0);
static SUBCLASSED_UNICODE: AtomicBool = AtomicBool::new(false);
static GETMSG_HOOK: AtomicIsize = AtomicIsize::new(0);
static ACTIVATE_PASSED: AtomicBool = AtomicBool::new(false);
static ACTIVATEAPP_PASSED: AtomicBool = AtomicBool::new(false);

struct PatchedSlot {
    slot: usize,
    original: usize,
}

static IAT_PATCHES: Mutex<Vec<PatchedSlot>> = Mutex::new(Vec::new());

pub(crate) fn subclassed_hwnd_raw() -> isize {
    SUBCLASSED_HWND.load(Ordering::Relaxed)
}

unsafe extern "system" fn hook_GetClientRect(hwnd: HWND, rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some((bound, w, h, _)) = logical_mode() {
            if hwnd == bound || hwnd == unsafe { GetDesktopWindow() } {
                unsafe {
                    *rect = RECT {
                        left: 0,
                        top: 0,
                        right: w as i32,
                        bottom: h as i32,
                    };
                }
                return BOOL(1);
            }
        }
    }
    BOOL::from(unsafe { GetClientRect(hwnd, rect) }.is_ok())
}

unsafe extern "system" fn hook_GetWindowRect(hwnd: HWND, rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some((bound, w, h, _)) = logical_mode() {
            if hwnd == bound || hwnd == unsafe { GetDesktopWindow() } {
                unsafe {
                    *rect = RECT {
                        left: 0,
                        top: 0,
                        right: w as i32,
                        bottom: h as i32,
                    };
                }
                return BOOL(1);
            }
            if is_related_dialog_window(bound, hwnd) && unsafe { GetWindowRect(hwnd, rect) }.is_ok()
            {
                let pts = unsafe { std::slice::from_raw_parts_mut(rect.cast::<POINT>(), 2) };
                unsafe {
                    MapWindowPoints(None, Some(bound), pts);
                }
                return BOOL(1);
            }
        }
    }
    BOOL::from(unsafe { GetWindowRect(hwnd, rect) }.is_ok())
}

unsafe extern "system" fn hook_GetSystemMetrics(index: i32) -> i32 {
    if let Some((_, w, h, _)) = logical_mode() {
        if index == SM_CXSCREEN.0 {
            return w as i32;
        }
        if index == SM_CYSCREEN.0 {
            return h as i32;
        }
    }
    unsafe { GetSystemMetrics(SYSTEM_METRICS_INDEX(index)) }
}

unsafe extern "system" fn hook_ClientToScreen(hwnd: HWND, pt: *mut POINT) -> BOOL {
    if !pt.is_null() {
        if let Some((bound, _, _, _)) = logical_mode() {
            if hwnd == bound {
                return BOOL(1);
            }
            if is_related_dialog_window(bound, hwnd) {
                let ok = unsafe { ClientToScreen(hwnd, pt) }.as_bool()
                    && unsafe { ScreenToClient(bound, pt) }.as_bool();
                return BOOL::from(ok);
            }
        }
    }
    unsafe { ClientToScreen(hwnd, pt) }
}

unsafe extern "system" fn hook_ScreenToClient(hwnd: HWND, pt: *mut POINT) -> BOOL {
    if !pt.is_null() {
        if let Some((bound, _, _, _)) = logical_mode() {
            if hwnd == bound {
                return BOOL(1);
            }
            if is_related_dialog_window(bound, hwnd) {
                let ok = unsafe { ClientToScreen(bound, pt) }.as_bool()
                    && unsafe { ScreenToClient(hwnd, pt) }.as_bool();
                return BOOL::from(ok);
            }
        }
    }
    unsafe { ScreenToClient(hwnd, pt) }
}

unsafe extern "system" fn hook_GetCursorPos(pt: *mut POINT) -> BOOL {
    if pt.is_null() {
        return BOOL(0);
    }
    let mut raw = POINT::default();
    if unsafe { GetCursorPos(&mut raw) }.is_err() {
        return BOOL(0);
    }
    if let Some((bound, w, h, _)) = logical_mode() {
        if unsafe { ScreenToClient(bound, &mut raw) }.as_bool() {
            let (lx, ly) = client_to_logical(bound, raw.x, raw.y, w, h);
            unsafe {
                *pt = POINT { x: lx, y: ly };
            }
            return BOOL(1);
        }
    }
    unsafe { *pt = raw };
    BOOL(1)
}

unsafe extern "system" fn hook_SetCursorPos(x: i32, y: i32) -> BOOL {
    if let Some((bound, w, h, _)) = logical_mode() {
        let (cx, cy) = logical_to_client(bound, x, y, w, h);
        let mut pt = POINT { x: cx, y: cy };
        if unsafe { ClientToScreen(bound, &mut pt) }.as_bool() {
            return BOOL::from(unsafe { SetCursorPos(pt.x, pt.y) }.is_ok());
        }
    }
    BOOL::from(unsafe { SetCursorPos(x, y) }.is_ok())
}

unsafe extern "system" fn hook_ClipCursor(rect: *const RECT) -> BOOL {
    if let Some((bound, w, h, _)) = logical_mode() {
        if let Some(r) = unsafe { rect.as_ref() } {
            let (l, t) = logical_to_client(bound, r.left, r.top, w, h);
            let (ri, b) = logical_to_client(
                bound,
                r.right.saturating_sub(1),
                r.bottom.saturating_sub(1),
                w,
                h,
            );
            let mut pts = [
                POINT { x: l, y: t },
                POINT {
                    x: ri + 1,
                    y: b + 1,
                },
            ];
            unsafe {
                MapWindowPoints(Some(bound), None, &mut pts);
            }
            let mapped = RECT {
                left: pts[0].x,
                top: pts[0].y,
                right: pts[1].x,
                bottom: pts[1].y,
            };
            return BOOL::from(unsafe { ClipCursor(Some(&mapped)) }.is_ok());
        }
    }
    let opt = if rect.is_null() { None } else { Some(rect) };
    BOOL::from(unsafe { ClipCursor(opt) }.is_ok())
}

unsafe extern "system" fn hook_GetClipCursor(rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some((_, w, h, _)) = logical_mode() {
            unsafe {
                *rect = RECT {
                    left: 0,
                    top: 0,
                    right: w as i32,
                    bottom: h as i32,
                };
            }
            return BOOL(1);
        }
    }
    BOOL::from(unsafe { GetClipCursor(rect) }.is_ok())
}

unsafe extern "system" fn hook_WindowFromPoint(point: POINT) -> HWND {
    if let Some((bound, _, _, _)) = logical_mode() {
        if let Some(hit) = hit_test_logical_child(bound, point.x, point.y) {
            return hit;
        }
        return bound;
    }
    unsafe { WindowFromPoint(point) }
}

unsafe extern "system" fn hook_SetWindowPos(
    hwnd: HWND,
    insert_after: HWND,
    x: i32,
    y: i32,
    cx: i32,
    cy: i32,
    flags: u32,
) -> BOOL {
    if is_scaling_active() && hwnd.0 as isize == bound_hwnd_raw() {
        return BOOL(1);
    }
    let ok = unsafe {
        SetWindowPos(
            hwnd,
            Some(insert_after),
            x,
            y,
            cx,
            cy,
            SET_WINDOW_POS_FLAGS(flags),
        )
    }
    .is_ok();
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && is_related_dialog_window(bound, hwnd) {
            ensure_present_on_top_if_active(bound);
        }
    }
    BOOL::from(ok)
}

unsafe extern "system" fn hook_BringWindowToTop(hwnd: HWND) -> BOOL {
    let ok = unsafe { BringWindowToTop(hwnd) }.is_ok();
    if let Some((bound, _, _, _)) = logical_mode() {
        if !is_present_hwnd(hwnd) && (hwnd == bound || is_related_dialog_window(bound, hwnd)) {
            ensure_present_on_top_if_active(bound);
        }
    }
    BOOL::from(ok)
}

unsafe extern "system" fn hook_MoveWindow(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    repaint: BOOL,
) -> BOOL {
    if is_scaling_active() && hwnd.0 as isize == bound_hwnd_raw() {
        return BOOL(1);
    }
    BOOL::from(unsafe { MoveWindow(hwnd, x, y, width, height, repaint.as_bool()) }.is_ok())
}

unsafe extern "system" fn hook_GetDeviceCaps(hdc: HDC, index: i32) -> i32 {
    if bound_hwnd_raw() != 0 && (index == LOGPIXELSX.0 as i32 || index == LOGPIXELSY.0 as i32) {
        return 96;
    }
    if let Some((_, w, h, bpp)) = logical_mode() {
        if index == BITSPIXEL.0 as i32 && bpp > 0 {
            return bpp as i32;
        }
        if index == HORZRES.0 as i32 {
            return w as i32;
        }
        if index == VERTRES.0 as i32 {
            return h as i32;
        }
    }
    unsafe { GetDeviceCaps(Some(hdc), GET_DEVICE_CAPS_INDEX(index as u32)) }
}

const DS_SETFONT: u32 = 0x40;

fn system_dpi() -> u32 {
    unsafe {
        let dc = GetDC(None);
        let dpi = if !dc.is_invalid() {
            let v = GetDeviceCaps(Some(dc), LOGPIXELSY);
            let _ = ReleaseDC(None, dc);
            v
        } else {
            96
        };
        if dpi > 0 {
            dpi as u32
        } else {
            96
        }
    }
}

fn scale_dialog_point_size(pt: u16, dpi: u32) -> u16 {
    if dpi <= 96 || pt == 0 {
        return pt;
    }
    ((pt as u32 * 96) / dpi).max(1) as u16
}

fn read_u16_at(ptr: *const u8, off: usize) -> u16 {
    unsafe { ptr.add(off).cast::<u16>().read_unaligned() }
}

fn read_u32_at(ptr: *const u8, off: usize) -> u32 {
    unsafe { ptr.add(off).cast::<u32>().read_unaligned() }
}

fn skip_sz_or_ord(ptr: *const u8, mut off: usize, allow_ordinal: bool) -> usize {
    let first = read_u16_at(ptr, off);
    if first == 0 {
        off + 2
    } else if allow_ordinal && first == 0xFFFF {
        off + 4
    } else {
        off += 2;
        while read_u16_at(ptr, off) != 0 {
            off += 2;
        }
        off + 2
    }
}

fn align4(off: usize) -> usize {
    (off + 3) & !3
}

/// Computes the byte length and `pointsize` field offset of a Win32 `DLGTEMPLATE` or
/// `DLGTEMPLATEEX` so dialog templates built for 96 DPI keep their exact 96-DPI pixel layout
/// even when the process is DPI-aware on a high-DPI monitor (e.g. 240 DPI / 250%).
unsafe fn measure_dialog_template(tpl: *const u8) -> Option<(usize, Option<usize>)> {
    if tpl.is_null() {
        return None;
    }
    let ver = read_u16_at(tpl, 0);
    let sig = read_u16_at(tpl, 2);
    let is_ex = ver == 1 && sig == 0xFFFF;
    let (style, item_count, mut off) = if is_ex {
        (read_u32_at(tpl, 12), read_u16_at(tpl, 16) as usize, 26)
    } else {
        (read_u32_at(tpl, 0), read_u16_at(tpl, 8) as usize, 18)
    };
    // menu, windowClass, title
    off = skip_sz_or_ord(tpl, off, true);
    off = skip_sz_or_ord(tpl, off, true);
    off = skip_sz_or_ord(tpl, off, false);
    let font_pt_off = if style & DS_SETFONT != 0 {
        let pt_off = off;
        off += 2; // pointsize
        if is_ex {
            off += 4; // weight (u16), italic (u8), charset (u8)
        }
        off = skip_sz_or_ord(tpl, off, false); // typeface
        Some(pt_off)
    } else {
        None
    };
    let item_fixed = if is_ex { 24 } else { 18 };
    for _ in 0..item_count {
        off = align4(off);
        off += item_fixed;
        off = skip_sz_or_ord(tpl, off, true); // windowClass
        off = skip_sz_or_ord(tpl, off, true); // title
        let extra = read_u16_at(tpl, off) as usize;
        off += 2 + extra;
    }
    Some((off, font_pt_off))
}

/// Clones a `DLGTEMPLATE`/`DLGTEMPLATEEX` and scales its font point size by `96 / dpi` so
/// legacy 96-DPI Win32 dialogs (like RA2's Map Selection dialog `0x6B`) match 96-DPI dimensions.
fn patch_dialog_template_bytes(src: &[u8], dpi: u32) -> Option<Vec<u8>> {
    if dpi <= 96 || src.len() < 18 {
        return None;
    }
    let ver = u16::from_le_bytes([src[0], src[1]]);
    let sig = u16::from_le_bytes([src[2], src[3]]);
    let is_ex = ver == 1 && sig == 0xFFFF;
    let (style, mut off) = if is_ex {
        if src.len() < 26 {
            return None;
        }
        (u32::from_le_bytes([src[12], src[13], src[14], src[15]]), 26)
    } else {
        (u32::from_le_bytes([src[0], src[1], src[2], src[3]]), 18)
    };
    if style & DS_SETFONT == 0 {
        return None;
    }
    for allow_ord in [true, true, false] {
        if off + 2 > src.len() {
            return None;
        }
        let first = u16::from_le_bytes([src[off], src[off + 1]]);
        if first == 0 {
            off += 2;
        } else if allow_ord && first == 0xFFFF {
            off += 4;
        } else {
            off += 2;
            while off + 2 <= src.len() && u16::from_le_bytes([src[off], src[off + 1]]) != 0 {
                off += 2;
            }
            off += 2;
        }
    }
    if off + 2 > src.len() {
        return None;
    }
    let old_pt = u16::from_le_bytes([src[off], src[off + 1]]);
    let new_pt = scale_dialog_point_size(old_pt, dpi);
    if new_pt == old_pt {
        return None;
    }
    let mut out = src.to_vec();
    out[off..off + 2].copy_from_slice(&new_pt.to_le_bytes());
    Some(out)
}

fn load_and_patch_dialog_resource(hinst: HINSTANCE, template_name: PCSTR) -> Option<Vec<u8>> {
    let dpi = system_dpi();
    if dpi <= 96 || bound_hwnd_raw() == 0 {
        return None;
    }
    unsafe {
        let hmod = if hinst.0.is_null() {
            None
        } else {
            Some(HMODULE(hinst.0))
        };
        let hres = FindResourceA(hmod, template_name, PCSTR(RT_DIALOG.0 as *const u8)).ok()?;
        let size = SizeofResource(hmod, hres) as usize;
        if size < 18 {
            return None;
        }
        let hglob = LoadResource(hmod, hres).ok()?;
        let ptr = LockResource(hglob) as *const u8;
        if ptr.is_null() {
            return None;
        }
        patch_dialog_template_bytes(std::slice::from_raw_parts(ptr, size), dpi)
    }
}

fn patch_dialog_indirect_ptr(tpl: *const DLGTEMPLATE) -> Option<Vec<u8>> {
    let dpi = system_dpi();
    if dpi <= 96 || bound_hwnd_raw() == 0 || tpl.is_null() {
        return None;
    }
    unsafe {
        let raw = tpl.cast::<u8>();
        let (len, font_pt_off) = measure_dialog_template(raw)?;
        let pt_off = font_pt_off?;
        if pt_off + 2 > len {
            return None;
        }
        let old_pt = read_u16_at(raw, pt_off);
        let new_pt = scale_dialog_point_size(old_pt, dpi);
        if new_pt == old_pt {
            return None;
        }
        let mut out = std::slice::from_raw_parts(raw, len).to_vec();
        out[pt_off..pt_off + 2].copy_from_slice(&new_pt.to_le_bytes());
        Some(out)
    }
}

unsafe extern "system" fn hook_CreateDialogParamA(
    hinst: HINSTANCE,
    template_name: PCSTR,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    if let Some(patched) = load_and_patch_dialog_resource(hinst, template_name) {
        if let Ok(hwnd) = unsafe {
            CreateDialogIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        } {
            return hwnd;
        }
    }
    unsafe {
        CreateDialogParamA(
            Some(hinst),
            template_name,
            Some(parent),
            dlg_proc,
            init_param,
        )
    }
    .unwrap_or_default()
}

unsafe extern "system" fn hook_CreateDialogIndirectParamA(
    hinst: HINSTANCE,
    template: *const DLGTEMPLATE,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    if let Some(patched) = patch_dialog_indirect_ptr(template) {
        if let Ok(hwnd) = unsafe {
            CreateDialogIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        } {
            return hwnd;
        }
    }
    unsafe { CreateDialogIndirectParamA(Some(hinst), template, Some(parent), dlg_proc, init_param) }
        .unwrap_or_default()
}

unsafe extern "system" fn hook_DialogBoxParamA(
    hinst: HINSTANCE,
    template_name: PCSTR,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> isize {
    if let Some(patched) = load_and_patch_dialog_resource(hinst, template_name) {
        return unsafe {
            DialogBoxIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        };
    }
    unsafe {
        DialogBoxParamA(
            Some(hinst),
            template_name,
            Some(parent),
            dlg_proc,
            init_param,
        )
    }
}

unsafe extern "system" fn hook_DialogBoxIndirectParamA(
    hinst: HINSTANCE,
    template: *const DLGTEMPLATE,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> isize {
    if let Some(patched) = patch_dialog_indirect_ptr(template) {
        return unsafe {
            DialogBoxIndirectParamA(
                Some(hinst),
                patched.as_ptr().cast::<DLGTEMPLATE>(),
                Some(parent),
                dlg_proc,
                init_param,
            )
        };
    }
    unsafe { DialogBoxIndirectParamA(Some(hinst), template, Some(parent), dlg_proc, init_param) }
}

unsafe extern "system" fn getmsg_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        if let Some((bound, w, h, _)) = logical_mode() {
            let msg = unsafe { &mut *(lparam.0 as *mut MSG) };
            let is_bound = msg.hwnd == bound;
            let is_dialog = is_related_dialog_window(bound, msg.hwnd);
            if is_bound || is_dialog {
                let mut cursor = POINT::default();
                if unsafe { GetCursorPos(&mut cursor) }.is_ok()
                    && unsafe { ScreenToClient(bound, &mut cursor) }.as_bool()
                {
                    let (lx, ly) = client_to_logical(bound, cursor.x, cursor.y, w, h);
                    msg.pt = POINT { x: lx, y: ly };
                    if is_mouse_message(msg.message) {
                        if matches!(
                            msg.message,
                            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
                        ) {
                            ensure_present_on_top_if_active(bound);
                        }
                        let captured = unsafe { GetCapture() };
                        let target = if captured == msg.hwnd || is_bound {
                            msg.hwnd
                        } else {
                            hit_test_logical_child(bound, lx, ly).unwrap_or(bound)
                        };
                        msg.hwnd = target;
                        if msg.message == WM_MOUSEWHEEL || target == bound {
                            msg.lParam =
                                LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
                        } else {
                            let (tx, ty) = client_origin_in_bound(bound, target);
                            let cx = lx - tx;
                            let cy = ly - ty;
                            msg.lParam =
                                LPARAM(((cy as u16 as u32) << 16 | (cx as u16 as u32)) as isize);
                        }
                    }
                }
            }
        }
    }
    let hook = HHOOK(GETMSG_HOOK.load(Ordering::Relaxed) as *mut c_void);
    unsafe { CallNextHookEx(Some(hook), code, wparam, lparam) }
}

unsafe extern "system" fn subclass_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    mut lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_FLUSH {
        crate::draw::flush_pending_present(false);
        return LRESULT(0);
    }
    if msg == WM_TIMER && wparam.0 == FLUSH_TIMER_ID {
        unsafe {
            let _ = KillTimer(Some(hwnd), FLUSH_TIMER_ID);
        }
        crate::draw::flush_pending_present(true);
        return LRESULT(0);
    }
    let toggle_vk = config::get().toggle_fullscreen_vk;
    if toggle_vk != 0 {
        let alt_down = (lparam.0 as u32) & (1 << 29) != 0;
        let was_down = (lparam.0 as u32) & (1 << 30) != 0;
        if msg == WM_SYSKEYDOWN && wparam.0 as u32 == toggle_vk && alt_down {
            if !was_down {
                crate::draw::toggle_fullscreen_hotkey();
            }
            return LRESULT(0);
        }
        if msg == WM_SYSCHAR && wparam.0 as u32 == toggle_vk && alt_down {
            // Suppress default Windows Alt+Enter system beep.
            return LRESULT(0);
        }
    }
    let shield_focus = is_scaling_active()
        && (config::get().noactivateapp || !crate::draw::is_fullscreen_active());
    if shield_focus && matches!(msg, WM_KILLFOCUS | WM_NCACTIVATE) {
        return if SUBCLASSED_UNICODE.load(Ordering::Relaxed) {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        } else {
            unsafe { DefWindowProcA(hwnd, msg, wparam, lparam) }
        };
    }
    if msg == WM_ACTIVATE && shield_focus {
        let active = (wparam.0 & 0xffff) != 0;
        if !ACTIVATE_PASSED.swap(true, Ordering::Relaxed) && active {
            // Allow the initial activation through once so the game initializes its focus flag.
        } else {
            return LRESULT(0);
        }
    }
    if msg == WM_ACTIVATEAPP {
        if wparam.0 == 0 {
            unsafe {
                let _ = ClipCursor(None);
            }
        } else {
            crate::draw::on_host_window_changed();
        }
        if shield_focus {
            if !ACTIVATEAPP_PASSED.swap(true, Ordering::Relaxed) && wparam.0 != 0 {
                // Allow initial activation once.
            } else {
                return LRESULT(0);
            }
        }
    }
    if matches!(msg, WM_MOVE | WM_SIZE | WM_WINDOWPOSCHANGED) && hwnd.0 as isize == bound_hwnd_raw()
    {
        crate::draw::on_host_window_changed();
    }
    // Only remap physical coordinates on `bound` when no owned presentation overlay exists;
    // when `PRESENT_HWND` is active, `present_wndproc` and `getmsg_hook` deliver logical coords.
    if is_mouse_message(msg) && present_hwnd_raw() == 0 {
        if let Some((bound, w, h, _)) = logical_mode() {
            if hwnd == bound {
                let raw = lparam.0 as u32;
                let mut px = (raw & 0xffff) as i16 as i32;
                let mut py = ((raw >> 16) & 0xffff) as i16 as i32;
                if msg == WM_MOUSEWHEEL {
                    let mut pt = POINT { x: px, y: py };
                    if unsafe { ScreenToClient(bound, &mut pt) }.as_bool() {
                        px = pt.x;
                        py = pt.y;
                    }
                }
                let (lx, ly) = client_to_logical(bound, px, py, w, h);
                lparam = LPARAM(((ly as u16 as u32) << 16 | (lx as u16 as u32)) as isize);
            }
        }
    }
    let prev_raw = ORIG_WNDPROC.load(Ordering::Relaxed);
    let prev: WNDPROC = unsafe { std::mem::transmute(prev_raw) };
    if SUBCLASSED_UNICODE.load(Ordering::Relaxed) {
        unsafe { CallWindowProcW(prev, hwnd, msg, wparam, lparam) }
    } else {
        unsafe { CallWindowProcA(prev, hwnd, msg, wparam, lparam) }
    }
}

pub(crate) fn install_getmsg_hook(tid: u32) {
    uninstall_getmsg_hook();
    if tid == 0 {
        return;
    }
    if let Ok(hook) = unsafe { SetWindowsHookExW(WH_GETMESSAGE, Some(getmsg_hook), None, tid) } {
        GETMSG_HOOK.store(hook.0 as isize, Ordering::Relaxed);
    }
}

pub(crate) fn uninstall_getmsg_hook() {
    let raw = GETMSG_HOOK.swap(0, Ordering::Relaxed);
    if raw != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut c_void));
        }
    }
}

pub(crate) fn install_subclass(hwnd: HWND) {
    uninstall_subclass();
    if hwnd.is_invalid() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    let is_unicode = unsafe { IsWindowUnicode(hwnd) }.as_bool();
    let new_proc = subclass_wndproc as *const () as usize as i32;
    let prev = if is_unicode {
        let cur = unsafe { GetWindowLongW(hwnd, GWLP_WNDPROC) };
        if cur == new_proc || cur == 0 {
            return;
        }
        unsafe { SetWindowLongW(hwnd, GWLP_WNDPROC, new_proc) }
    } else {
        let cur = unsafe { GetWindowLongA(hwnd, GWLP_WNDPROC) };
        if cur == new_proc || cur == 0 {
            return;
        }
        unsafe { SetWindowLongA(hwnd, GWLP_WNDPROC, new_proc) }
    };
    if prev != 0 {
        ORIG_WNDPROC.store(prev as isize, Ordering::Relaxed);
        SUBCLASSED_UNICODE.store(is_unicode, Ordering::Relaxed);
        SUBCLASSED_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        ACTIVATE_PASSED.store(false, Ordering::Relaxed);
        ACTIVATEAPP_PASSED.store(false, Ordering::Relaxed);
    }
}

pub(crate) fn uninstall_subclass() {
    let raw = SUBCLASSED_HWND.swap(0, Ordering::Relaxed);
    let prev = ORIG_WNDPROC.swap(0, Ordering::Relaxed);
    if raw == 0 || prev == 0 {
        return;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), FLUSH_TIMER_ID);
    }
    let our_proc = subclass_wndproc as *const () as usize as i32;
    if SUBCLASSED_UNICODE.load(Ordering::Relaxed) {
        if unsafe { GetWindowLongW(hwnd, GWLP_WNDPROC) } == our_proc {
            unsafe {
                let _ = SetWindowLongW(hwnd, GWLP_WNDPROC, prev as i32);
            }
        }
    } else if unsafe { GetWindowLongA(hwnd, GWLP_WNDPROC) } == our_proc {
        unsafe {
            let _ = SetWindowLongA(hwnd, GWLP_WNDPROC, prev as i32);
        }
    }
}

#[repr(C)]
struct DosHeader {
    e_magic: u16,
    _pad: [u8; 58],
    e_lfanew: i32,
}

#[repr(C)]
struct DataDirectory {
    virtual_address: u32,
    size: u32,
}

#[repr(C)]
struct OptionalHeader32Prefix {
    magic: u16,
    _pad: [u8; 90],
    number_of_rva_and_sizes: u32,
    data_directory: [DataDirectory; 16],
}

#[repr(C)]
struct NtHeaders32 {
    signature: u32,
    _file_header: [u8; 20],
    optional: OptionalHeader32Prefix,
}

#[repr(C)]
struct ImportDescriptor {
    original_first_thunk: u32,
    time_date_stamp: u32,
    forwarder_chain: u32,
    name: u32,
    first_thunk: u32,
}

pub(crate) fn install_iat_hooks() {
    let Ok(mut patches) = IAT_PATCHES.lock() else {
        return;
    };
    if !patches.is_empty() {
        return;
    }
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let base = module.0 as usize;
    if base == 0 {
        return;
    }
    let targets: &[(&str, &str, usize)] = &[
        (
            "user32.dll",
            "GetClientRect",
            hook_GetClientRect as *const () as usize,
        ),
        (
            "user32.dll",
            "GetWindowRect",
            hook_GetWindowRect as *const () as usize,
        ),
        (
            "user32.dll",
            "GetSystemMetrics",
            hook_GetSystemMetrics as *const () as usize,
        ),
        (
            "user32.dll",
            "ClientToScreen",
            hook_ClientToScreen as *const () as usize,
        ),
        (
            "user32.dll",
            "ScreenToClient",
            hook_ScreenToClient as *const () as usize,
        ),
        (
            "user32.dll",
            "GetCursorPos",
            hook_GetCursorPos as *const () as usize,
        ),
        (
            "user32.dll",
            "SetCursorPos",
            hook_SetCursorPos as *const () as usize,
        ),
        (
            "user32.dll",
            "ClipCursor",
            hook_ClipCursor as *const () as usize,
        ),
        (
            "user32.dll",
            "GetClipCursor",
            hook_GetClipCursor as *const () as usize,
        ),
        (
            "user32.dll",
            "WindowFromPoint",
            hook_WindowFromPoint as *const () as usize,
        ),
        (
            "user32.dll",
            "SetWindowPos",
            hook_SetWindowPos as *const () as usize,
        ),
        (
            "user32.dll",
            "BringWindowToTop",
            hook_BringWindowToTop as *const () as usize,
        ),
        (
            "user32.dll",
            "MoveWindow",
            hook_MoveWindow as *const () as usize,
        ),
        (
            "user32.dll",
            "CreateDialogParamA",
            hook_CreateDialogParamA as *const () as usize,
        ),
        (
            "user32.dll",
            "CreateDialogIndirectParamA",
            hook_CreateDialogIndirectParamA as *const () as usize,
        ),
        (
            "user32.dll",
            "DialogBoxParamA",
            hook_DialogBoxParamA as *const () as usize,
        ),
        (
            "user32.dll",
            "DialogBoxIndirectParamA",
            hook_DialogBoxIndirectParamA as *const () as usize,
        ),
        (
            "gdi32.dll",
            "GetDeviceCaps",
            hook_GetDeviceCaps as *const () as usize,
        ),
    ];

    unsafe {
        let dos = &*(base as *const DosHeader);
        if dos.e_magic != 0x5A4D || dos.e_lfanew <= 0 {
            return;
        }
        let nt = &*((base + dos.e_lfanew as usize) as *const NtHeaders32);
        if nt.signature != 0x0000_4550 || nt.optional.magic != 0x010B {
            return;
        }
        if nt.optional.number_of_rva_and_sizes <= 1 {
            return;
        }
        let import_rva = nt.optional.data_directory[1].virtual_address as usize;
        if import_rva == 0 {
            return;
        }
        let mut desc = (base + import_rva) as *const ImportDescriptor;
        while (*desc).name != 0 {
            let dll_name =
                CStr::from_ptr((base + (*desc).name as usize) as *const i8).to_string_lossy();
            let has_dll = targets
                .iter()
                .any(|(d, _, _)| dll_name.eq_ignore_ascii_case(d));
            if has_dll && (*desc).original_first_thunk != 0 && (*desc).first_thunk != 0 {
                let mut orig_thunk = (base + (*desc).original_first_thunk as usize) as *const u32;
                let mut iat_thunk = (base + (*desc).first_thunk as usize) as *mut usize;
                while *orig_thunk != 0 {
                    // Skip ordinal-only imports (bit 31 set on PE32).
                    if *orig_thunk & 0x8000_0000 == 0 {
                        let name_ptr = (base + *orig_thunk as usize + 2) as *const i8;
                        let fn_name = CStr::from_ptr(name_ptr).to_string_lossy();
                        if let Some(&(_, _, replacement)) = targets
                            .iter()
                            .find(|(d, f, _)| dll_name.eq_ignore_ascii_case(d) && fn_name == *f)
                        {
                            let mut old_prot = PAGE_PROTECTION_FLAGS(0);
                            if VirtualProtect(
                                iat_thunk.cast(),
                                std::mem::size_of::<usize>(),
                                PAGE_READWRITE,
                                &mut old_prot,
                            )
                            .is_ok()
                            {
                                let original = *iat_thunk;
                                if original != replacement {
                                    *iat_thunk = replacement;
                                    patches.push(PatchedSlot {
                                        slot: iat_thunk as usize,
                                        original,
                                    });
                                }
                                let mut _ignored = PAGE_PROTECTION_FLAGS(0);
                                let _ = VirtualProtect(
                                    iat_thunk.cast(),
                                    std::mem::size_of::<usize>(),
                                    old_prot,
                                    &mut _ignored,
                                );
                            }
                        }
                    }
                    orig_thunk = orig_thunk.add(1);
                    iat_thunk = iat_thunk.add(1);
                }
            }
            desc = desc.add(1);
        }
    }
}

pub(crate) fn uninstall_iat_hooks() {
    let Ok(mut patches) = IAT_PATCHES.lock() else {
        return;
    };
    for patch in patches.drain(..) {
        unsafe {
            let slot = patch.slot as *mut usize;
            let mut old_prot = PAGE_PROTECTION_FLAGS(0);
            if VirtualProtect(
                slot.cast(),
                std::mem::size_of::<usize>(),
                PAGE_READWRITE,
                &mut old_prot,
            )
            .is_ok()
            {
                *slot = patch.original;
                let mut _ignored = PAGE_PROTECTION_FLAGS(0);
                let _ = VirtualProtect(
                    slot.cast(),
                    std::mem::size_of::<usize>(),
                    old_prot,
                    &mut _ignored,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, WS_POPUP};

    fn push_utf16z(buf: &mut Vec<u8>, s: &str) {
        for cu in s.encode_utf16() {
            buf.extend_from_slice(&cu.to_le_bytes());
        }
        buf.extend_from_slice(&0u16.to_le_bytes());
    }

    fn build_standard_template(cx: i16, cy: i16, pt: u16, with_item: bool) -> Vec<u8> {
        let mut buf = Vec::new();
        let style = WS_POPUP.0 | DS_SETFONT;
        buf.extend_from_slice(&style.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // dwExtendedStyle
        buf.extend_from_slice(&(u16::from(with_item)).to_le_bytes()); // cdit
        buf.extend_from_slice(&0i16.to_le_bytes()); // x
        buf.extend_from_slice(&0i16.to_le_bytes()); // y
        buf.extend_from_slice(&cx.to_le_bytes());
        buf.extend_from_slice(&cy.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // menu
        buf.extend_from_slice(&0u16.to_le_bytes()); // class
        buf.extend_from_slice(&0u16.to_le_bytes()); // title
        buf.extend_from_slice(&pt.to_le_bytes());
        push_utf16z(&mut buf, "MS Sans Serif");
        if with_item {
            while buf.len() % 4 != 0 {
                buf.push(0);
            }
            buf.extend_from_slice(&0x5000_0000u32.to_le_bytes()); // WS_CHILD | WS_VISIBLE
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&10i16.to_le_bytes());
            buf.extend_from_slice(&10i16.to_le_bytes());
            buf.extend_from_slice(&50i16.to_le_bytes());
            buf.extend_from_slice(&14i16.to_le_bytes());
            buf.extend_from_slice(&100u16.to_le_bytes()); // id
            buf.extend_from_slice(&0xFFFFu16.to_le_bytes());
            buf.extend_from_slice(&0x0082u16.to_le_bytes()); // Static
            push_utf16z(&mut buf, "OK");
            buf.extend_from_slice(&0u16.to_le_bytes()); // creation data
        }
        buf
    }

    fn build_ex_template(pt: u16) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u16.to_le_bytes()); // dlgVer
        buf.extend_from_slice(&0xFFFFu16.to_le_bytes()); // signature
        buf.extend_from_slice(&0u32.to_le_bytes()); // helpID
        buf.extend_from_slice(&0u32.to_le_bytes()); // exStyle
        buf.extend_from_slice(&(WS_POPUP.0 | DS_SETFONT).to_le_bytes()); // style
        buf.extend_from_slice(&0u16.to_le_bytes()); // cDlgItems
        buf.extend_from_slice(&0i16.to_le_bytes()); // x
        buf.extend_from_slice(&0i16.to_le_bytes()); // y
        buf.extend_from_slice(&200i16.to_le_bytes()); // cx
        buf.extend_from_slice(&100i16.to_le_bytes()); // cy
        buf.extend_from_slice(&0u16.to_le_bytes()); // menu
        buf.extend_from_slice(&0u16.to_le_bytes()); // class
        push_utf16z(&mut buf, "Ex"); // title
        buf.extend_from_slice(&pt.to_le_bytes());
        buf.extend_from_slice(&400u16.to_le_bytes()); // weight
        buf.push(0); // italic
        buf.push(1); // charset
        push_utf16z(&mut buf, "MS Shell Dlg");
        buf
    }

    #[test]
    fn dialog_template_point_size_scaling() {
        let std_tpl = build_standard_template(200, 100, 8, true);
        let (measured_len, pt_off) = unsafe { measure_dialog_template(std_tpl.as_ptr()) }.unwrap();
        assert_eq!(measured_len, std_tpl.len());
        let pt_off = pt_off.unwrap();
        assert_eq!(
            u16::from_le_bytes([std_tpl[pt_off], std_tpl[pt_off + 1]]),
            8
        );
        assert!(patch_dialog_template_bytes(&std_tpl, 96).is_none());
        let patched_240 = patch_dialog_template_bytes(&std_tpl, 240).unwrap();
        assert_eq!(patched_240.len(), std_tpl.len());
        assert_eq!(
            u16::from_le_bytes([patched_240[pt_off], patched_240[pt_off + 1]]),
            3
        );
        let patched_192 = patch_dialog_template_bytes(&std_tpl, 192).unwrap();
        assert_eq!(
            u16::from_le_bytes([patched_192[pt_off], patched_192[pt_off + 1]]),
            4
        );

        let ex_tpl = build_ex_template(8);
        let (ex_len, ex_pt_off) = unsafe { measure_dialog_template(ex_tpl.as_ptr()) }.unwrap();
        assert_eq!(ex_len, ex_tpl.len());
        let ex_pt_off = ex_pt_off.unwrap();
        let ex_patched = patch_dialog_template_bytes(&ex_tpl, 240).unwrap();
        assert_eq!(
            u16::from_le_bytes([ex_patched[ex_pt_off], ex_patched[ex_pt_off + 1]]),
            3
        );
    }

    #[test]
    fn patched_dialog_keeps_96_dpi_pixel_dimensions() {
        crate::window::enable_dpi_awareness();
        let dpi = system_dpi();
        let raw = build_standard_template(200, 100, 8, false);
        let active = patch_dialog_template_bytes(&raw, dpi).unwrap_or(raw);
        let hwnd = unsafe {
            CreateDialogIndirectParamA(
                None,
                active.as_ptr().cast::<DLGTEMPLATE>(),
                None,
                None,
                LPARAM(0),
            )
        }
        .expect("CreateDialogIndirectParamA should create patched dialog");
        let mut rc = RECT::default();
        unsafe {
            GetClientRect(hwnd, &mut rc).unwrap();
            let _ = DestroyWindow(hwnd);
        }
        // At 96 DPI, 200x100 DLU with 8pt MS Sans Serif is 300x163 px.
        assert!(
            (280..=320).contains(&rc.right) && (145..=180).contains(&rc.bottom),
            "unexpected patched dialog size {}x{} at {} DPI",
            rc.right,
            rc.bottom,
            dpi
        );
    }
}
