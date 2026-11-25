use std::sync::{Arc, Mutex, Weak};
use windows::core::{implement, IUnknownImpl, Interface, Ref, Result, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_POINTER};
use windows::Win32::Graphics::DirectDraw::{
    IDirectDraw, IDirectDrawPalette, IDirectDrawPalette_Impl, DDPCAPS_8BIT, DDPCAPS_ALLOW256,
    DDPCAPS_INITIALIZE, DDPCAPS_PRIMARYSURFACE, DDPCAPS_VSYNC,
};
use windows::Win32::Graphics::Gdi::PALETTEENTRY;

use crate::draw::DisplayShared;
use crate::ffi::ptr as ffi_ptr;

#[implement(IDirectDrawPalette, Agile = false)]
struct Palette {
    flags: u32,
    entries: Mutex<[PALETTEENTRY; 256]>,
    display: Weak<DisplayShared>,
}

/// # Safety
/// For supported flags, a non-null entries pointer must address 256 initialized palette entries.
pub unsafe fn create_palette(
    flags: u32,
    entries: *const PALETTEENTRY,
    display: Arc<DisplayShared>,
) -> Result<IDirectDrawPalette> {
    let allowed = (DDPCAPS_8BIT
        | DDPCAPS_ALLOW256
        | DDPCAPS_INITIALIZE
        | DDPCAPS_PRIMARYSURFACE
        | DDPCAPS_VSYNC) as u32;
    if flags & DDPCAPS_8BIT as u32 == 0 || flags & !allowed != 0 {
        return Err(E_NOTIMPL.into());
    }
    // SAFETY: Caller guarantees `entries` addresses 256 initialized `PALETTEENTRY` items when non-null.
    let mut colors =
        *unsafe { ffi_ptr::req_ref(entries.cast::<[PALETTEENTRY; 256]>(), E_POINTER)? };
    preserve_endpoints(flags, &mut colors);
    Ok(Palette {
        flags,
        entries: Mutex::new(colors),
        display: Arc::downgrade(&display),
    }
    .into())
}

pub fn check_owner(palette: &IDirectDrawPalette, display: &Arc<DisplayShared>) -> Result<()> {
    let object = palette.cast_object_ref::<Palette>()?;
    if object.display.as_ptr() == Arc::as_ptr(display) {
        Ok(())
    } else {
        Err(E_INVALIDARG.into())
    }
}

pub(crate) fn snapshot(palette: &IDirectDrawPalette) -> Result<[PALETTEENTRY; 256]> {
    let object = palette.cast_object_ref::<Palette>()?;
    let colors = object.entries.lock().unwrap_or_else(|e| e.into_inner());
    Ok(*colors)
}

fn preserve_endpoints(flags: u32, colors: &mut [PALETTEENTRY; 256]) {
    if flags & DDPCAPS_ALLOW256 as u32 == 0 {
        colors[0] = PALETTEENTRY::default();
        colors[255] = PALETTEENTRY {
            peRed: 255,
            peGreen: 255,
            peBlue: 255,
            peFlags: 0,
        };
    }
}

fn range(flags: u32, start: u32, count: u32) -> Result<std::ops::Range<usize>> {
    let end = start.checked_add(count).filter(|end| *end <= 256);
    match end {
        Some(end) if flags == 0 => Ok(start as usize..end as usize),
        _ => Err(E_INVALIDARG.into()),
    }
}

impl IDirectDrawPalette_Impl for Palette_Impl {
    fn GetCaps(&self, out: *mut u32) -> Result<()> {
        // SAFETY: Caller supplies a writable `u32` output slot per COM GetCaps.
        unsafe { ffi_ptr::write_out(out, self.flags, E_POINTER) }
    }

    fn GetEntries(&self, flags: u32, start: u32, count: u32, out: *mut PALETTEENTRY) -> Result<()> {
        let span = range(flags, start, count)?;
        if span.is_empty() {
            return Ok(());
        }
        if out.is_null() {
            return Err(E_POINTER.into());
        }
        let colors = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: `out` is non-null and writable for `count` `PALETTEENTRY` items; `range()` bounds `span`.
        unsafe { std::ptr::copy_nonoverlapping(colors[span].as_ptr(), out, count as usize) };
        Ok(())
    }

    fn SetEntries(&self, flags: u32, start: u32, count: u32, src: *mut PALETTEENTRY) -> Result<()> {
        let span = range(flags, start, count)?;
        if span.is_empty() {
            return Ok(());
        }
        if src.is_null() {
            return Err(E_POINTER.into());
        }
        {
            let mut colors = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: SetEntries supplies count readable entries. range() bounds count to 256.
            colors[span]
                .copy_from_slice(unsafe { std::slice::from_raw_parts(src, count as usize) });
            preserve_endpoints(self.flags, &mut colors);
        }
        if let Some(display) = self.display.upgrade() {
            display.refresh_if_primary_uses_palette(&self.as_interface::<IDirectDrawPalette>())?;
        }
        Ok(())
    }

    fn Initialize(&self, _: Ref<IDirectDraw>, _: u32, _: *mut PALETTEENTRY) -> Result<()> {
        // DDERR_ALREADYINITIALIZED
        Err(HRESULT(0x8876_0005u32 as i32).into())
    }
}
