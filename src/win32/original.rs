//! Win32 calls made by the game can see virtual coordinates. Calls originating in other
//! modules, including our renderer, and explicit original calls use the physical APIs.
use std::ops::Range;
use std::sync::OnceLock;

use parking_lot::Mutex;
use retour::{Function, GenericDetour};
use windows::core::{Error, Owned, Result, BOOL, PCSTR, PCWSTR};
use windows::Win32::Foundation::{
    E_FAIL, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{DLGPROC, DLGTEMPLATE};

use super::hotpatch::HotpatchSite;
use crate::window::{bound_hwnd, hooks};

static GAME_IMAGE: OnceLock<Range<usize>> = OnceLock::new();
static INSTALL: Mutex<()> = Mutex::new(());

fn game_image() -> Result<Range<usize>> {
    let info = super::module_info(super::current_module_handle()?)?;
    let base = info.lpBaseOfDll as usize;
    Ok(base..base.checked_add(info.SizeOfImage as usize).ok_or(E_FAIL)?)
}

fn pin_image(address: *const ()) -> Result<()> {
    let mut module = HMODULE::default();
    // SAFETY: FROM_ADDRESS uses the address only to identify the containing DLL.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(address.cast()),
            &mut module,
        )
    }
}

fn flush_code() -> Result<()> {
    // SAFETY: A null address flushes the current process's instruction cache.
    unsafe { FlushInstructionCache(GetCurrentProcess(), None, 0) }
}

struct Prepared<F: Function> {
    module: Owned<HMODULE>,
    site: HotpatchSite,
    physical: GenericDetour<F>,
    padding: GenericDetour<F>,
}

struct Hook<F: Function> {
    site: HotpatchSite,
    physical: F,
    padding: GenericDetour<F>,
}

impl<F: Function> Hook<F> {
    fn prepare_padding(&self) -> Result<()> {
        if self.padding.is_enabled() {
            return Ok(());
        }
        self.site.verify_padding()?;
        // SAFETY: This detour writes only the five unreachable padding bytes.
        // The public entry still executes its original first instruction.
        unsafe { self.padding.enable() }.map_err(|e| Error::new(E_FAIL, e.to_string()))
    }
}

impl<F: Function> Prepared<F> {
    /// The resolved SDK export and dispatch must both have exactly F's ABI.
    unsafe fn new(
        module: PCWSTR,
        name: PCSTR,
        dispatch: F,
        regions: &mut Vec<Range<usize>>,
    ) -> Result<Option<Self>> {
        // SAFETY: The system DLL is already imported by ddraw, and both names are static strings.
        let target =
            unsafe { GetProcAddress(GetModuleHandleW(module)?, name) }.ok_or(E_FAIL)? as *const ();
        let mut owner = HMODULE::default();
        // SAFETY: Acquire a reference to the actual owner, which may be a forwarded export.
        let owner = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                PCWSTR(target.cast()),
                &mut owner,
            )?;
            Owned::new(owner)
        };
        // SAFETY: This is a live SDK export. Inspect checks the hotpatch layout
        // and readable span for retour's decoder before either detour is constructed.
        let Some(site) = (unsafe { HotpatchSite::inspect(target as usize)? }) else {
            return Ok(None);
        };
        let range = site.padding()..site.relocated_end();
        if regions
            .iter()
            .any(|other| other.start < range.end && range.start < other.end)
        {
            return Err(Error::new(E_FAIL, "Overlapping Win32 hotpatch sites"));
        }
        regions.push(range);
        let error = |e: retour::Error| Error::new(E_FAIL, e.to_string());
        // SAFETY: Both typed detours dispatch with F's ABI. The real entry is used only
        // to relocate its original instructions; only the reserved padding detour is enabled.
        unsafe {
            let physical = GenericDetour::new(F::from_ptr(target), dispatch).map_err(error)?;
            let padding = GenericDetour::new(F::from_ptr(site.padding() as *const ()), dispatch)
                .map_err(error)?;
            Ok(Some(Self {
                module: owner,
                site,
                physical,
                padding,
            }))
        }
    }

    fn commit(self) -> Hook<F> {
        // SAFETY: retour's trampoline has exactly the ABI of the original export.
        let physical = unsafe { F::from_ptr(self.physical.trampoline() as *const ()) };
        // Keep the executable trampoline alive for the process lifetime. Discard its
        // unused entry patcher before publishing the hotpatch; it must never edit that entry.
        std::mem::forget(self.physical);
        std::mem::forget(self.module);
        Hook {
            site: self.site,
            physical,
            padding: self.padding,
        }
    }
}

