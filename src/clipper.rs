//! DirectDraw clipper implementation backed by Win32 GDI regions.
#![allow(non_snake_case)]

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use windows::core::{implement, Interface, Owned, Ref, Result, BOOL, HRESULT};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_POINTER, HWND, RECT};
use windows::Win32::Graphics::DirectDraw::{
    IDirectDraw, IDirectDrawClipper, IDirectDrawClipper_Impl,
};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgnIndirect, GetRegionData, HRGN, RGNDATA, RGNDATAHEADER, RGN_AND,
    RGN_ERROR, RGN_OR,
};

use crate::draw::DisplayShared;
use crate::ffi::ptr as ffi_ptr;
use crate::win32::AtomicHwnd;
use crate::window::{client_rect, is_window};

const fn dd_hresult(code: u32) -> HRESULT {
    HRESULT((0x8876_0000 | code) as i32)
}

const DDERR_ALREADYINITIALIZED: HRESULT = dd_hresult(5);
const DDERR_INVALIDCLIPLIST: HRESULT = dd_hresult(110);
const DDERR_NOCLIPLIST: HRESULT = dd_hresult(205);
const DDERR_REGIONTOOSMALL: HRESULT = dd_hresult(566);
const DDERR_CLIPPERISUSINGHWND: HRESULT = dd_hresult(567);

struct OwnedRgn(Owned<HRGN>);

impl OwnedRgn {
    fn from_rect(rect: &RECT) -> Option<Self> {
        // SAFETY: `rect` is a valid shared reference to a Win32 RECT.
        let rgn = unsafe { CreateRectRgnIndirect(rect) };
        if rgn.is_invalid() {
            None
        } else {
            // SAFETY: CreateRectRgnIndirect returned a new region owned exclusively by `Owned`.
            Some(Self(unsafe { Owned::new(rgn) }))
        }
    }

    fn handle(&self) -> HRGN {
        *self.0
    }

    fn intersect(&self, bounds: &RECT) -> Result<Self> {
        let clipped = Self::from_rect(bounds).ok_or(E_INVALIDARG)?;
        // SAFETY: Both `self` and `clipped` hold valid live GDI region handles.
        let kind = unsafe {
            CombineRgn(
                Some(clipped.handle()),
                Some(self.handle()),
                Some(clipped.handle()),
                RGN_AND,
            )
        };
        if kind == RGN_ERROR {
            return Err(E_FAIL.into());
        }
        Ok(clipped)
    }

    fn data_size(&self) -> u32 {
        // SAFETY: Passing `None` with size 0 queries the required RGNDATA byte length.
        unsafe { GetRegionData(self.handle(), 0, None) }
    }

    fn rectangles(&self) -> Vec<RECT> {
        // GDI writes an RGNDATAHEADER followed by its canonical array of RECTs.
        // Allocate in RECT units so both the header and the array are aligned.
        const HEADER_RECTS: usize = size_of::<RGNDATAHEADER>() / size_of::<RECT>();
        const {
            assert!(size_of::<RGNDATAHEADER>().is_multiple_of(size_of::<RECT>()));
            assert!(align_of::<RECT>() >= align_of::<RGNDATAHEADER>());
        }
        let size = self.data_size();
        if (size as usize) < size_of::<RGNDATAHEADER>() {
            return Vec::new();
        }
        let mut data = vec![RECT::default(); (size as usize).div_ceil(size_of::<RECT>())];
        // SAFETY: The live region cannot change during this query. The buffer
        // holds at least `size` bytes, and `RECT` accepts every integer bit pattern.
        let written =
            unsafe { GetRegionData(self.handle(), size, Some(data.as_mut_ptr().cast())) } as usize;
        if written > size as usize || written < size_of::<RGNDATAHEADER>() {
            return Vec::new();
        }
        data.truncate(written / size_of::<RECT>());
        data.drain(..HEADER_RECTS);
        data.retain(|r| r.right > r.left && r.bottom > r.top);
        data
    }

    /// # Safety
    /// The SDK caller supplies a readable header. For valid dimensions the
    /// allocation must also contain `nCount` initialized `RECT`s at `dwSize`.
    unsafe fn from_clip_list(list: *const RGNDATA) -> Result<Self> {
        // The variable-length header may place rectangles at an unaligned offset.
        #[repr(C, packed)]
        struct PackedRect(RECT);

        let header = list.cast::<RGNDATAHEADER>();
        // SAFETY: Caller supplies a readable `RGNDATAHEADER`; only `dwSize` and `nCount` are read
        // via unaligned loads so uninitialized legacy fields are never touched.
        let (offset, count) = unsafe {
            (
                (&raw const (*header).dwSize).read_unaligned() as usize,
                (&raw const (*header).nCount).read_unaligned() as usize,
            )
        };
        if offset < size_of::<RGNDATAHEADER>()
            || count == 0
            || count > (isize::MAX as usize).saturating_sub(offset) / size_of::<RECT>()
        {
            return Err(DDERR_INVALIDCLIPLIST.into());
        }
        // SAFETY: The caller provides `count` initialized rects at `offset`; `PackedRect` has align 1.
        let rects = unsafe {
            std::slice::from_raw_parts(list.cast::<u8>().add(offset).cast::<PackedRect>(), count)
        };
        let first = rects[0].0;
        let combined = Self::from_rect(&first).ok_or(DDERR_INVALIDCLIPLIST)?;
        for packed in &rects[1..] {
            let rect = packed.0;
            let next = Self::from_rect(&rect).ok_or(DDERR_INVALIDCLIPLIST)?;
            // SAFETY: Both `combined` and `next` are valid live GDI regions.
            let kind = unsafe {
                CombineRgn(
                    Some(combined.handle()),
                    Some(combined.handle()),
                    Some(next.handle()),
                    RGN_OR,
                )
            };
            if kind == RGN_ERROR {
                return Err(DDERR_INVALIDCLIPLIST.into());
            }
        }
        Ok(combined)
    }
}

