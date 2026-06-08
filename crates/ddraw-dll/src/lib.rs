#![allow(non_snake_case)]

use ddraw_com::{create_clipper, create_direct_draw, DirectDrawVersion, HResult, E_FAIL, S_OK};
use ddraw_core::contract::HookMode;
use ddraw_hooks::HookManager;
use ddraw_platform::DllReason;
use ddraw_render::{Renderer, RendererKind};
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::{Mutex, OnceLock};

type Dword = u32;
type Bool = i32;
type HModule = *mut c_void;
type HResultCallbackA =
    unsafe extern "system" fn(*mut c_void, *const c_char, *const c_char, *mut c_void) -> HResult;
type HResultCallbackW =
    unsafe extern "system" fn(*mut c_void, *const u16, *const u16, *mut c_void) -> HResult;
type HResultCallbackExA = unsafe extern "system" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *mut c_void,
    HModule,
) -> HResult;
type HResultCallbackExW =
    unsafe extern "system" fn(*mut c_void, *const u16, *const u16, *mut c_void, HModule) -> HResult;

static HOOK_MANAGER: OnceLock<Mutex<HookManager>> = OnceLock::new();

fn create_renderer(kind: RendererKind) -> Box<dyn Renderer + Send> {
    match kind {
        RendererKind::Gdi => Box::<ddraw_render_gdi::GdiRenderer>::default(),
        RendererKind::OpenGl => Box::<ddraw_render_opengl::OpenGlRenderer>::default(),
        RendererKind::D3d9 | RendererKind::Auto => {
            Box::<ddraw_render_d3d9::D3d9Renderer>::default()
        }
        RendererKind::D3d11 | RendererKind::D3d12 | RendererKind::Vulkan => {
            Box::<ddraw_render_d3d9::D3d9Renderer>::default()
        }
    }
}

#[no_mangle]
pub static mut GameHandlesClose: Bool = 0;

#[no_mangle]
pub static mut FakePrimarySurface: *mut c_void = ptr::null_mut();

#[no_mangle]
pub extern "system" fn DllMain(_module: HModule, reason: Dword, _reserved: *mut c_void) -> Bool {
    match DllReason::from(reason) {
        DllReason::ProcessAttach => {
            ddraw_platform::initialize_process();
            let manager = HOOK_MANAGER.get_or_init(|| {
                Mutex::new(HookManager::new(HookMode::LocalModulesIatWithProcAddress))
            });
            let _ = manager.lock().expect("hook manager poisoned").install();
        }
        DllReason::ProcessDetach => {
            if let Some(manager) = HOOK_MANAGER.get() {
                manager.lock().expect("hook manager poisoned").uninstall();
            }
            ddraw_platform::shutdown_process();
        }
        DllReason::ThreadAttach | DllReason::ThreadDetach | DllReason::Unknown(_) => {}
    }

    1
}

#[no_mangle]
pub extern "system" fn DDEnableZoom() {}

#[no_mangle]
pub extern "system" fn DDIsWindowed() -> Bool {
    0
}