// Keep all callbacks in physical passthrough until the complete batch succeeds.
fn publish_sites(sites: &[(&str, Option<&HotpatchSite>, usize)]) -> Result<()> {
    let supported = || {
        sites
            .iter()
            .filter_map(|&(name, site, dispatch)| site.map(|s| (name, s, dispatch)))
    };
    flush_code()?;
    for (name, site, dispatch) in supported() {
        site.verify_prepared(dispatch)
            .map_err(|e| Error::new(e.code(), format!("{name}: {e}")))?;
    }
    for (name, site, _) in supported() {
        // SAFETY: All padding jumps and original trampolines have been verified and flushed.
        if let Err(error) = unsafe { site.publish() } {
            for (_, prepared, _) in supported().rev() {
                if let Err(rollback) = prepared.restore() {
                    crate::compat::log_msg(&format!("restoring Win32 entry: {rollback}"));
                }
            }
            return Err(Error::new(error.code(), format!("{name}: {error}")));
        }
    }
    crate::compat::log_msg(&format!(
        "Win32 hooks installed: {}/{}",
        supported().count(),
        sites.len()
    ));
    Ok(())
}

macro_rules! win32_hooks {
    ($( $dll:literal $api:ident($($arg:ident: $ty:ty),*) -> $ret:ty; )+) => {
        $(
            mod $api {
                use super::*;
                pub(super) type Proc = unsafe extern "system" fn($($ty),*) -> $ret;
                #[link(name = $dll)]
                unsafe extern "system" {
                    #[link_name = stringify!($api)]
                    pub(super) fn imported($($arg: $ty),*) -> $ret;
                }

                extern "system" fn select(caller: usize) -> Proc {
                    if GAME_IMAGE.get().is_some_and(|image| image.contains(&caller)) && bound_hwnd().is_some() {
                        hooks::$api
                    } else {
                        super::$api
                    }
                }

                // Capture the immediate caller before a Rust prologue can change the stack.
                // select pops its one stdcall argument. The tail call preserves the original
                // return address and every argument, including structures passed by value.
                #[unsafe(naked)]
                pub(super) unsafe extern "system" fn dispatch($(_: $ty),*) -> $ret {
                    core::arch::naked_asm!("push dword ptr [esp]", "call {select}", "jmp eax", select = sym select);
                }
            }

            pub(crate) unsafe extern "system" fn $api($($arg: $ty),*) -> $ret {
                let physical = HOOKS.get().and_then(|h| h.$api.as_ref())
                    .map_or($api::imported as $api::Proc, |hook| hook.physical);
                // SAFETY: The caller supplies the arguments required by this Win32 API.
                unsafe { physical($($arg),*) }
            }
        )+

        struct Hooks { $( $api: Option<Hook<$api::Proc>>, )+ }
        static HOOKS: OnceLock<Hooks> = OnceLock::new();

        impl Hooks {
            fn prepare() -> Result<Self> {
                let mut regions = Vec::new();
                $(
                    // SAFETY: The SDK declarations above define the ABI of these named Win32 exports.
                    let $api = unsafe {
                        Prepared::new(
                            windows::core::w!($dll),
                            PCSTR(concat!(stringify!($api), "\0").as_ptr()),
                            $api::dispatch as $api::Proc,
                            &mut regions,
                        )
                    }.map_err(|e| Error::new(e.code(), format!("{}: {e}", stringify!($api))))?;
                    if $api.is_none() {
                        crate::compat::log_msg(concat!(stringify!($api), ": no compatible Win32 hotpatch site; passing through"));
                    }
                )+
                // A modal call or another thread can outlive a binding and even FreeLibrary.
                if !regions.is_empty() {
                    pin_image(ensure_installed as *const ())?;
                    flush_code()?;
                }
                Ok(Self { $( $api: $api.map(Prepared::commit), )+ })
            }

            fn install(&self) -> Result<()> {
                $(
                    if let Some(hook) = &self.$api {
                        hook.prepare_padding().map_err(|e| Error::new(e.code(), format!("{}: {e}", stringify!($api))))?;
                    }
                )+
                publish_sites(&[$((
                    stringify!($api), self.$api.as_ref().map(|h| &h.site), $api::dispatch as *const () as usize
                )),+])
            }
        }
    };
}

