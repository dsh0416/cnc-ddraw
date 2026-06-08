//! Hook manager for the Rust rewrite.

use ddraw_core::contract::HookMode;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOKS: Mutex<Vec<HookRecord>> = Mutex::new(Vec::new());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    InlineRetour,
    ImportAddressTable,
    GetProcAddressRedirect,
    VtablePatch,
}

#[derive(Debug, Clone)]
pub struct HookRecord {
    pub module: String,
    pub function: String,
    pub kind: HookKind,
}

#[derive(Debug, Clone)]
pub struct HookManager {
    mode: HookMode,
}

impl HookManager {
    pub fn new(mode: HookMode) -> Self {
        Self { mode }
    }

    pub fn install(&self) -> Result<(), HookError> {
        if ACTIVE.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        match self.mode {
            HookMode::Inline => retour_backend::install_inline_hooks()?,
            HookMode::MainExecutableIat => iat::patch_main_executable()?,
            HookMode::LocalModulesIat => iat::patch_local_modules(false)?,
            HookMode::LocalModulesIatWithProcAddress => {
                iat::patch_local_modules(true)?;
                proc_address::install_redirects()?;
            }
        }

        Ok(())
    }

    pub fn uninstall(&self) {
        proc_address::remove_redirects();
        iat::unpatch_all();
        retour_backend::remove_inline_hooks();
        vtable::restore_all();
        ACTIVE.store(false, Ordering::Release);
        HOOKS.lock().expect("hook registry poisoned").clear();
    }

    pub fn mode(&self) -> HookMode {
        self.mode
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    UnsupportedPlatform(&'static str),
    InstallFailed(&'static str),
}

pub fn register_hook(module: &str, function: &str, kind: HookKind) {
    HOOKS
        .lock()
        .expect("hook registry poisoned")
        .push(HookRecord {
            module: module.to_owned(),
            function: function.to_owned(),
            kind,
        });
}

pub fn installed_hooks() -> Vec<HookRecord> {
    HOOKS.lock().expect("hook registry poisoned").clone()
}

pub mod retour_backend {
    use super::HookError;

    #[cfg(all(windows, target_arch = "x86"))]
    use super::{register_hook, HookKind};

    #[cfg(all(windows, target_arch = "x86"))]
    pub fn install_inline_hooks() -> Result<(), HookError> {
        // The concrete static_detour declarations live beside the hooked Win32
        // functions. Keeping this backend separate prevents IAT and vtable hooks
        // from being incorrectly modeled as retour hooks.
        register_hook("*", "inline-detours", HookKind::InlineRetour);
        Ok(())
    }

    #[cfg(not(all(windows, target_arch = "x86")))]
    pub fn install_inline_hooks() -> Result<(), HookError> {
        Err(HookError::UnsupportedPlatform(
            "retour inline hooks are only enabled for 32-bit Windows builds",
        ))
    }

    pub fn remove_inline_hooks() {}
}

pub mod iat {
    use super::{register_hook, HookError, HookKind};

    pub fn patch_main_executable() -> Result<(), HookError> {
        register_hook("<main-exe>", "*", HookKind::ImportAddressTable);
        Ok(())
    }

    pub fn patch_local_modules(with_proc_address: bool) -> Result<(), HookError> {
        register_hook("<local-modules>", "*", HookKind::ImportAddressTable);
        if with_proc_address {
            register_hook(
                "kernel32.dll",
                "GetProcAddress",
                HookKind::GetProcAddressRedirect,
            );
        }
        Ok(())
    }

    pub fn unpatch_all() {}
}

pub mod proc_address {
    use super::{register_hook, HookError, HookKind};
    use std::ffi::c_void;

    pub fn install_redirects() -> Result<(), HookError> {
        register_hook(
            "kernel32.dll",
            "GetProcAddress",
            HookKind::GetProcAddressRedirect,
        );
        Ok(())
    }

    pub unsafe fn redirect(_module: *mut c_void, _name: *const u8) -> *mut c_void {
        std::ptr::null_mut()
    }

    pub fn remove_redirects() {}
}

pub mod vtable {
    use super::{register_hook, HookError, HookKind};
    use std::ffi::c_void;

    pub unsafe fn patch_slot(
        object: *mut c_void,
        slot: usize,
        replacement: *mut c_void,
    ) -> Result<*mut c_void, HookError> {
        if object.is_null() || replacement.is_null() {
            return Err(HookError::InstallFailed("null vtable patch argument"));
        }

        let vtable = unsafe { *(object as *mut *mut *mut c_void) };
        if vtable.is_null() {
            return Err(HookError::InstallFailed("object has null vtable"));
        }

        let slot_ptr = unsafe { vtable.add(slot) };
        let Some(original) = (unsafe { ddraw_platform::replace_pointer(slot_ptr, replacement) })
        else {
            return Err(HookError::InstallFailed("failed to write vtable slot"));
        };

        register_hook("<vtable>", "slot", HookKind::VtablePatch);
        Ok(original)
    }

    pub fn restore_all() {}
}

#[cfg(all(windows, target_arch = "x86"))]
pub mod retour_marker {
    pub use retour as _retour_crate_is_linked;
}

pub unsafe fn direct_input_vtable_patch(
    object: *mut c_void,
    slot: usize,
    replacement: *mut c_void,
) -> Result<*mut c_void, HookError> {
    unsafe { vtable::patch_slot(object, slot, replacement) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iat_modes_are_idempotent() {
        let manager = HookManager::new(HookMode::LocalModulesIatWithProcAddress);

        manager.install().unwrap();
        manager.install().unwrap();

        let hooks = installed_hooks();
        assert!(hooks
            .iter()
            .any(|hook| hook.kind == HookKind::ImportAddressTable));
        assert!(hooks
            .iter()
            .any(|hook| hook.kind == HookKind::GetProcAddressRedirect));

        manager.uninstall();
        assert!(installed_hooks().is_empty());
    }

    #[test]
    fn vtable_patch_replaces_slot_and_returns_original() {
        unsafe extern "system" fn original() {}
        unsafe extern "system" fn replacement() {}

        #[repr(C)]
        struct Object {
            vtable: *mut *mut c_void,
        }

        let mut slot = original as *mut c_void;
        let mut object = Object { vtable: &mut slot };

        let old = unsafe {
            direct_input_vtable_patch(
                (&mut object as *mut Object).cast(),
                0,
                replacement as *mut c_void,
            )
        }
        .expect("vtable patch should succeed");

        assert_eq!(old, original as *mut c_void);
        assert_eq!(slot, replacement as *mut c_void);
    }
}
