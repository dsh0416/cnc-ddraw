//! PE32 Import Address Table (IAT) patching and restoration for the current executable image.
use std::ffi::CStr;
use std::sync::Mutex;

use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};

use super::current_module_handle;

struct PatchedSlot {
    slot: usize,
    original: usize,
}

static IAT_PATCHES: Mutex<Vec<PatchedSlot>> = Mutex::new(Vec::new());

#[repr(C)]
struct DosHeader {
    e_magic: u16,
    _pad: [u8; 58],
    e_lfanew: i32,
}

#[repr(C)]
struct DataDirectory {
    virtual_address: u32,
    size: u32,
}

#[repr(C)]
struct OptionalHeader32Prefix {
    magic: u16,
    _pad: [u8; 90],
    number_of_rva_and_sizes: u32,
    data_directory: [DataDirectory; 16],
}

#[repr(C)]
struct NtHeaders32 {
    signature: u32,
    _file_header: [u8; 20],
    optional: OptionalHeader32Prefix,
}

#[repr(C)]
struct ImportDescriptor {
    original_first_thunk: u32,
    time_date_stamp: u32,
    forwarder_chain: u32,
    name: u32,
    first_thunk: u32,
}

/// Replaces one IAT slot's function pointer using `VirtualProtect` and returns the prior entry.
///
/// # Safety
/// `slot` must point to a valid, aligned IAT entry in the current process image.
unsafe fn write_iat_slot(slot: *mut usize, value: usize) -> Option<usize> {
    let mut old_prot = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: `slot` is a single pointer-sized IAT entry; we restore its original page protection.
    unsafe {
        VirtualProtect(
            slot.cast(),
            std::mem::size_of::<usize>(),
            PAGE_READWRITE,
            &mut old_prot,
        )
        .ok()?;
        let original = *slot;
        if original != value {
            *slot = value;
        }
        let mut _ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(
            slot.cast(),
            std::mem::size_of::<usize>(),
            old_prot,
            &mut _ignored,
        );
        Some(original)
    }
}

/// Installs IAT hooks on the main PE32 module for each `(dll_name, fn_name, replacement)` target.
///
/// # Safety
/// Every replacement address must be a valid `extern "system"` function pointer matching the ABI
/// and signature of the targeted Win32 import, remaining valid until `uninstall` runs.
pub(crate) unsafe fn install(targets: &[(&str, &str, usize)]) {
    let Ok(mut patches) = IAT_PATCHES.lock() else {
        return;
    };
    if !patches.is_empty() {
        return;
    }
    let Ok(module) = current_module_handle() else {
        return;
    };
    let base = module.0 as usize;
    if base == 0 {
        return;
    }

    // SAFETY: `base` is the live PE32 image mapped by the Windows loader for the main executable.
    // Headers and import descriptors are validated before dereferencing RVAs.
    unsafe {
        let dos = &*(base as *const DosHeader);
        if dos.e_magic != 0x5A4D || dos.e_lfanew <= 0 {
            return;
        }
        let nt = &*((base + dos.e_lfanew as usize) as *const NtHeaders32);
        if nt.signature != 0x0000_4550 || nt.optional.magic != 0x010B {
            return;
        }
        if nt.optional.number_of_rva_and_sizes <= 1 {
            return;
        }
        let import_rva = nt.optional.data_directory[1].virtual_address as usize;
        if import_rva == 0 {
            return;
        }
        let mut desc = (base + import_rva) as *const ImportDescriptor;
        while (*desc).name != 0 {
            let dll_name =
                CStr::from_ptr((base + (*desc).name as usize) as *const i8).to_string_lossy();
            let has_dll = targets
                .iter()
                .any(|(d, _, _)| dll_name.eq_ignore_ascii_case(d));
            if has_dll && (*desc).original_first_thunk != 0 && (*desc).first_thunk != 0 {
                let mut orig_thunk = (base + (*desc).original_first_thunk as usize) as *const u32;
                let mut iat_thunk = (base + (*desc).first_thunk as usize) as *mut usize;
                while *orig_thunk != 0 {
                    // Skip ordinal-only imports (bit 31 set on PE32).
                    if *orig_thunk & 0x8000_0000 == 0 {
                        let name_ptr = (base + *orig_thunk as usize + 2) as *const i8;
                        let fn_name = CStr::from_ptr(name_ptr).to_string_lossy();
                        if let Some(&(_, _, replacement)) = targets
                            .iter()
                            .find(|(d, f, _)| dll_name.eq_ignore_ascii_case(d) && fn_name == *f)
                        {
                            if let Some(original) = write_iat_slot(iat_thunk, replacement) {
                                if original != replacement {
                                    patches.push(PatchedSlot {
                                        slot: iat_thunk as usize,
                                        original,
                                    });
                                }
                            }
                        }
                    }
                    orig_thunk = orig_thunk.add(1);
                    iat_thunk = iat_thunk.add(1);
                }
            }
            desc = desc.add(1);
        }
    }
}

/// Restores all IAT slots previously modified by `install`.
pub(crate) fn uninstall() {
    let Ok(mut patches) = IAT_PATCHES.lock() else {
        return;
    };
    for patch in patches.drain(..) {
        // SAFETY: `patch.slot` was recorded from the main module's IAT during `install`.
        unsafe {
            let _ = write_iat_slot(patch.slot as *mut usize, patch.original);
        }
    }
}
