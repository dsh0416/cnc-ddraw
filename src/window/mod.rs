//! Win32 windowing subsystem: host window mode management, owned presentation overlay,
//! logical<->physical coordinate mapping, and PE32 IAT / WndProc / WH_GETMESSAGE hooks.
#![allow(non_snake_case)]

pub mod hooks;
pub mod host;
pub mod input;
pub mod overlay;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, KillTimer, PostMessageW, SetProcessDPIAware, SetTimer,
};

pub use host::{HostWindow, PresentationMode};
pub use overlay::OverlayWindow;

pub(crate) const WM_APP_FLUSH: u32 = 0x8000 + 0x4321;
pub(crate) const FLUSH_TIMER_ID: usize = 0x434E_4344;

static NEXT_BIND_ID: AtomicUsize = AtomicUsize::new(1);
static ACTIVE_BIND_ID: AtomicUsize = AtomicUsize::new(0);
static BOUND_HWND: AtomicIsize = AtomicIsize::new(0);
static PRESENT_HWND: AtomicIsize = AtomicIsize::new(0);
static BOUND_THREAD: AtomicU32 = AtomicU32::new(0);
static LOGICAL_WIDTH: AtomicU32 = AtomicU32::new(0);
static LOGICAL_HEIGHT: AtomicU32 = AtomicU32::new(0);
static LOGICAL_BPP: AtomicU32 = AtomicU32::new(0);
static MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static LOGICAL_SCALING_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn enable_dpi_awareness() {
    unsafe {
        let _ = SetProcessDPIAware();
    }
}

/// Computes the presentation viewport inside a `target_w x target_h` client area.
/// Shared by both `wgpu` rendering and mouse input mapping so clicks always align pixel-for-pixel.
pub fn compute_viewport(
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
    maintas: bool,
) -> (u32, u32, u32, u32) {
    if !maintas || width == 0 || height == 0 || target_w == 0 || target_h == 0 {
        return (0, 0, target_w.max(1), target_h.max(1));
    }
    let w = (target_h as u64 * width as u64 / height as u64).clamp(1, target_w as u64) as u32;
    let h = (target_w as u64 * height as u64 / width as u64).clamp(1, target_h as u64) as u32;
    ((target_w - w) / 2, (target_h - h) / 2, w, h)
}

pub(crate) fn logical_mode() -> Option<(HWND, u32, u32, u32)> {
    if !LOGICAL_SCALING_ACTIVE.load(Ordering::Relaxed) || !MODE_ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let hwnd_raw = BOUND_HWND.load(Ordering::Relaxed);
    let w = LOGICAL_WIDTH.load(Ordering::Relaxed);
    let h = LOGICAL_HEIGHT.load(Ordering::Relaxed);
    let bpp = LOGICAL_BPP.load(Ordering::Relaxed);
    if hwnd_raw == 0 || w == 0 || h == 0 {
        return None;
    }
    Some((HWND(hwnd_raw as *mut c_void), w, h, bpp))
}

pub(crate) fn bound_hwnd_raw() -> isize {
    BOUND_HWND.load(Ordering::Relaxed)
}

pub(crate) fn bound_thread_id() -> u32 {
    BOUND_THREAD.load(Ordering::Relaxed)
}

/// Returns true when the caller is on the bound UI thread (or no window is bound yet).
pub fn is_ui_thread_or_unbound() -> bool {
    let bound = BOUND_THREAD.load(Ordering::Relaxed);
    bound == 0 || bound == unsafe { GetCurrentThreadId() }
}

pub(crate) fn present_hwnd_raw() -> isize {
    PRESENT_HWND.load(Ordering::Relaxed)
}

pub(crate) fn is_scaling_active() -> bool {
    LOGICAL_SCALING_ACTIVE.load(Ordering::Relaxed)
}

pub fn set_present_hwnd(hwnd: HWND) {
    PRESENT_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
}

