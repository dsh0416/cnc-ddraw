//! Safe Win32 OS primitives centralizing benign API calls, handle lifetimes, and atomic HWNDs.
#![allow(non_snake_case)]

pub(crate) mod geometry;
pub(crate) mod hwnd;
pub(crate) mod process;

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::Result;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;

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

    pub(crate) fn load_raw(&self) -> isize {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn store(&self, hwnd: Option<HWND>) {
        let raw = hwnd.map_or(0, |h| h.0 as isize);
        self.0.store(raw, Ordering::Relaxed);
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