#[no_mangle]
pub unsafe extern "system" fn DDGetProcAddress(
    module: HModule,
    proc_name: *const c_char,
) -> *mut c_void {
    ddraw_platform::get_proc_address(module, proc_name.cast())
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawCreate(
    _guid: *mut ddraw_com::Guid,
    out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    unsafe { create_direct_draw(DirectDrawVersion::V1, out) }
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawCreateEx(
    _guid: *mut ddraw_com::Guid,
    out: *mut *mut c_void,
    _iid: *const ddraw_com::Guid,
    _outer: *mut c_void,
) -> HResult {
    unsafe { create_direct_draw(DirectDrawVersion::V7, out) }
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawCreateClipper(
    _flags: Dword,
    out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    unsafe { create_clipper(out) }
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawEnumerateA(
    callback: Option<HResultCallbackA>,
    context: *mut c_void,
) -> HResult {
    static DRIVER_DESC: &[u8] = b"Primary Display Driver\0";
    static DRIVER_NAME: &[u8] = b"display\0";

    if let Some(callback) = callback {
        unsafe {
            callback(
                ptr::null_mut(),
                DRIVER_DESC.as_ptr().cast(),
                DRIVER_NAME.as_ptr().cast(),
                context,
            );
        }
    }

    S_OK
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawEnumerateW(
    callback: Option<HResultCallbackW>,
    context: *mut c_void,
) -> HResult {
    const DRIVER_DESC: &[u16] = &[
        80, 114, 105, 109, 97, 114, 121, 32, 68, 105, 115, 112, 108, 97, 121, 32, 68, 114, 105,
        118, 101, 114, 0,
    ];
    const DRIVER_NAME: &[u16] = &[100, 105, 115, 112, 108, 97, 121, 0];

    if let Some(callback) = callback {
        unsafe {
            callback(
                ptr::null_mut(),
                DRIVER_DESC.as_ptr(),
                DRIVER_NAME.as_ptr(),
                context,
            );
        }
    }

    S_OK
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawEnumerateExA(
    callback: Option<HResultCallbackExA>,
    context: *mut c_void,
    _flags: Dword,
) -> HResult {
    static DRIVER_DESC: &[u8] = b"Primary Display Driver\0";
    static DRIVER_NAME: &[u8] = b"display\0";

    if let Some(callback) = callback {
        unsafe {
            callback(
                ptr::null_mut(),
                DRIVER_DESC.as_ptr().cast(),
                DRIVER_NAME.as_ptr().cast(),
                context,
                ptr::null_mut(),
            );
        }
    }

    S_OK
}

#[no_mangle]
pub unsafe extern "system" fn DirectDrawEnumerateExW(
    callback: Option<HResultCallbackExW>,
    context: *mut c_void,
    _flags: Dword,
) -> HResult {
    const DRIVER_DESC: &[u16] = &[
        80, 114, 105, 109, 97, 114, 121, 32, 68, 105, 115, 112, 108, 97, 121, 32, 68, 114, 105,
        118, 101, 114, 0,
    ];
    const DRIVER_NAME: &[u16] = &[100, 105, 115, 112, 108, 97, 121, 0];

    if let Some(callback) = callback {
        unsafe {
            callback(
                ptr::null_mut(),
                DRIVER_DESC.as_ptr(),
                DRIVER_NAME.as_ptr(),
                context,
                ptr::null_mut(),
            );
        }
    }

    S_OK
}

#[no_mangle]
pub extern "system" fn CompleteCreateSysmemSurface(_a: Dword) -> Dword {
    0
}

#[no_mangle]
pub unsafe extern "system" fn D3DParseUnknownCommand(
    _cmd: *mut c_void,
    ret_cmd: *mut *mut c_void,
) -> HResult {
    if !ret_cmd.is_null() {
        unsafe {
            *ret_cmd = ptr::null_mut();
        }
    }
    E_FAIL
}

#[no_mangle]
pub extern "system" fn AcquireDDThreadLock() -> Dword {
    0
}

#[no_mangle]
pub extern "system" fn ReleaseDDThreadLock() -> Dword {
    0
}

#[no_mangle]
pub extern "system" fn DDInternalLock(_a: Dword, _b: Dword) -> Dword {
    0
}

#[no_mangle]
pub extern "system" fn DDInternalUnlock(_a: Dword) -> Dword {
    0
}

#[no_mangle]
pub extern "system" fn fake_DirectInputCreateA(
    _instance: HModule,
    _version: Dword,
    _out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    E_FAIL
}

#[no_mangle]
pub extern "system" fn fake_DirectInputCreateW(
    _instance: HModule,
    _version: Dword,
    _out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    E_FAIL
}

#[no_mangle]
pub extern "system" fn fake_DirectInputCreateEx(
    _instance: HModule,
    _version: Dword,
    _riid: *const ddraw_com::Guid,
    _out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    E_FAIL
}

#[no_mangle]
pub extern "system" fn fake_DirectInput8Create(
    _instance: HModule,
    _version: Dword,
    _riid: *const ddraw_com::Guid,
    _out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    E_FAIL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_factory_preserves_current_auto_preference() {
        assert_eq!(
            create_renderer(RendererKind::Auto).kind(),
            RendererKind::D3d9
        );
        assert_eq!(create_renderer(RendererKind::Gdi).kind(), RendererKind::Gdi);
        assert_eq!(
            create_renderer(RendererKind::OpenGl).kind(),
            RendererKind::OpenGl
        );
    }
}