pub fn clear_present_hwnd(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    if raw != 0 {
        let _ = PRESENT_HWND.compare_exchange(raw, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}

pub(crate) fn is_present_hwnd(hwnd: HWND) -> bool {
    let raw = PRESENT_HWND.load(Ordering::Relaxed);
    raw != 0 && hwnd.0 as isize == raw
}

pub fn bind_window(hwnd: HWND, scaling_active: bool) -> usize {
    if hwnd.is_invalid() {
        return 0;
    }
    let id = NEXT_BIND_ID.fetch_add(1, Ordering::Relaxed);
    let tid = unsafe { GetCurrentThreadId() };
    ACTIVE_BIND_ID.store(id, Ordering::Relaxed);
    BOUND_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    BOUND_THREAD.store(tid, Ordering::Relaxed);
    LOGICAL_SCALING_ACTIVE.store(scaling_active, Ordering::Relaxed);
    MODE_ACTIVE.store(false, Ordering::Relaxed);
    hooks::install_iat_hooks();
    hooks::install_subclass(hwnd);
    hooks::install_getmsg_hook(tid);
    id
}

pub fn set_mode(width: u32, height: u32, bpp: u32, active: bool) {
    LOGICAL_WIDTH.store(width, Ordering::Relaxed);
    LOGICAL_HEIGHT.store(height, Ordering::Relaxed);
    LOGICAL_BPP.store(bpp, Ordering::Relaxed);
    if BOUND_HWND.load(Ordering::Relaxed) != 0 {
        MODE_ACTIVE.store(active, Ordering::Relaxed);
    }
}

/// Keeps logical dimensions aligned with the exact primary surface dimensions presented by wgpu.
pub fn sync_presented_size(width: u32, height: u32) {
    if width > 0 && height > 0 && BOUND_HWND.load(Ordering::Relaxed) != 0 {
        LOGICAL_WIDTH.store(width, Ordering::Relaxed);
        LOGICAL_HEIGHT.store(height, Ordering::Relaxed);
        if LOGICAL_SCALING_ACTIVE.load(Ordering::Relaxed) {
            MODE_ACTIVE.store(true, Ordering::Relaxed);
        }
    }
}

pub fn set_scaling_active(bind_id: usize, active: bool) {
    if bind_id == 0 || ACTIVE_BIND_ID.load(Ordering::Relaxed) != bind_id {
        return;
    }
    LOGICAL_SCALING_ACTIVE.store(active, Ordering::Relaxed);
    MODE_ACTIVE.store(false, Ordering::Relaxed);
}

pub fn is_active_owner(bind_id: usize) -> bool {
    bind_id != 0 && ACTIVE_BIND_ID.load(Ordering::Relaxed) == bind_id
}

pub fn unbind_window(bind_id: usize) {
    if bind_id == 0
        || ACTIVE_BIND_ID
            .compare_exchange(bind_id, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    MODE_ACTIVE.store(false, Ordering::Relaxed);
    LOGICAL_SCALING_ACTIVE.store(false, Ordering::Relaxed);
    BOUND_HWND.store(0, Ordering::Relaxed);
    BOUND_THREAD.store(0, Ordering::Relaxed);
    hooks::uninstall_getmsg_hook();
    hooks::uninstall_subclass();
    hooks::uninstall_iat_hooks();
}

pub fn can_schedule_flush() -> bool {
    let raw = hooks::subclassed_hwnd_raw();
    raw != 0 && unsafe { IsWindow(Some(HWND(raw as *mut c_void))) }.as_bool()
}

pub fn schedule_flush() -> bool {
    let raw = hooks::subclassed_hwnd_raw();
    if raw == 0 {
        return false;
    }
    let hwnd = HWND(raw as *mut c_void);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return false;
    }
    unsafe {
        PostMessageW(
            Some(hwnd),
            WM_APP_FLUSH,
            Default::default(),
            Default::default(),
        )
    }
    .is_ok()
}

pub fn arm_flush_timer(ms: u32) {
    let raw = hooks::subclassed_hwnd_raw();
    if raw != 0 {
        let hwnd = HWND(raw as *mut c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            unsafe {
                let _ = SetTimer(Some(hwnd), FLUSH_TIMER_ID, ms.max(1), None);
            }
        }
    }
}

pub fn disarm_flush_timer() {
    let raw = hooks::subclassed_hwnd_raw();
    if raw != 0 {
        let hwnd = HWND(raw as *mut c_void);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            unsafe {
                let _ = KillTimer(Some(hwnd), FLUSH_TIMER_ID);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_aspect_ratio_and_stretch() {
        assert_eq!(
            compute_viewport(640, 480, 1920, 1080, true),
            (240, 0, 1440, 1080)
        );
        assert_eq!(
            compute_viewport(640, 480, 1920, 1080, false),
            (0, 0, 1920, 1080)
        );
        assert_eq!(compute_viewport(640, 480, 640, 480, true), (0, 0, 640, 480));
        assert_eq!(compute_viewport(640, 480, 1, 99, true), (0, 49, 1, 1));
    }

    #[test]
    fn stale_unbind_and_present_clear_do_not_clobber_newer_binding() {
        let old_present = HWND(0x1001 as *mut c_void);
        let new_present = HWND(0x2002 as *mut c_void);
        set_present_hwnd(old_present);
        set_present_hwnd(new_present);
        clear_present_hwnd(old_present);
        assert_eq!(present_hwnd_raw(), new_present.0 as isize);
        clear_present_hwnd(new_present);
        assert_eq!(present_hwnd_raw(), 0);

        let old_id = NEXT_BIND_ID.fetch_add(1, Ordering::Relaxed);
        let new_id = NEXT_BIND_ID.fetch_add(1, Ordering::Relaxed);
        ACTIVE_BIND_ID.store(new_id, Ordering::Relaxed);
        assert!(!is_active_owner(old_id));
        assert!(is_active_owner(new_id));
        unbind_window(old_id);
        assert!(is_active_owner(new_id));
        ACTIVE_BIND_ID.store(0, Ordering::Relaxed);
    }
}
