//! Process, module, and thread enumeration/affinity primitives.
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::core::{s, w, Owned};
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, OpenThread, SetThreadAffinityMask,
    THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
};

const THREAD_QUERY_SET_WIN32_START_ADDRESS: u32 = 9;

type NtQueryInformationThreadFn =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32, *mut u32) -> i32;

static NT_QUERY_THREAD: OnceLock<Option<NtQueryInformationThreadFn>> = OnceLock::new();

pub(crate) fn current_process_id() -> u32 {
    // SAFETY: GetCurrentProcessId takes no arguments and returns an integer.
    unsafe { GetCurrentProcessId() }
}

pub(crate) fn current_thread_id() -> u32 {
    // SAFETY: GetCurrentThreadId takes no arguments and returns an integer.
    unsafe { GetCurrentThreadId() }
}

pub(crate) fn module_path_at(address: *const c_void) -> Option<PathBuf> {
    let mut module = HMODULE::default();
    // SAFETY: FROM_ADDRESS makes this an address lookup, with no string dereference.
    // The borrowed module handle is used only as an identifier for the path query.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(address.cast()),
            &mut module,
        )
    }
    .ok()?;
    let mut buf = [0u16; 1024];
    // SAFETY: The binding passes the slice length and Win32 writes at most that many units.
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    (len > 0 && len < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..len])))
}

fn nt_query_information_thread() -> Option<NtQueryInformationThreadFn> {
    *NT_QUERY_THREAD.get_or_init(|| {
        // SAFETY: `ntdll.dll` is always loaded; `NtQueryInformationThread` has the documented
        // stdcall signature matching `NtQueryInformationThreadFn`.
        unsafe {
            let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
            let proc = GetProcAddress(ntdll, s!("NtQueryInformationThread"))?;
            Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                NtQueryInformationThreadFn,
            >(proc))
        }
    })
}

/// Resolves `NtQueryInformationThread` outside the loader lock before `DLL_THREAD_ATTACH` runs.
pub(crate) fn prewarm_thread_query() {
    let _ = nt_query_information_thread();
}

pub(crate) fn pin_current_thread_to_cpu0() {
    // SAFETY: `GetCurrentThread()` returns a valid pseudo-handle for the calling thread.
    unsafe {
        let _ = SetThreadAffinityMask(GetCurrentThread(), 1);
    }
}

fn query_thread_start_address(thread: HANDLE) -> Option<Option<*const c_void>> {
    let query_fn = nt_query_information_thread()?;
    let mut start: *mut c_void = std::ptr::null_mut();
    // SAFETY: `start` is a valid pointer-sized output buffer matching the length passed.
    let status = unsafe {
        query_fn(
            thread.0,
            THREAD_QUERY_SET_WIN32_START_ADDRESS,
            (&raw mut start).cast(),
            std::mem::size_of::<*mut c_void>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status != 0 || start.is_null() {
        Some(None)
    } else {
        Some(Some(start.cast_const()))
    }
}

/// Opens thread `tid`, queries its Win32 start address, and pins it to CPU0 if `should_pin`
/// returns true (passing `None` if `NtQueryInformationThread` is unavailable).
pub(crate) fn pin_thread_to_cpu0_if(
    tid: u32,
    should_pin: impl FnOnce(Option<Option<*const c_void>>) -> bool,
) {
    // SAFETY: A successful OpenThread transfers a fresh handle to Owned.
    let Ok(thread) = (unsafe {
        OpenThread(
            THREAD_QUERY_INFORMATION | THREAD_SET_INFORMATION,
            false,
            tid,
        )
        .map(|handle| Owned::new(handle))
    }) else {
        return;
    };
    if should_pin(query_thread_start_address(*thread)) {
        // SAFETY: `*thread` is an open handle with `THREAD_SET_INFORMATION`.
        unsafe {
            let _ = SetThreadAffinityMask(*thread, 1);
        }
    }
}

/// Returns the thread IDs currently belonging to `pid`.
pub(crate) fn process_thread_ids(pid: u32) -> Vec<u32> {
    let mut tids = Vec::new();
    // SAFETY: This snapshot handle is owned exclusively by this invocation.
    let Ok(snap) =
        (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).map(|h| Owned::new(h)) })
    else {
        return tids;
    };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry.dwSize` is initialized and `*snap` is a valid ToolHelp snapshot handle.
    if unsafe { Thread32First(*snap, &mut entry) }.is_ok() {
        loop {
            if entry.th32OwnerProcessID == pid {
                tids.push(entry.th32ThreadID);
            }
            // SAFETY: Continuing enumeration on the same valid snapshot handle and entry buffer.
            if unsafe { Thread32Next(*snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    tids
}
