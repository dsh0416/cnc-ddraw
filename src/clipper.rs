//! DirectDraw clipper implementation backed by Win32 GDI regions.
#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::rc::{Rc, Weak};

use windows::core::{implement, Interface, Ref, Result, BOOL, HRESULT};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_POINTER, HWND, RECT};
use windows::Win32::Graphics::DirectDraw::{
    IDirectDraw, IDirectDrawClipper, IDirectDrawClipper_Impl,
};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgnIndirect, DeleteObject, GetRegionData, HRGN, RGNDATA, RGNDATAHEADER,
    RGN_AND, RGN_ERROR, RGN_OR,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsWindow};

use crate::draw::Display;

const fn dd_hresult(code: u32) -> HRESULT {
    HRESULT((0x8876_0000 | code) as i32)
}

const DDERR_ALREADYINITIALIZED: HRESULT = dd_hresult(5);
const DDERR_INVALIDCLIPLIST: HRESULT = dd_hresult(110);
const DDERR_NOCLIPLIST: HRESULT = dd_hresult(205);
const DDERR_REGIONTOOSMALL: HRESULT = dd_hresult(566);
const DDERR_CLIPPERISUSINGHWND: HRESULT = dd_hresult(567);

struct OwnedRgn(HRGN);

impl OwnedRgn {
    fn from_rect(rect: &RECT) -> Option<Self> {
        let rgn = unsafe { CreateRectRgnIndirect(rect) };
        if rgn.is_invalid() {
            None
        } else {
            Some(Self(rgn))
        }
    }

    fn handle(&self) -> HRGN {
        self.0
    }
}

impl Drop for OwnedRgn {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteObject(self.0.into());
            }
        }
    }
}

#[implement(IDirectDrawClipper, Agile = false)]
pub struct Clipper {
    hwnd: Cell<HWND>,
    region: RefCell<Option<OwnedRgn>>,
    display: Option<Weak<RefCell<Display>>>,
    initialized: Cell<bool>,
}

pub fn create_clipper(
    flags: u32,
    display: Option<Rc<RefCell<Display>>>,
) -> Result<IDirectDrawClipper> {
    if flags != 0 {
        return Err(E_INVALIDARG.into());
    }
    Ok(Clipper {
        hwnd: Cell::new(HWND::default()),
        region: RefCell::new(None),
        display: display.map(|d| Rc::downgrade(&d)),
        initialized: Cell::new(true),
    }
    .into())
}

pub fn create_uninitialized_clipper() -> IDirectDrawClipper {
    Clipper {
        hwnd: Cell::new(HWND::default()),
        region: RefCell::new(None),
        display: None,
        initialized: Cell::new(false),
    }
    .into()
}

