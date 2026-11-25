//! Safe Win32 OS primitives centralizing benign API calls, handle lifetimes, and atomic HWNDs.
#![allow(non_snake_case)]

pub(crate) mod geometry;
pub(crate) mod hwnd;
pub(crate) mod iat;
pub(crate) mod process;

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::Result;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Gdi::{
    GetDC, GetDeviceCaps, ReleaseDC, GET_DEVICE_CAPS_INDEX, HDC, LOGPIXELSY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SetProcessDPIAware, SYSTEM_METRICS_INDEX,
};

pub(crate) use process::{current_process_id, current_thread_id, module_path_at};

/// Lock-free storage for an optional `HWND`, isolating pointer-to-integer handle casts.
pub(crate) struct AtomicHwnd(AtomicIsize);

impl AtomicHwnd {
    pub(crate) const fn new() -> Self {
        Self(AtomicIsize::new(0))
    }

    pub(crate) fn load(&self) -> Option<HWND> {
        let raw = self.0.load(Ordering::Relaxed);
        (raw != 0).then_some(HWND(raw as *mut c_void))
    }

    pub(crate) fn store(&self, hwnd: Option<HWND>) {
        let raw = hwnd.map_or(0, |h| h.0 as isize);
        self.0.store(raw, Ordering::Relaxed);
    }

    pub(crate) fn swap(&self, hwnd: Option<HWND>) -> Option<HWND> {
        let raw = hwnd.map_or(0, |h| h.0 as isize);
        let prev = self.0.swap(raw, Ordering::Relaxed);
        (prev != 0).then_some(HWND(prev as *mut c_void))
    }

    pub(crate) fn clear_if(&self, hwnd: HWND) {
        let raw = hwnd.0 as isize;
        if raw != 0 {
            let _ = self
                .0
                .compare_exchange(raw, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    pub(crate) fn matches(&self, hwnd: HWND) -> bool {
        let raw = self.0.load(Ordering::Relaxed);
        raw != 0 && hwnd.0 as isize == raw
    }
}

pub(crate) fn enable_dpi_awareness() {
    // SAFETY: SetProcessDPIAware takes no arguments and is safe to call during process init.
    unsafe {
        let _ = SetProcessDPIAware();
    }
}

pub(crate) fn current_module_handle() -> Result<HMODULE> {
    // SAFETY: Passing None queries the current executable module handle.
    unsafe { GetModuleHandleW(None) }
}

pub(crate) fn system_dpi() -> u32 {
    // SAFETY: GetDC(None) returns the screen DC which is released immediately via ReleaseDC.
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

pub(crate) fn system_metrics(index: i32) -> i32 {
    // SAFETY: GetSystemMetrics accepts any integer metric index and returns 0 if invalid.
    unsafe { GetSystemMetrics(SYSTEM_METRICS_INDEX(index)) }
}

pub(crate) fn device_caps(hdc: HDC, index: i32) -> i32 {
    // SAFETY: GetDeviceCaps validates the HDC and metric index internally.
    unsafe { GetDeviceCaps(Some(hdc), GET_DEVICE_CAPS_INDEX(index as u32)) }
}
