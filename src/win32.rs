//! Process and thread queries shared by the DLL and its configuration loader.
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

pub(crate) fn current_thread_id() -> u32 {
    // SAFETY: This query takes no arguments and returns an integer.
    unsafe { GetCurrentThreadId() }
}

pub(crate) fn module_path_at(address: *const c_void) -> Option<PathBuf> {
    let mut module = HMODULE::default();
    // SAFETY: FROM_ADDRESS makes this an address lookup, with no string dereference.
    // The borrowed module handle is used only as an identifier for the path query.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address.cast()),
            &mut module,
        )
    }
    .ok()?;
    let mut buf = [0u16; 1024];
    // SAFETY: The binding passes the slice length and Win32 writes at most that many units.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    (len > 0 && len < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
}
