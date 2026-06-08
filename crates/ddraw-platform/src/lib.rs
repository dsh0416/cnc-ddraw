//! Windows 10+ platform layer.

use std::ffi::c_void;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleHandle(pub *mut c_void);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowHandle(pub isize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceHandle(pub isize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DllReason {
    ProcessDetach,
    ProcessAttach,
    ThreadAttach,
    ThreadDetach,
    Unknown(u32),
}

impl From<u32> for DllReason {
    fn from(value: u32) -> Self {
        match value {
            0 => Self::ProcessDetach,
            1 => Self::ProcessAttach,
            2 => Self::ThreadAttach,
            3 => Self::ThreadDetach,
            other => Self::Unknown(other),
        }
    }
}

pub fn initialize_process() {
    set_dpi_awareness();
    keep_display_awake();
}

pub fn shutdown_process() {
    restore_execution_state();
}

pub fn trace(message: &str) {
    debug_output(message);
}

pub unsafe fn replace_pointer(
    slot: *mut *mut c_void,
    replacement: *mut c_void,
) -> Option<*mut c_void> {
    if slot.is_null() {
        return None;
    }

    replace_pointer_impl(slot, replacement)
}

pub fn is_wine() -> bool {
    cfg!(target_os = "windows") && wine::is_wine()
}

#[cfg(windows)]
pub fn get_proc_address(module: *mut c_void, name: *const u8) -> *mut c_void {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::GetProcAddress;

    // SAFETY: The caller provides a module handle and a NUL-terminated ANSI name
    // matching the Win32 GetProcAddress contract.
    unsafe {
        GetProcAddress(HMODULE(module as isize), windows::core::PCSTR(name))
            .map(|proc| proc as *mut c_void)
            .unwrap_or(std::ptr::null_mut())
    }
}

#[cfg(not(windows))]
pub fn get_proc_address(_module: *mut c_void, _name: *const u8) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(windows)]
fn set_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };

    // SAFETY: Process-wide DPI setup is intentionally performed once during DLL
    // process attach. Windows 10+ guarantees this API is present.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

#[cfg(not(windows))]
fn set_dpi_awareness() {}

#[cfg(windows)]
fn keep_display_awake() {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED,
    };

    // SAFETY: SetThreadExecutionState has no pointer arguments and is valid during
    // process attach on Windows 10+.
    let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED) };
}

#[cfg(not(windows))]
fn keep_display_awake() {}

#[cfg(windows)]
fn restore_execution_state() {
    use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};

    // SAFETY: Restores process thread execution state during detach.
    let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
}

#[cfg(not(windows))]
fn restore_execution_state() {}

#[cfg(windows)]
fn debug_output(message: &str) {
    use windows::Win32::System::Diagnostics::Debug::OutputDebugStringA;

    let mut bytes = Vec::with_capacity(message.len() + 2);
    bytes.extend_from_slice(message.as_bytes());
    if !message.ends_with('\n') {
        bytes.push(b'\n');
    }
    bytes.push(0);

    // SAFETY: The buffer is explicitly NUL-terminated and lives for the duration
    // of the call.
    unsafe {
        OutputDebugStringA(windows::core::PCSTR(bytes.as_ptr()));
    }
}

#[cfg(not(windows))]
fn debug_output(message: &str) {
    eprintln!("{message}");
}

#[cfg(windows)]
fn replace_pointer_impl(slot: *mut *mut c_void, replacement: *mut c_void) -> Option<*mut c_void> {
    use windows::Win32::System::Memory::{
        VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
    };

    let mut old_protect = PAGE_PROTECTION_FLAGS(0);
    let size = std::mem::size_of::<*mut c_void>();

    // SAFETY: The caller guarantees slot is writable code/data memory belonging
    // to the target vtable/IAT. Protection is restored after the pointer write.
    unsafe {
        if VirtualProtect(slot.cast(), size, PAGE_EXECUTE_READWRITE, &mut old_protect).is_err() {
            return None;
        }

        let old = *slot;
        *slot = replacement;

        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(slot.cast(), size, old_protect, &mut ignored);
        Some(old)
    }
}

#[cfg(not(windows))]
fn replace_pointer_impl(slot: *mut *mut c_void, replacement: *mut c_void) -> Option<*mut c_void> {
    // Non-Windows builds only run unit tests; perform a plain pointer write so
    // vtable patching logic remains testable.
    unsafe {
        let old = *slot;
        *slot = replacement;
        Some(old)
    }
}

mod wine {
    #[cfg(windows)]
    pub fn is_wine() -> bool {
        use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

        // SAFETY: ntdll is always loaded in a normal Windows process; missing
        // wine_get_version simply means native Windows.
        unsafe {
            let Ok(module) = GetModuleHandleA(windows::core::s!("ntdll.dll")) else {
                return false;
            };

            GetProcAddress(module, windows::core::s!("wine_get_version")).is_some()
        }
    }

    #[cfg(not(windows))]
    pub fn is_wine() -> bool {
        false
    }
}