impl Clipper {
    fn hwnd_region(&self) -> Option<OwnedRgn> {
        let hwnd = self.hwnd.get();
        if hwnd.is_invalid() {
            return None;
        }
        let mut rect = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut rect) }.is_ok() && rect.right > 0 && rect.bottom > 0 {
            return OwnedRgn::from_rect(&rect);
        }
        if let Some(display) = self.display.as_ref().and_then(Weak::upgrade) {
            if let Ok(d) = display.try_borrow() {
                if d.width > 0 && d.height > 0 {
                    let fallback = RECT {
                        left: 0,
                        top: 0,
                        right: d.width as i32,
                        bottom: d.height as i32,
                    };
                    return OwnedRgn::from_rect(&fallback);
                }
            }
        }
        None
    }

    /// Returns intersected clip rectangles when a manual clip list is attached.
    /// Returns `Ok(None)` when no manual clip list is active (including HWND-only clippers).
    pub fn clip_rects(&self, bounds: RECT) -> Result<Option<Vec<RECT>>> {
        if !self.hwnd.get().is_invalid() {
            return Ok(None);
        }
        let region_ref = self.region.borrow();
        let Some(region) = region_ref.as_ref() else {
            return Ok(None);
        };
        let intersected = OwnedRgn::from_rect(&bounds).ok_or(E_INVALIDARG)?;
        let kind = unsafe {
            CombineRgn(
                Some(intersected.handle()),
                Some(region.handle()),
                Some(intersected.handle()),
                RGN_AND,
            )
        };
        if kind == RGN_ERROR {
            return Err(E_FAIL.into());
        }
        let needed = unsafe { GetRegionData(intersected.handle(), 0, None) } as usize;
        if needed < size_of::<RGNDATAHEADER>() {
            return Ok(Some(Vec::new()));
        }
        let mut buf = vec![0u8; needed];
        let written = unsafe {
            GetRegionData(
                intersected.handle(),
                buf.len() as u32,
                Some(buf.as_mut_ptr().cast::<RGNDATA>()),
            )
        } as usize;
        if written < size_of::<RGNDATAHEADER>() {
            return Ok(Some(Vec::new()));
        }
        let header = unsafe { &*buf.as_ptr().cast::<RGNDATAHEADER>() };
        let count = header.nCount as usize;
        let offset = header.dwSize as usize;
        let rects_end = offset.saturating_add(count.saturating_mul(size_of::<RECT>()));
        if rects_end > written {
            return Ok(Some(Vec::new()));
        }
        let mut rects = Vec::with_capacity(count);
        let ptr = unsafe { buf.as_ptr().add(offset).cast::<RECT>() };
        for i in 0..count {
            let r = unsafe { ptr.add(i).read_unaligned() };
            if r.right > r.left && r.bottom > r.top {
                rects.push(r);
            }
        }
        Ok(Some(rects))
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
        let size = unsafe { size.as_mut() }.ok_or(E_INVALIDARG)?;
        let manual = self.region.borrow();
        let hwnd_rgn = if manual.is_none() {
            self.hwnd_region()
        } else {
            None
        };
        let base_rgn = manual
            .as_ref()
            .or(hwnd_rgn.as_ref())
            .ok_or(DDERR_NOCLIPLIST)?;

        let clipped = if let Some(bounds) = unsafe { rect.as_ref() } {
            let temp = OwnedRgn::from_rect(bounds).ok_or(E_INVALIDARG)?;
            let kind = unsafe {
                CombineRgn(
                    Some(temp.handle()),
                    Some(base_rgn.handle()),
                    Some(temp.handle()),
                    RGN_AND,
                )
            };
            if kind == RGN_ERROR {
                return Err(E_FAIL.into());
            }
            Some(temp)
        } else {
            None
        };
        let effective = clipped.as_ref().unwrap_or(base_rgn);

        if clip_list.is_null() {
            let needed = unsafe { GetRegionData(effective.handle(), 0, None) };
            if needed == 0 {
                return Err(E_FAIL.into());
            }
            *size = needed;
            return Ok(());
        }

        let written = unsafe { GetRegionData(effective.handle(), *size, Some(clip_list)) };
        if written == 0 {
            let needed = unsafe { GetRegionData(effective.handle(), 0, None) };
            *size = needed;
            return Err(DDERR_REGIONTOOSMALL.into());
        }
        *size = written;
        Ok(())
    }

    fn GetHWnd(&self, out: *mut HWND) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        *out = self.hwnd.get();
        Ok(())
    }

    fn Initialize(&self, _: Ref<IDirectDraw>, flags: u32) -> Result<()> {
        if self.initialized.get() {
            return Err(DDERR_ALREADYINITIALIZED.into());
        }
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        self.initialized.set(true);
        Ok(())
    }

    fn IsClipListChanged(&self, out: *mut BOOL) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_POINTER)?;
        *out = BOOL(0);
        Ok(())
    }

    fn SetClipList(&self, clip_list: *mut RGNDATA, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if !self.hwnd.get().is_invalid() {
            return Err(DDERR_CLIPPERISUSINGHWND.into());
        }
        if clip_list.is_null() {
            *self.region.borrow_mut() = None;
            return Ok(());
        }
        let header = unsafe { &(*clip_list).rdh };
        if (header.dwSize as usize) < size_of::<RGNDATAHEADER>() || header.nCount == 0 {
            return Err(DDERR_INVALIDCLIPLIST.into());
        }
        let rects_ptr = unsafe {
            clip_list
                .cast::<u8>()
                .add(header.dwSize as usize)
                .cast::<RECT>()
        };
        let first = unsafe { rects_ptr.read_unaligned() };
        let combined = OwnedRgn::from_rect(&first).ok_or(DDERR_INVALIDCLIPLIST)?;
        for i in 1..header.nCount as usize {
            let next_rect = unsafe { rects_ptr.add(i).read_unaligned() };
            let next = OwnedRgn::from_rect(&next_rect).ok_or(DDERR_INVALIDCLIPLIST)?;
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
        *self.region.borrow_mut() = Some(combined);
        Ok(())
    }

    fn SetHWnd(&self, flags: u32, hwnd: HWND) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if !hwnd.is_invalid() && !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(E_INVALIDARG.into());
        }
        self.hwnd.set(hwnd);
        if !hwnd.is_invalid() {
            *self.region.borrow_mut() = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::RDH_RECTANGLES;

    #[repr(C)]
    struct TwoRectRgn {
        header: RGNDATAHEADER,
        rects: [RECT; 2],
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
