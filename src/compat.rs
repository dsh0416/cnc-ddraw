//! Compatibility helpers: Win11-24H2-safe per-game-thread CPU0 affinity (`singlecpu`)
//! and optional opt-in diagnostic logging (`CNC_DDRAW_LOG` or `ddraw.ini` `log=true`).
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{s, w, Owned};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, OpenThread, SetThreadAffinityMask,
    THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
};

use crate::config;
use crate::win32::{current_thread_id, module_path_at};

const THREAD_QUERY_SET_WIN32_START_ADDRESS: u32 = 9;

type NtQueryInformationThreadFn =
    unsafe extern "system" fn(*mut c_void, u32, *mut c_void, u32, *mut u32) -> i32;

static SINGLECPU_ACTIVE: AtomicBool = AtomicBool::new(false);
static GAME_DIR_LOWER: OnceLock<Option<String>> = OnceLock::new();
static NT_QUERY_THREAD: OnceLock<Option<NtQueryInformationThreadFn>> = OnceLock::new();
static LOG_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn game_dir_lower() -> Option<&'static str> {
    GAME_DIR_LOWER
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .map(|p| p.to_string_lossy().to_ascii_lowercase())
        })
        .as_deref()
}

fn nt_query_information_thread() -> Option<NtQueryInformationThreadFn> {
    *NT_QUERY_THREAD.get_or_init(|| unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let proc = GetProcAddress(ntdll, s!("NtQueryInformationThread"))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            NtQueryInformationThreadFn,
        >(proc))
    })
}

fn should_pin_thread_handle(thread: windows::Win32::Foundation::HANDLE) -> bool {
    let Some(query_fn) = nt_query_information_thread() else {
        return true;
    };
    let Some(game_dir) = game_dir_lower() else {
        return true;
    };
    let mut start: *mut c_void = std::ptr::null_mut();
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
        return false;
    }
    let Some(mod_dir) = module_path_at(start)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|p| p.to_string_lossy().to_ascii_lowercase())
    else {
        return false;
    };
    mod_dir.starts_with(game_dir)
}

fn pin_thread_by_id(tid: u32) {
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
    if should_pin_thread_handle(*thread) {
        unsafe {
            let _ = SetThreadAffinityMask(*thread, 1);
        }
    }
}

/// Pins the current game thread and all threads originating from modules in the game directory
/// to CPU0 while leaving GPU driver / wgpu worker threads free to run on all cores.
pub fn apply_single_cpu_affinity() {
    if !config::get().singlecpu {
        return;
    }
    // Always pin the calling (game UI/DirectDraw) thread to CPU0.
    unsafe {
        let _ = SetThreadAffinityMask(GetCurrentThread(), 1);
    }
    if SINGLECPU_ACTIVE.swap(true, Ordering::Relaxed) {
        return;
    }
    // Pre-initialize OnceLocks outside DllMain loader lock before enabling DLL_THREAD_ATTACH.
    let _ = game_dir_lower();
    let _ = nt_query_information_thread();

    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: This snapshot is owned by this invocation.
    let Ok(snap) =
        (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).map(|h| Owned::new(h)) })
    else {
        return;
    };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    if unsafe { Thread32First(*snap, &mut entry) }.is_ok() {
        loop {
            if entry.th32OwnerProcessID == pid {
                pin_thread_by_id(entry.th32ThreadID);
            }
            if unsafe { Thread32Next(*snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    log_msg("singlecpu affinity enabled for game threads");
}

/// Called from `DllMain(DLL_THREAD_ATTACH)` once single-CPU mode has been activated.
pub fn on_thread_attach() {
    if !SINGLECPU_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    pin_thread_by_id(current_thread_id());
}

fn open_log_file() -> Option<Mutex<std::fs::File>> {
    let path = if let Some(env_path) = std::env::var_os("CNC_DDRAW_LOG") {
        let p = PathBuf::from(env_path);
        if p.as_os_str().is_empty() {
            return None;
        }
        p
    } else if config::get().log {
        config::dll_or_exe_directory()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cnc-ddraw.log")
    } else {
        return None;
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
        .map(Mutex::new)
}

pub fn log_msg(msg: &str) {
    let Some(file_mutex) = LOG_FILE.get_or_init(open_log_file).as_ref() else {
        return;
    };
    if let Ok(mut file) = file_mutex.lock() {
        let tid = current_thread_id();
        let _ = writeln!(file, "[tid={tid}] {msg}");
        let _ = file.flush();
    }
}