win32_hooks! {
    "user32" GetClientRect(hwnd: HWND, rect: *mut RECT) -> BOOL;
    "user32" GetWindowRect(hwnd: HWND, rect: *mut RECT) -> BOOL;
    "user32" GetSystemMetrics(index: i32) -> i32;
    "user32" ClientToScreen(hwnd: HWND, pt: *mut POINT) -> BOOL;
    "user32" ScreenToClient(hwnd: HWND, pt: *mut POINT) -> BOOL;
    "user32" GetCursorPos(pt: *mut POINT) -> BOOL;
    "user32" SetCursorPos(x: i32, y: i32) -> BOOL;
    "user32" ClipCursor(rect: *const RECT) -> BOOL;
    "user32" GetClipCursor(rect: *mut RECT) -> BOOL;
    "user32" WindowFromPoint(point: POINT) -> HWND;
    "user32" SetWindowPos(hwnd: HWND, insert_after: HWND, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> BOOL;
    "user32" BringWindowToTop(hwnd: HWND) -> BOOL;
    "user32" SetForegroundWindow(hwnd: HWND) -> BOOL;
    "user32" MoveWindow(hwnd: HWND, x: i32, y: i32, width: i32, height: i32, repaint: BOOL) -> BOOL;
    "gdi32" GetDeviceCaps(hdc: HDC, index: i32) -> i32;
    "user32" SendMessageA(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    "user32" SendMessageW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
    "user32" CreateDialogParamA(hinst: HINSTANCE, template_name: PCSTR, parent: HWND, dlg_proc: DLGPROC, init_param: LPARAM) -> HWND;
    "user32" CreateDialogIndirectParamA(hinst: HINSTANCE, template: *const DLGTEMPLATE, parent: HWND, dlg_proc: DLGPROC, init_param: LPARAM) -> HWND;
    "user32" DialogBoxParamA(hinst: HINSTANCE, template_name: PCSTR, parent: HWND, dlg_proc: DLGPROC, init_param: LPARAM) -> isize;
    "user32" DialogBoxIndirectParamA(hinst: HINSTANCE, template: *const DLGTEMPLATE, parent: HWND, dlg_proc: DLGPROC, init_param: LPARAM) -> isize;
}

/// Install once, outside the loader lock. Bind/unbind only changes the virtualization policy.
pub(crate) fn ensure_installed() -> Result<()> {
    let _guard = INSTALL.lock();
    if GAME_IMAGE.get().is_some() {
        return Ok(());
    }
    let image = game_image()?;
    if HOOKS.get().is_none() {
        let hooks = Hooks::prepare()?;
        // Publish every original trampoline before internal callers can use any of them.
        let _ = HOOKS.set(hooks);
    }
    HOOKS
        .get()
        .expect("hooks prepared under install lock")
        .install()?;
    let _ = GAME_IMAGE.set(image);
    Ok(())
}