#[implement(IDirectDrawClipper, Agile = false)]
pub struct Clipper {
    hwnd: AtomicHwnd,
    region: Mutex<Option<OwnedRgn>>,
    display: Option<Weak<DisplayShared>>,
    initialized: AtomicBool,
}

pub fn create_clipper(
    flags: u32,
    display: Option<Arc<DisplayShared>>,
) -> Result<IDirectDrawClipper> {
    if flags != 0 {
        return Err(E_INVALIDARG.into());
    }
    Ok(Clipper {
        hwnd: AtomicHwnd::new(),
        region: Mutex::new(None),
        display: display.map(|d| Arc::downgrade(&d)),
        initialized: AtomicBool::new(true),
    }
    .into())
}

pub fn create_uninitialized_clipper() -> IDirectDrawClipper {
    Clipper {
        hwnd: AtomicHwnd::new(),
        region: Mutex::new(None),
        display: None,
        initialized: AtomicBool::new(false),
    }
    .into()
}

impl Clipper {
    fn current_hwnd(&self) -> HWND {
        self.hwnd.load().unwrap_or_default()
    }

    fn hwnd_region(&self) -> Option<OwnedRgn> {
        let hwnd = self.current_hwnd();
        if hwnd.is_invalid() {
            return None;
        }
        if let Some(rect) = client_rect(hwnd)
            .ok()
            .filter(|r| r.right > 0 && r.bottom > 0)
        {
            return OwnedRgn::from_rect(&rect);
        }
        if let Some(display) = self.display.as_ref().and_then(Weak::upgrade) {
            let (width, height, _) = display.mode();
            if width > 0 && height > 0 {
                let fallback = RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                };
                return OwnedRgn::from_rect(&fallback);
            }
        }
        None
    }

    /// Returns intersected clip rectangles when a manual clip list is attached.
    /// Returns `Ok(None)` when no manual clip list is active (including HWND-only clippers).
    pub fn clip_rects(&self, bounds: RECT) -> Result<Option<Vec<RECT>>> {
        if !self.current_hwnd().is_invalid() {
            return Ok(None);
        }
        let region_ref = self.region.lock().unwrap_or_else(|e| e.into_inner());
        let Some(region) = region_ref.as_ref() else {
            return Ok(None);
        };
        Ok(Some(region.intersect(&bounds)?.rectangles()))
    }
}

pub fn clip_rects(clipper: &IDirectDrawClipper, bounds: RECT) -> Result<Option<Vec<RECT>>> {
    match clipper.cast_object_ref::<Clipper>() {
        Ok(c) => c.clip_rects(bounds),
        Err(_) => Ok(None),
    }
}

impl IDirectDrawClipper_Impl for Clipper_Impl {
    fn GetClipList(&self, rect: *mut RECT, clip_list: *mut RGNDATA, size: *mut u32) -> Result<()> {
        // SAFETY: Caller supplies a writable `u32` size slot (and initialized input when `clip_list` is non-null).
        let size = unsafe { ffi_ptr::out_slot(size, E_INVALIDARG)? };
        let manual = self.region.lock().unwrap_or_else(|e| e.into_inner());
        let hwnd_rgn = if manual.is_none() {
            self.hwnd_region()
        } else {
            None
        };
        let base_rgn = manual
            .as_ref()
            .or(hwnd_rgn.as_ref())
            .ok_or(DDERR_NOCLIPLIST)?;

        // SAFETY: Caller passes nullable readable bounds `RECT`.
        let clipped = unsafe { ffi_ptr::opt_ref(rect) }
            .map(|bounds| base_rgn.intersect(bounds))
            .transpose()?;
        let effective = clipped.as_ref().unwrap_or(base_rgn);

        if clip_list.is_null() {
            let needed = effective.data_size();
            if needed == 0 {
                return Err(E_FAIL.into());
            }
            size.write(needed);
            return Ok(());
        }

        // SAFETY: When `clip_list` is non-null, `*size` holds its initialized byte capacity per COM GetClipList.
        let written =
            unsafe { GetRegionData(effective.handle(), size.assume_init(), Some(clip_list)) };
        if written == 0 {
            let needed = effective.data_size();
            size.write(needed);
            return Err(DDERR_REGIONTOOSMALL.into());
        }
        size.write(written);
        Ok(())
    }

