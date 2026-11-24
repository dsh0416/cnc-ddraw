//! Legacy DirectDraw DLL entry points, thread lock, helper exports, and COM class factory.
#![allow(non_snake_case)]

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_POINTER, FARPROC, HINSTANCE,
    HMODULE, S_FALSE,
};
use windows::Win32::Graphics::DirectDraw::{
    CLSID_DirectDraw, CLSID_DirectDraw7, CLSID_DirectDrawClipper,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetProcAddress;
use windows::Win32::System::Threading::{
    EnterCriticalSection, InitializeCriticalSection, LeaveCriticalSection, CRITICAL_SECTION,
};

use crate::{clipper, draw, window};

const DLL_PROCESS_ATTACH: u32 = 1;

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    _module: HINSTANCE,
    reason: u32,
    _reserved: *mut c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // Must run before WinMain creates any HWND so process DPI awareness is consistent.
        window::enable_dpi_awareness();
    }
    BOOL(1)
}

struct ThreadLock(UnsafeCell<CRITICAL_SECTION>);

// Win32 CRITICAL_SECTION is thread-safe by design once initialized.
unsafe impl Sync for ThreadLock {}

fn dd_thread_lock() -> &'static ThreadLock {
    static LOCK: OnceLock<&'static ThreadLock> = OnceLock::new();
    *LOCK.get_or_init(|| {
        let boxed = Box::leak(Box::new(ThreadLock(UnsafeCell::new(
            CRITICAL_SECTION::default(),
        ))));
        unsafe { InitializeCriticalSection(boxed.0.get()) };
        boxed
    })
}

static ZOOM_ENABLED: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
pub unsafe extern "system" fn AcquireDDThreadLock() -> u32 {
    unsafe { EnterCriticalSection(dd_thread_lock().0.get()) };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ReleaseDDThreadLock() -> u32 {
    unsafe { LeaveCriticalSection(dd_thread_lock().0.get()) };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn CompleteCreateSysmemSurface(_arg: u32) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3DParseUnknownCommand(
    _cmd: *mut c_void,
    ret_cmd: *mut *mut c_void,
) -> HRESULT {
    if !ret_cmd.is_null() {
        unsafe { ret_cmd.write(ptr::null_mut()) };
    }
    E_FAIL
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DDInternalLock(_a: u32, _b: u32) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DDInternalUnlock(_a: u32) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn DDIsWindowed() -> BOOL {
    BOOL::from(!draw::is_fullscreen_active())
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DDGetProcAddress(module: HMODULE, proc_name: PCSTR) -> FARPROC {
    unsafe { GetProcAddress(module, proc_name) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn DDEnableZoom() {
    ZOOM_ENABLED.store(true, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
enum FactoryClass {
    DirectDraw,
    DirectDrawClipper,
}

#[implement(IClassFactory, Agile = false)]
struct ClassFactory {
    class: FactoryClass,
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> Result<()> {
        if out.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { out.write(ptr::null_mut()) };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let unk: IUnknown = match self.class {
            FactoryClass::DirectDraw => draw::create_uninitialized_draw().cast()?,
            FactoryClass::DirectDrawClipper => clipper::create_uninitialized_clipper().cast()?,
        };
        unsafe { unk.query(riid, out).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    clsid: *const GUID,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || clsid.is_null() || riid.is_null() {
        return E_POINTER;
    }
    unsafe { out.write(ptr::null_mut()) };
    let clsid = unsafe { *clsid };
    let class = if clsid == CLSID_DirectDraw || clsid == CLSID_DirectDraw7 {
        FactoryClass::DirectDraw
    } else if clsid == CLSID_DirectDrawClipper {
        FactoryClass::DirectDrawClipper
    } else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    let factory: IClassFactory = ClassFactory { class }.into();
    unsafe { factory.query(riid, out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_lock_is_reentrant() {
        unsafe {
            assert_eq!(AcquireDDThreadLock(), 0);
            assert_eq!(AcquireDDThreadLock(), 0);
            assert_eq!(ReleaseDDThreadLock(), 0);
            assert_eq!(ReleaseDDThreadLock(), 0);
        }
    }

    #[test]
    fn zoom_and_windowed_helpers_succeed() {
        unsafe {
            assert_eq!(DDIsWindowed(), BOOL(1));
            DDEnableZoom();
            assert!(ZOOM_ENABLED.load(Ordering::Relaxed));
        }
    }
}
