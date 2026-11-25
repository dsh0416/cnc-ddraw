//! Thin Win32 hooking layer: PE32 IAT patching, host WndProc subclassing, and WH_GETMESSAGE.
use std::ffi::{c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{BOOL, PCSTR};
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetDC, GetDeviceCaps, ReleaseDC, ScreenToClient, BITSPIXEL,
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
    GetDesktopWindow, GetSystemMetrics, GetWindowLongA, GetWindowLongW, GetWindowRect,
    IsWindowUnicode, KillTimer, MoveWindow, SetCursorPos, SetWindowLongA, SetWindowLongW,
    SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, DLGPROC, DLGTEMPLATE,
    GWLP_WNDPROC, HHOOK, MSG, RT_DIALOG, SET_WINDOW_POS_FLAGS, SM_CXSCREEN, SM_CYSCREEN,
    SYSTEM_METRICS_INDEX, WH_GETMESSAGE, WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_MOUSEWHEEL, WM_MOVE, WM_NCACTIVATE, WM_RBUTTONDOWN, WM_SIZE, WM_SYSCHAR,
    WM_SYSKEYDOWN, WM_TIMER, WM_WINDOWPOSCHANGED, WNDPROC,
};

use crate::config;

use super::dialog_template::FontPatch;
use super::input::{
    client_origin_in_bound, client_to_logical, hit_test_logical_child, is_mouse_message,
    is_related_dialog_window, logical_to_client,
};
use super::overlay::ensure_present_on_top_if_active;
use super::{
    bound_hwnd_raw, is_present_hwnd, is_scaling_active, logical_mode, present_hwnd_raw,
    FLUSH_TIMER_ID, WM_APP_FLUSH,
};
use crate::win32::geometry::{
    client_to_screen, is_window, map_rect, screen_to_client, window_rect,
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

pub(crate) fn subclassed_hwnd() -> Option<HWND> {
    let raw = SUBCLASSED_HWND.load(Ordering::Relaxed);
    (raw != 0).then_some(HWND(raw as *mut c_void))
}

fn virtual_window_rect(hwnd: HWND, include_dialogs: bool) -> Option<RECT> {
    let (bound, w, h, _) = logical_mode()?;
    if hwnd == bound || hwnd == unsafe { GetDesktopWindow() } {
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

unsafe extern "system" fn hook_GetClientRect(hwnd: HWND, rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some(value) = virtual_window_rect(hwnd, false) {
            unsafe { rect.write(value) };
            return BOOL(1);
        }
    }
    BOOL::from(unsafe { GetClientRect(hwnd, rect) }.is_ok())
}

unsafe extern "system" fn hook_GetWindowRect(hwnd: HWND, rect: *mut RECT) -> BOOL {
    if !rect.is_null() {
        if let Some(value) = virtual_window_rect(hwnd, true) {
            unsafe { rect.write(value) };
            return BOOL(1);
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
                let pt = unsafe { &mut *pt };
                let ok = client_to_screen(hwnd, pt) && screen_to_client(bound, pt);
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
                let pt = unsafe { &mut *pt };
                let ok = client_to_screen(bound, pt) && screen_to_client(hwnd, pt);
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
        if screen_to_client(bound, &mut raw) {
            let (lx, ly) = client_to_logical(bound, raw.x, raw.y, w, h);
            raw = POINT { x: lx, y: ly };
        }
    }
    unsafe { *pt = raw };
    BOOL(1)
}

unsafe extern "system" fn hook_SetCursorPos(x: i32, y: i32) -> BOOL {
    if let Some((bound, w, h, _)) = logical_mode() {
        let (cx, cy) = logical_to_client(bound, x, y, w, h);
        let mut pt = POINT { x: cx, y: cy };
        if client_to_screen(bound, &mut pt) {
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

// Win32 requires a DWORD-aligned template. The indirect caller may leave padding
// uninitialized, so copy it as raw storage and only read actual template fields.
type DialogStorage = Vec<std::mem::MaybeUninit<u32>>;

/// The source must be readable through `len` bytes and `patch` must describe that source.
unsafe fn copy_patched_template(
    src: *const u8,
    len: usize,
    patch: FontPatch,
) -> Option<DialogStorage> {
    let words = len.div_ceil(4);
    let mut out = DialogStorage::new();
    out.try_reserve_exact(words).ok()?;
    out.resize_with(words, std::mem::MaybeUninit::uninit);
    let dst = out.as_mut_ptr().cast::<u8>();
    // SAFETY: the destination spans at least len bytes, is freshly allocated,
    // and the parser located a full point-size word within the source.
    unsafe {
        std::ptr::copy_nonoverlapping(src, dst, len);
        dst.add(patch.offset)
            .cast::<u16>()
            .write_unaligned(patch.point_size);
    }
    Some(out)
}

/// The module and resource identifier must satisfy the FindResourceA contract.
unsafe fn load_and_patch_dialog_resource(
    hinst: HINSTANCE,
    template_name: PCSTR,
) -> Option<DialogStorage> {
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
        if size == 0 || size > isize::MAX as usize {
            return None;
        }
        let hglob = LoadResource(hmod, hres).ok()?;
        let ptr = LockResource(hglob) as *const u8;
        if ptr.is_null() {
            return None;
        }
        // LockResource returns SizeofResource initialized bytes, valid while
        // the caller's module remains loaded for this call.
        let patch = FontPatch::from_bytes(std::slice::from_raw_parts(ptr, size), dpi)?;
        copy_patched_template(ptr, size, patch)
    }
}

/// Win32 passes no length. The indirect-dialog caller must supply one complete
/// DLGTEMPLATE or DLGTEMPLATEEX that remains readable and stable during the call.
unsafe fn patch_dialog_indirect_ptr(tpl: *const DLGTEMPLATE) -> Option<DialogStorage> {
    let dpi = system_dpi();
    if dpi <= 96 || bound_hwnd_raw() == 0 || tpl.is_null() {
        return None;
    }
    let raw = tpl.cast::<u8>();
    // SAFETY: the parser requests only initialized structural words at their
    // documented offsets in the caller's valid template, never alignment padding.
    let patch = FontPatch::parse(None, dpi, |offset| unsafe {
        raw.add(offset).cast::<u16>().read_unaligned()
    })?;
    unsafe { copy_patched_template(raw, patch.len, patch) }
}

unsafe extern "system" fn hook_CreateDialogParamA(
    hinst: HINSTANCE,
    template_name: PCSTR,
    parent: HWND,
    dlg_proc: DLGPROC,
    init_param: LPARAM,
) -> HWND {
    if let Some(patched) = unsafe { load_and_patch_dialog_resource(hinst, template_name) } {
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
    if let Some(patched) = unsafe { patch_dialog_indirect_ptr(template) } {
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
    if let Some(patched) = unsafe { load_and_patch_dialog_resource(hinst, template_name) } {
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
    if let Some(patched) = unsafe { patch_dialog_indirect_ptr(template) } {
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
                    && screen_to_client(bound, &mut cursor)
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
    if hwnd.is_invalid() || !is_window(hwnd) {
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
    if !is_window(hwnd) {
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

    const DS_SETFONT: u32 = 0x40;

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
        assert!(FontPatch::from_bytes(&std_tpl, 96).is_none());
        for (template, offset) in [(&std_tpl, 24), (&build_ex_template(8), 36)] {
            for (dpi, expected) in [(192, 4), (240, 3)] {
                let patch = FontPatch::from_bytes(template, dpi).unwrap();
                assert_eq!(patch.len, template.len());
                assert_eq!(patch.offset, offset);
                assert_eq!(patch.point_size, expected);
                let mut expected_bytes = template.clone();
                expected_bytes[offset..offset + 2].copy_from_slice(&expected.to_le_bytes());
                // Both fixtures initialize every byte, including control padding.
                unsafe {
                    let copy =
                        copy_patched_template(template.as_ptr(), template.len(), patch).unwrap();
                    let bytes =
                        std::slice::from_raw_parts(copy.as_ptr().cast::<u8>(), template.len());
                    assert_eq!(bytes, expected_bytes);
                }
            }
        }
    }

    #[test]
    fn truncated_templates_are_rejected() {
        for template in [
            build_standard_template(200, 100, 8, true),
            build_ex_template(8),
        ] {
            for len in 0..template.len() {
                assert!(
                    FontPatch::from_bytes(&template[..len], 192).is_none(),
                    "accepted {len} bytes"
                );
            }
        }
    }

    #[test]
    fn control_creation_data_lengths_follow_each_format() {
        let mut standard = build_standard_template(200, 100, 8, true);
        let end = standard.len();
        standard[end - 2..].copy_from_slice(&5u16.to_le_bytes());
        standard.extend_from_slice(&[1, 2, 3]);
        assert!(FontPatch::parse(None, 192, |i| u16::from_le_bytes([
            standard[i],
            standard[i + 1]
        ]))
        .is_none());

        let mut extended = build_ex_template(8);
        extended[16..18].copy_from_slice(&1u16.to_le_bytes());
        extended.resize(extended.len().next_multiple_of(4) + 24, 0);
        extended.extend_from_slice(&[0xff, 0xff, 0x82, 0, 0, 0, 3, 0, 1, 2, 3]);

        for template in [standard, extended] {
            assert_eq!(
                FontPatch::from_bytes(&template, 192).unwrap().len,
                template.len()
            );
            assert!(FontPatch::from_bytes(&template[..template.len() - 1], 192).is_none());
        }
    }

    #[test]
    fn system_message_font_can_omit_typeface() {
        let mut standard = build_standard_template(200, 100, 0x7fff, false);
        standard.truncate(26); // template ends immediately after pointsize
        assert!(FontPatch::parse(None, 192, |i| u16::from_le_bytes([
            standard[i],
            standard[i + 1]
        ]))
        .is_none());
    }

    #[test]
    fn patched_dialog_keeps_96_dpi_pixel_dimensions() {
        crate::window::enable_dpi_awareness();
        let dpi = system_dpi();
        let raw = build_standard_template(200, 100, 8, false);
        let patched = FontPatch::from_bytes(&raw, dpi)
            .map(|patch| unsafe { copy_patched_template(raw.as_ptr(), raw.len(), patch).unwrap() });
        let template = patched
            .as_ref()
            .map_or(raw.as_ptr().cast(), |p| p.as_ptr().cast());
        let hwnd = unsafe { CreateDialogIndirectParamA(None, template, None, None, LPARAM(0)) }
            .expect("CreateDialogIndirectParamA should create patched dialog");
        let rc = super::super::client_rect(hwnd).unwrap();
        unsafe {
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