    fn GetHWnd(&self, out: *mut HWND) -> Result<()> {
        // SAFETY: Caller supplies a writable `HWND` output slot.
        unsafe { ffi_ptr::write_out(out, self.current_hwnd(), E_INVALIDARG) }
    }

    fn Initialize(&self, _: Ref<IDirectDraw>, flags: u32) -> Result<()> {
        if self.initialized.load(Ordering::Acquire) {
            return Err(DDERR_ALREADYINITIALIZED.into());
        }
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn IsClipListChanged(&self, out: *mut BOOL) -> Result<()> {
        // SAFETY: Caller supplies a writable `BOOL` output slot.
        unsafe { ffi_ptr::write_out(out, BOOL(0), E_POINTER) }
    }

    fn SetClipList(&self, clip_list: *mut RGNDATA, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if !self.current_hwnd().is_invalid() {
            return Err(DDERR_CLIPPERISUSINGHWND.into());
        }
        if clip_list.is_null() {
            *self.region.lock().unwrap_or_else(|e| e.into_inner()) = None;
            return Ok(());
        }
        // SAFETY: SetClipList receives the variable-length region from the COM caller.
        let combined = unsafe { OwnedRgn::from_clip_list(clip_list)? };
        *self.region.lock().unwrap_or_else(|e| e.into_inner()) = Some(combined);
        Ok(())
    }

    fn SetHWnd(&self, flags: u32, hwnd: HWND) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if !hwnd.is_invalid() && !is_window(hwnd) {
            return Err(E_INVALIDARG.into());
        }
        self.hwnd.store((!hwnd.is_invalid()).then_some(hwnd));
        if !hwnd.is_invalid() {
            *self.region.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::MaybeUninit;
    use windows::Win32::Graphics::Gdi::RDH_RECTANGLES;

    #[repr(C)]
    struct TwoRectRgn {
        header: RGNDATAHEADER,
        rects: [RECT; 2],
    }

    #[test]
    fn clip_list_handles_extended_headers_and_rejects_impossible_counts() {
        #[repr(C, packed)]
        struct ExtendedRegion {
            header: RGNDATAHEADER,
            extension: u8,
            rect: RECT,
        }
        let bounds = RECT {
            left: -8,
            top: -3,
            right: 7,
            bottom: 12,
        };
        let mut data = ExtendedRegion {
            header: RGNDATAHEADER {
                dwSize: (size_of::<RGNDATAHEADER>() + 1) as u32,
                iType: RDH_RECTANGLES,
                nCount: 1,
                ..Default::default()
            },
            extension: 0,
            rect: bounds,
        };
        let clipper = create_clipper(0, None).unwrap();
        // SAFETY: Passing stack-allocated valid `ExtendedRegion` and writable `size` slot.
        unsafe {
            clipper.SetClipList((&raw mut data).cast(), 0).unwrap();
            let mut size = MaybeUninit::uninit();
            clipper
                .GetClipList(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    size.as_mut_ptr(),
                )
                .unwrap();
            assert_eq!(
                size.assume_init() as usize,
                size_of::<RGNDATAHEADER>() + size_of::<RECT>()
            );
            data.header.nCount = u32::MAX;
            assert_eq!(
                clipper
                    .SetClipList((&raw mut data).cast(), 0)
                    .unwrap_err()
                    .code(),
                DDERR_INVALIDCLIPLIST
            );
        }
        assert_eq!(clip_rects(&clipper, bounds).unwrap().unwrap(), vec![bounds]);
    }

    #[test]
    fn clip_list_intersects_with_bounds() {
        let clipper = create_clipper(0, None).unwrap();
        let data = TwoRectRgn {
            header: RGNDATAHEADER {
                dwSize: size_of::<RGNDATAHEADER>() as u32,
                iType: RDH_RECTANGLES,
                nCount: 2,
                nRgnSize: (2 * size_of::<RECT>()) as u32,
                rcBound: RECT {
                    left: 0,
                    top: 0,
                    right: 20,
                    bottom: 20,
                },
            },
            rects: [
                RECT {
                    left: 0,
                    top: 0,
                    right: 10,
                    bottom: 10,
                },
                RECT {
                    left: 12,
                    top: 12,
                    right: 20,
                    bottom: 20,
                },
            ],
        };
        // SAFETY: Passing stack-allocated valid `TwoRectRgn` matching `nCount == 2`.
        unsafe {
            clipper
                .SetClipList((&raw const data).cast::<RGNDATA>().cast_mut(), 0)
                .unwrap();
        }
        let rects = clip_rects(
            &clipper,
            RECT {
                left: 5,
                top: 5,
                right: 15,
                bottom: 15,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(rects.len(), 2);
        assert_eq!(
            rects[0],
            RECT {
                left: 5,
                top: 5,
                right: 10,
                bottom: 10,
            }
        );
        assert_eq!(
            rects[1],
            RECT {
                left: 12,
                top: 12,
                right: 15,
                bottom: 15,
            }
        );
    }
}
