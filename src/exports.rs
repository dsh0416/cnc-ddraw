//! Legacy DirectDraw DLL entry points, thread lock, helper exports, and COM class factory.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::ReentrantMutex;
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

use crate::{clipper, compat, draw, window};

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_THREAD_ATTACH: u32 = 2;

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // Must run before WinMain creates any HWND so host and overlay share one DPI space.
        window::enable_dpi_awareness();
    } else if reason == DLL_THREAD_ATTACH {
        compat::on_thread_attach();
    }
    BOOL(1)
}

static DD_THREAD_LOCK: ReentrantMutex<()> = ReentrantMutex::new(());
static ZOOM_ENABLED: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
pub extern "system" fn AcquireDDThreadLock() -> u32 {
    // The matching Release export consumes this acquisition.
    std::mem::forget(DD_THREAD_LOCK.lock());
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn ReleaseDDThreadLock() -> u32 {
    if DD_THREAD_LOCK.is_owned_by_current_thread() {
        // SAFETY: Each owned recursion level has exactly one forgotten guard.
        unsafe { DD_THREAD_LOCK.force_unlock() };
    }
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn CompleteCreateSysmemSurface(_arg: u32) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn D3DParseUnknownCommand(
    _cmd: *mut c_void,
    ret_cmd: Option<&mut MaybeUninit<*mut c_void>>,
) -> HRESULT {
    if let Some(ret_cmd) = ret_cmd {
        ret_cmd.write(ptr::null_mut());
    }
    E_FAIL
}

#[unsafe(no_mangle)]
pub extern "system" fn DDInternalLock(_a: u32, _b: u32) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn DDInternalUnlock(_a: u32) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn DDIsWindowed() -> BOOL {
    BOOL::from(!draw::is_fullscreen_active())
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DDGetProcAddress(module: HMODULE, proc_name: PCSTR) -> FARPROC {
    // SAFETY: Forwarding caller's module handle and procedure name/ordinal to Win32 GetProcAddress.
    unsafe { GetProcAddress(module, proc_name) }
}

#[unsafe(no_mangle)]
pub extern "C" fn DDEnableZoom() {
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
        // SAFETY: Caller passes a writable interface pointer slot per IClassFactory::CreateInstance.
        unsafe { out.write(ptr::null_mut()) };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let unk: IUnknown = match self.class {
            FactoryClass::DirectDraw => draw::create_uninitialized_draw()?.cast()?,
            FactoryClass::DirectDrawClipper => clipper::create_uninitialized_clipper().cast()?,
        };
        // SAFETY: `unk` is live, `riid` is readable, and `out` is a writable interface slot.
        unsafe { unk.query(riid, out).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[unsafe(no_mangle)]
pub extern "system" fn DllGetClassObject(
    clsid: Option<&GUID>,
    riid: Option<&GUID>,
    out: Option<&mut MaybeUninit<*mut c_void>>,
) -> HRESULT {
    let (Some(clsid), Some(riid), Some(out)) = (clsid, riid, out) else {
        return E_POINTER;
    };
    out.write(ptr::null_mut());
    let class = if *clsid == CLSID_DirectDraw || *clsid == CLSID_DirectDraw7 {
        FactoryClass::DirectDraw
    } else if *clsid == CLSID_DirectDrawClipper {
        FactoryClass::DirectDrawClipper
    } else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    let factory: IClassFactory = ClassFactory { class }.into();
    // SAFETY: The factory is live, riid is readable and out is a writable interface slot.
    unsafe { factory.query(riid, out.as_mut_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_lock_is_reentrant() {
        assert_eq!(AcquireDDThreadLock(), 0);
        assert_eq!(AcquireDDThreadLock(), 0);
        assert_eq!(ReleaseDDThreadLock(), 0);
        assert!(!std::thread::spawn(|| DD_THREAD_LOCK.try_lock().is_some())
            .join()
            .unwrap());
        assert_eq!(ReleaseDDThreadLock(), 0);
        assert!(std::thread::spawn(|| DD_THREAD_LOCK.try_lock().is_some())
            .join()
            .unwrap());
    }

    #[test]
    fn zoom_and_windowed_helpers_succeed() {
        assert_eq!(DDIsWindowed(), BOOL(1));
        DDEnableZoom();
        assert!(ZOOM_ENABLED.load(Ordering::Relaxed));
    }
}
