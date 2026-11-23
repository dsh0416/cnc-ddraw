//! CPU-backed DirectDraw 1 surfaces. A lock exposes stable storage until Unlock.
#![allow(non_snake_case)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::rc::Rc;

use windows::core::{implement, Error, Interface, OutRef, Ref, Result, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, HANDLE, RECT};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_BITFIELDS, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    PALETTEENTRY,
};

use crate::draw::Display;

const fn dd_hresult(code: u32) -> HRESULT {
    HRESULT((0x8876_0000 | code) as i32)
}

const DDERR_ALREADYINITIALIZED: HRESULT = dd_hresult(5);
const DDERR_INVALIDCAPS: HRESULT = dd_hresult(100);
const DDERR_INVALIDPIXELFORMAT: HRESULT = dd_hresult(145);
const DDERR_INVALIDRECT: HRESULT = dd_hresult(150);
const DDERR_NOCOLORKEY: HRESULT = dd_hresult(215);
const DDERR_NOTFOUND: HRESULT = dd_hresult(255);
const DDERR_SURFACEBUSY: HRESULT = dd_hresult(430);
const DDERR_SURFACENOTATTACHED: HRESULT = dd_hresult(460);
const DDERR_PRIMARYSURFACEALREADYEXISTS: HRESULT = dd_hresult(564);
const DDERR_NOCLIPPERATTACHED: HRESULT = dd_hresult(568);
const DDERR_NOPALETTEATTACHED: HRESULT = dd_hresult(572);
const DDERR_NOTFLIPPABLE: HRESULT = dd_hresult(582);
const DDERR_NOTLOCKED: HRESULT = dd_hresult(584);
const DDERR_CANTCREATEDC: HRESULT = dd_hresult(585);
const DDERR_NODC: HRESULT = dd_hresult(586);
const DDERR_DCALREADYCREATED: HRESULT = dd_hresult(620);

fn unsupported<T>() -> Result<T> {
    Err(E_NOTIMPL.into())
}

fn zeros(size: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    data.try_reserve_exact(size)
        .map_err(|_| Error::from(E_OUTOFMEMORY))?;
    data.resize(size, 0);
    Ok(data)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Indexed,
    Rgb555,
    Rgb565,
    Xrgb,
}

impl Format {
    fn from_depth(bpp: u32) -> Result<Self> {
        match bpp {
            8 => Ok(Self::Indexed),
            16 => Ok(Self::Rgb565),
            32 => Ok(Self::Xrgb),
            _ => Err(DDERR_INVALIDPIXELFORMAT.into()), // DDERR_INVALIDPIXELFORMAT
        }
    }

    fn from_dd(pf: &DDPIXELFORMAT) -> Result<Self> {
        if pf.dwSize as usize != size_of::<DDPIXELFORMAT>()
            || pf.dwFlags & !(DDPF_RGB | DDPF_PALETTEINDEXED8) as u32 != 0
        {
            return Err(DDERR_INVALIDPIXELFORMAT.into());
        }
        unsafe {
            let masks = (
                pf.Anonymous2.dwRBitMask,
                pf.Anonymous3.dwGBitMask,
                pf.Anonymous4.dwBBitMask,
            );
            match (pf.Anonymous1.dwRGBBitCount, masks) {
                (8, _) if pf.dwFlags & DDPF_PALETTEINDEXED8 as u32 != 0 => Ok(Self::Indexed),
                (16, (0xf800, 0x07e0, 0x001f)) => Ok(Self::Rgb565),
                (16, (0x7c00, 0x03e0, 0x001f)) => Ok(Self::Rgb555),
                (32, (0xff0000, 0xff00, 0xff)) => Ok(Self::Xrgb),
                _ => Err(DDERR_INVALIDPIXELFORMAT.into()),
            }
        }
    }

    fn bytes(self) -> usize {
        match self {
            Self::Indexed => 1,
            Self::Rgb565 | Self::Rgb555 => 2,
            Self::Xrgb => 4,
        }
    }

    fn description(self) -> DDPIXELFORMAT {
        let (red, green, blue) = match self {
            Self::Indexed => (0, 0, 0),
            Self::Rgb555 => (0x7c00, 0x03e0, 0x001f),
            Self::Rgb565 => (0xf800, 0x07e0, 0x001f),
            Self::Xrgb => (0xff0000, 0xff00, 0xff),
        };
        let mut pf = DDPIXELFORMAT {
            dwSize: size_of::<DDPIXELFORMAT>() as u32,
            dwFlags: DDPF_RGB as u32,
            ..Default::default()
        };
        if self == Self::Indexed {
            pf.dwFlags |= DDPF_PALETTEINDEXED8 as u32;
        }
        pf.Anonymous1.dwRGBBitCount = self.bytes() as u32 * 8;
        pf.Anonymous2.dwRBitMask = red;
        pf.Anonymous3.dwGBitMask = green;
        pf.Anonymous4.dwBBitMask = blue;
        pf
    }
}

pub fn pixel_format(bpp: u32) -> Result<DDPIXELFORMAT> {
    Ok(Format::from_depth(bpp)?.description())
}

// A DirectDraw lock and a GDI DC both grant exclusive write access.
enum Access {
    Ready,
    Locked(usize),
    Gdi(SurfaceDc),
}

#[repr(C)]
struct DibInfo {
    header: BITMAPINFOHEADER,
    // GDI interprets the trailing bytes as either RGBQUADs or three DWORD masks.
    table: [u32; 256],
}

struct SurfaceDc {
    handle: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
}

impl SurfaceDc {
    fn new(surface: &SurfaceData) -> Result<Self> {
        let mut info = DibInfo {
            header: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: surface.width as i32,
                biHeight: -(surface.height as i32), // top-down, same pitch as the surface
                biPlanes: 1,
                biBitCount: (surface.format.bytes() * 8) as u16,
                biCompression: BI_RGB.0,
                biSizeImage: surface.pixels.len() as u32,
                ..Default::default()
            },
            table: [0; 256],
        };
        match surface.format {
            Format::Indexed => {
                info.header.biClrUsed = 256;
                for (slot, color) in info.table.iter_mut().zip(surface.palette_entries()?) {
                    *slot = u32::from_le_bytes([color.peBlue, color.peGreen, color.peRed, 0]);
                }
            }
            Format::Rgb555 | Format::Rgb565 => {
                info.header.biCompression = BI_BITFIELDS.0;
                let format = surface.format.description();
                info.table[..3].copy_from_slice(&unsafe {
                    [
                        format.Anonymous2.dwRBitMask,
                        format.Anonymous3.dwGBitMask,
                        format.Anonymous4.dwBBitMask,
                    ]
                });
            }
            Format::Xrgb => {}
        }
        let mut dc = Self {
            handle: unsafe { CreateCompatibleDC(None) },
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
        };
        if dc.handle.is_invalid() {
            return Err(DDERR_CANTCREATEDC.into());
        }
        unsafe {
            dc.bitmap = CreateDIBSection(
                Some(dc.handle),
                (&raw const info).cast::<BITMAPINFO>(),
                DIB_RGB_COLORS,
                &mut dc.bits,
                None,
                0,
            )
            .map_err(|_| DDERR_CANTCREATEDC)?;
            dc.previous = SelectObject(dc.handle, dc.bitmap.into());
            if dc.bits.is_null() || dc.previous.is_invalid() {
                return Err(DDERR_CANTCREATEDC.into());
            }
            std::ptr::copy_nonoverlapping(
                surface.pixels.as_ptr(),
                dc.bits.cast(),
                surface.pixels.len(),
            );
        }
        Ok(dc)
    }
}

impl Drop for SurfaceDc {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                let _ = SelectObject(self.handle, self.previous);
            }
            if !self.handle.is_invalid() {
                let _ = DeleteDC(self.handle);
            }
            if !self.bitmap.is_invalid() {
                let _ = DeleteObject(self.bitmap.into());
            }
        }
    }
}

pub struct SurfaceData {
    pub width: u32,
    pub height: u32,
    access: Access,
    pitch: usize,
    format: Format,
    caps: u32,
    pixels: Vec<u8>,
    palette: Option<IDirectDrawPalette>,
    source_key: Option<DDCOLORKEY>,
}

impl SurfaceData {
    fn new(width: u32, height: u32, format: Format, caps: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err(E_INVALIDARG.into());
        }
        let pitch = (width as usize)
            .checked_mul(format.bytes())
            .and_then(|v| v.checked_add(3))
            .map(|v| v & !3)
            .filter(|v| *v <= i32::MAX as usize)
            .ok_or(E_OUTOFMEMORY)?;
        let len = pitch.checked_mul(height as usize).ok_or(E_OUTOFMEMORY)?;
        Ok(Self {
            width,
            height,
            pitch,
            format,
            caps,
            pixels: zeros(len)?,
            palette: None,
            access: Access::Ready,
            source_key: None,
        })
    }

    fn bounds(&self, rect: *const RECT) -> Result<RECT> {
        let r = unsafe { rect.as_ref() }.copied().unwrap_or(RECT {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        });
        if r.left < 0
            || r.top < 0
            || r.right <= r.left
            || r.bottom <= r.top
            || r.right > self.width as i32
            || r.bottom > self.height as i32
        {
            return Err(DDERR_INVALIDRECT.into()); // DDERR_INVALIDRECT
        }
        Ok(r)
    }

    fn desc(&self, back: bool) -> DDSURFACEDESC {
        let mut desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_PIXELFORMAT) as u32,
            dwWidth: self.width,
            dwHeight: self.height,
            ddsCaps: DDSCAPS { dwCaps: self.caps },
            ddpfPixelFormat: self.format.description(),
            ..Default::default()
        };
        desc.Anonymous1.lPitch = self.pitch as i32;
        if back {
            desc.dwFlags |= DDSD_BACKBUFFERCOUNT as u32;
            desc.dwBackBufferCount = 1;
        }
        if let Some(key) = self.source_key {
            desc.dwFlags |= DDSD_CKSRCBLT as u32;
            desc.ddckCKSrcBlt = key;
        }
        desc
    }

    pub fn locked(&self) -> bool {
        !matches!(self.access, Access::Ready)
    }

    fn ready(&self) -> Result<()> {
        if self.locked() {
            Err(DDERR_SURFACEBUSY.into())
        } else {
            Ok(())
        }
    }

    pub fn uses_palette(&self, palette: &IDirectDrawPalette) -> bool {
        self.palette.as_ref() == Some(palette)
    }

    fn palette_entries(&self) -> Result<[PALETTEENTRY; 256]> {
        let mut entries = [PALETTEENTRY::default(); 256];
        if self.format == Format::Indexed {
            if let Some(palette) = &self.palette {
                unsafe {
                    palette.GetEntries(0, 0, 256, entries.as_mut_ptr())?;
                }
            }
        }
        Ok(entries)
    }
}

pub fn frame_rgba(data: &SurfaceData, rgba: &mut Vec<u8>) -> Result<()> {
    data.ready()?;
    let entries = data.palette_entries()?;
    let width = data.width as usize;
    let size = width
        .checked_mul(data.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(E_OUTOFMEMORY)?;
    rgba.try_reserve_exact(size.saturating_sub(rgba.len()))
        .map_err(|_| E_OUTOFMEMORY)?;
    rgba.resize(size, 0);
    let bytes = data.format.bytes();
    for (source, dst) in data
        .pixels
        .chunks_exact(data.pitch)
        .zip(rgba.chunks_exact_mut(width * 4))
    {
        for (pixel, out) in source[..width * bytes]
            .chunks_exact(bytes)
            .zip(dst.as_chunks_mut::<4>().0.iter_mut())
        {
            let rgb = match data.format {
                Format::Indexed => {
                    let e = entries[pixel[0] as usize];
                    [e.peRed, e.peGreen, e.peBlue]
                }
                Format::Xrgb => [pixel[2], pixel[1], pixel[0]],
                Format::Rgb555 | Format::Rgb565 => {
                    let value = u16::from_le_bytes([pixel[0], pixel[1]]);
                    let green_bits = if data.format == Format::Rgb565 { 6 } else { 5 };
                    let green_max = (1 << green_bits) - 1;
                    [
                        (((value >> (5 + green_bits)) & 31) * 255 / 31) as u8,
                        (((value >> 5) & green_max) * 255 / green_max) as u8,
                        ((value & 31) * 255 / 31) as u8,
                    ]
                }
            };
            out[..3].copy_from_slice(&rgb);
            out[3] = 255;
        }
    }
    Ok(())
}

#[implement(IDirectDrawSurface, Agile = false)]
struct Surface {
    display: Rc<RefCell<Display>>,
    data: Rc<RefCell<SurfaceData>>,
    back: Option<IDirectDrawSurface>,
}

pub fn create_surface(
    display: Rc<RefCell<Display>>,
    desc: &DDSURFACEDESC,
) -> Result<IDirectDrawSurface> {
    if desc.dwSize as usize != size_of::<DDSURFACEDESC>() {
        return Err(E_INVALIDARG.into());
    }
    let caps = desc.ddsCaps.dwCaps;
    let supported_caps = DDSCAPS_PRIMARYSURFACE
        | DDSCAPS_OFFSCREENPLAIN
        | DDSCAPS_SYSTEMMEMORY
        | DDSCAPS_FLIP
        | DDSCAPS_COMPLEX
        | DDSCAPS_VISIBLE
        | DDSCAPS_FRONTBUFFER
        | DDSCAPS_BACKBUFFER;
    if caps & !(supported_caps as u32) != 0
        || desc.dwFlags
            & !((DDSD_CAPS
                | DDSD_WIDTH
                | DDSD_HEIGHT
                | DDSD_PIXELFORMAT
                | DDSD_BACKBUFFERCOUNT
                | DDSD_CKSRCBLT) as u32)
            != 0
    {
        return unsupported();
    }
    let primary = caps & DDSCAPS_PRIMARYSURFACE as u32 != 0;
    let d = display.try_borrow().map_err(|_| DDERR_SURFACEBUSY)?;
    if primary && d.primary.upgrade().is_some() {
        return Err(DDERR_PRIMARYSURFACEALREADYEXISTS.into());
    }
    let (width, height) = if primary {
        (d.width, d.height)
    } else {
        (desc.dwWidth, desc.dwHeight)
    };
    let format = if desc.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
        Format::from_dd(&desc.ddpfPixelFormat)?
    } else {
        Format::from_depth(d.bpp)?
    };
    drop(d);
    let count = if desc.dwFlags & DDSD_BACKBUFFERCOUNT as u32 != 0 {
        desc.dwBackBufferCount
    } else {
        0
    };
    if count > 1 {
        return unsupported();
    }
    if (count == 1) != (caps & DDSCAPS_FLIP as u32 != 0) {
        return Err(DDERR_INVALIDCAPS.into());
    }
    let mut data = SurfaceData::new(width, height, format, caps | DDSCAPS_SYSTEMMEMORY as u32)?;
    if primary {
        data.caps |= DDSCAPS_VISIBLE as u32;
    }
    if desc.dwFlags & DDSD_CKSRCBLT as u32 != 0 {
        data.source_key = Some(desc.ddckCKSrcBlt);
    }
    let back = if count == 1 {
        data.caps |= DDSCAPS_FRONTBUFFER as u32;
        let back = SurfaceData::new(
            width,
            height,
            format,
            (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP | DDSCAPS_SYSTEMMEMORY) as u32,
        )?;
        Some(
            Surface {
                display: display.clone(),
                data: Rc::new(RefCell::new(back)),
                back: None,
            }
            .into(),
        )
    } else {
        None
    };
    let data = Rc::new(RefCell::new(data));
    if primary {
        display
            .try_borrow_mut()
            .map_err(|_| DDERR_SURFACEBUSY)?
            .primary = Rc::downgrade(&data);
    }
    Ok(Surface {
        display,
        data,
        back,
    }
    .into())
}

impl Surface {
    fn read(&self) -> Result<std::cell::Ref<'_, SurfaceData>> {
        self.data
            .try_borrow()
            .map_err(|_| Error::from(DDERR_SURFACEBUSY))
    }

    fn write(&self) -> Result<std::cell::RefMut<'_, SurfaceData>> {
        self.data
            .try_borrow_mut()
            .map_err(|_| Error::from(DDERR_SURFACEBUSY))
    }

    fn refresh(&self) -> Result<()> {
        if self.read()?.caps & DDSCAPS_PRIMARYSURFACE as u32 != 0 {
            self.display
                .try_borrow_mut()
                .map_err(|_| DDERR_SURFACEBUSY)?
                .refresh()?;
        }
        Ok(())
    }

    fn copy(
        &self,
        source: &IDirectDrawSurface,
        src_rect: *const RECT,
        dest_rect: RECT,
        keyed: bool,
    ) -> Result<()> {
        let object = source.cast_object_ref::<Surface>()?;
        let src = object.read()?;
        src.ready()?;
        let src_rect = src.bounds(src_rect)?;
        let format = src.format;
        let key = if keyed {
            Some(src.source_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        // Snapshot overlapping self-blits before borrowing the destination mutably.
        if Rc::ptr_eq(&object.data, &self.data) {
            let row_bytes = (src_rect.right - src_rect.left) as usize * format.bytes();
            let mut snapshot = zeros(row_bytes * (src_rect.bottom - src_rect.top) as usize)?;
            for (y, row) in snapshot.chunks_exact_mut(row_bytes).enumerate() {
                let start = (src_rect.top as usize + y) * src.pitch
                    + src_rect.left as usize * format.bytes();
                row.copy_from_slice(&src.pixels[start..start + row_bytes]);
            }
            drop(src);
            let local = RECT {
                left: 0,
                top: 0,
                right: src_rect.right - src_rect.left,
                bottom: src_rect.bottom - src_rect.top,
            };
            blit(
                &mut *self.write()?,
                dest_rect,
                &snapshot,
                row_bytes,
                local,
                key,
            );
        } else {
            let mut dst = self.write()?;
            dst.ready()?;
            if dst.format != format {
                return unsupported();
            }
            blit(&mut dst, dest_rect, &src.pixels, src.pitch, src_rect, key);
        }
        self.refresh()
    }
}

fn blit(
    dst: &mut SurfaceData,
    dest: RECT,
    source: &[u8],
    pitch: usize,
    src: RECT,
    key: Option<DDCOLORKEY>,
) {
    let bytes = dst.format.bytes();
    let (dw, dh) = (
        (dest.right - dest.left) as usize,
        (dest.bottom - dest.top) as usize,
    );
    let (sw, sh) = (
        (src.right - src.left) as usize,
        (src.bottom - src.top) as usize,
    );
    for y in 0..dh {
        let sy = src.top as usize + (y as u64 * sh as u64 / dh as u64) as usize;
        let start = sy * pitch + src.left as usize * bytes;
        let row = &source[start..start + sw * bytes];
        let start = (dest.top as usize + y) * dst.pitch + dest.left as usize * bytes;
        let out = &mut dst.pixels[start..start + dw * bytes];
        if dw == sw && key.is_none() {
            out.copy_from_slice(row);
            continue;
        }
        for (x, target) in out.chunks_exact_mut(bytes).enumerate() {
            let sx = (x as u64 * sw as u64 / dw as u64) as usize * bytes;
            let pixel = &row[sx..sx + bytes];
            if let Some(range) = key {
                let mut word = [0; 4];
                word[..bytes].copy_from_slice(pixel);
                if (range.dwColorSpaceLowValue..=range.dwColorSpaceHighValue)
                    .contains(&u32::from_le_bytes(word))
                {
                    continue;
                }
            }
            target.copy_from_slice(pixel);
        }
    }
}

impl IDirectDrawSurface_Impl for Surface_Impl {
    fn Blt(
        &self,
        dest: *mut RECT,
        source: Ref<IDirectDrawSurface>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        if flags & !((DDBLT_WAIT | DDBLT_DONOTWAIT | DDBLT_COLORFILL | DDBLT_KEYSRC) as u32) != 0 {
            return unsupported();
        }
        let rect = self.read()?.bounds(dest)?;
        if flags & DDBLT_COLORFILL as u32 != 0 {
            if !source.is_null() || flags & DDBLT_KEYSRC as u32 != 0 {
                return Err(E_INVALIDARG.into());
            }
            let fx = unsafe { fx.as_ref() }.ok_or(E_INVALIDARG)?;
            if fx.dwSize as usize != size_of::<DDBLTFX>() {
                return Err(E_INVALIDARG.into());
            }
            let color = unsafe { fx.Anonymous5.dwFillColor }.to_le_bytes();
            let mut dst = self.write()?;
            dst.ready()?;
            let bytes = dst.format.bytes();
            let pitch = dst.pitch;
            for y in rect.top as usize..rect.bottom as usize {
                let row = &mut dst.pixels[y * pitch + rect.left as usize * bytes
                    ..y * pitch + rect.right as usize * bytes];
                for pixel in row.chunks_exact_mut(bytes) {
                    pixel.copy_from_slice(&color[..bytes]);
                }
            }
            drop(dst);
            self.refresh()
        } else {
            self.copy(source.ok()?, src, rect, flags & DDBLT_KEYSRC as u32 != 0)
        }
    }

    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface>,
        src_rect: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        if flags & !(DDBLTFAST_WAIT | DDBLTFAST_DONOTWAIT | DDBLTFAST_SRCCOLORKEY) != 0 {
            return unsupported();
        }
        let source = source.ok()?;
        let s = source
            .cast_object_ref::<Surface>()?
            .read()?
            .bounds(src_rect)?;
        let left = i32::try_from(x).map_err(|_| DDERR_INVALIDRECT)?;
        let top = i32::try_from(y).map_err(|_| DDERR_INVALIDRECT)?;
        let rect = RECT {
            left,
            top,
            right: left
                .checked_add(s.right - s.left)
                .ok_or(DDERR_INVALIDRECT)?,
            bottom: top.checked_add(s.bottom - s.top).ok_or(DDERR_INVALIDRECT)?,
        };
        self.read()?.bounds(&rect)?;
        self.copy(source, &s, rect, flags & DDBLTFAST_SRCCOLORKEY != 0)
    }

    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS,
        out: OutRef<IDirectDrawSurface>,
    ) -> Result<()> {
        let attachment = (|| {
            let caps = unsafe { caps.as_ref() }.ok_or(E_INVALIDARG)?.dwCaps;
            let back = self.back.as_ref().ok_or(DDERR_NOTFOUND)?;
            let actual = back.cast_object_ref::<Surface>()?.read()?.caps;
            if caps == 0 || actual & caps != caps {
                return Err(DDERR_NOTFOUND.into());
            }
            Ok(back)
        })();
        match attachment {
            Ok(back) => out.write(Some(back.clone())),
            Err(error) => {
                out.write(None)?;
                Err(error)
            }
        }
    }

    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        if let Some(s) = &self.back {
            let mut desc = s.cast_object_ref::<Surface>()?.read()?.desc(false);
            // The DirectDraw enumeration contract transfers a reference to the callback.
            let owned = std::mem::ManuallyDrop::new(s.clone());
            unsafe {
                callback((&*owned).into(), &mut desc, context).ok()?;
            }
        }
        Ok(())
    }

    fn Flip(&self, target: Ref<IDirectDrawSurface>, flags: u32) -> Result<()> {
        if flags & !((DDFLIP_WAIT | DDFLIP_DONOTWAIT | DDFLIP_NOVSYNC) as u32) != 0 {
            return unsupported();
        }
        let back = self.back.as_ref().ok_or(DDERR_NOTFLIPPABLE)?;
        if target.as_ref().is_some_and(|t| t != back) {
            return Err(DDERR_SURFACENOTATTACHED.into());
        }
        let object = back.cast_object_ref::<Surface>()?;
        {
            let mut front = self.write()?;
            let mut back = object.write()?;
            front.ready()?;
            back.ready()?;
            std::mem::swap(&mut front.pixels, &mut back.pixels);
        }
        self.refresh()
    }

    fn GetCaps(&self, caps: *mut DDSCAPS) -> Result<()> {
        unsafe { caps.as_mut() }.ok_or(E_INVALIDARG)?.dwCaps = self.read()?.caps;
        Ok(())
    }

    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        if flags & !(DDCKEY_COLORSPACE as u32) != DDCKEY_SRCBLT as u32 {
            return unsupported();
        }
        let mut key = unsafe { key.as_ref() }.copied();
        if flags & DDCKEY_COLORSPACE as u32 == 0 {
            if let Some(key) = &mut key {
                key.dwColorSpaceHighValue = key.dwColorSpaceLowValue;
            }
        }
        self.write()?.source_key = key;
        Ok(())
    }

    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        if flags != DDCKEY_SRCBLT as u32 {
            return unsupported();
        }
        *unsafe { key.as_mut() }.ok_or(E_INVALIDARG)? =
            self.read()?.source_key.ok_or(DDERR_NOCOLORKEY)?;
        Ok(())
    }

    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        let mut data = self.write()?;
        if data.format != Format::Indexed {
            return Err(DDERR_INVALIDPIXELFORMAT.into());
        }
        if let Some(palette) = palette.as_ref() {
            crate::palette::check_owner(palette, &self.display)?;
        }
        data.palette = palette.cloned();
        let locked = data.locked();
        drop(data);
        if locked {
            Ok(())
        } else {
            self.refresh()
        }
    }

    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        self.read()?
            .palette
            .clone()
            .ok_or_else(|| Error::from(DDERR_NOPALETTEATTACHED))
    }

    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        if out.dwSize as usize != size_of::<DDPIXELFORMAT>() {
            return Err(E_INVALIDARG.into());
        }
        *out = self.read()?.format.description();
        Ok(())
    }

    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        if out.dwSize as usize != size_of::<DDSURFACEDESC>() {
            return Err(E_INVALIDARG.into());
        }
        *out = self.read()?.desc(self.back.is_some());
        Ok(())
    }

    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC, flags: u32, _: HANDLE) -> Result<()> {
        if flags
            & !((DDLOCK_WAIT
                | DDLOCK_DONOTWAIT
                | DDLOCK_READONLY
                | DDLOCK_WRITEONLY
                | DDLOCK_NOSYSLOCK
                | DDLOCK_DISCARDCONTENTS
                | DDLOCK_NOOVERWRITE) as u32)
            != 0
        {
            return unsupported();
        }
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        if out.dwSize as usize != size_of::<DDSURFACEDESC>() {
            return Err(E_INVALIDARG.into());
        }
        let mut data = self.write()?;
        data.ready()?;
        let r = data.bounds(rect)?;
        *out = data.desc(self.back.is_some());
        let offset = r.top as usize * data.pitch + r.left as usize * data.format.bytes();
        out.lpSurface = data.pixels.as_mut_ptr().wrapping_add(offset).cast();
        out.dwFlags |= DDSD_LPSURFACE as u32;
        data.access = Access::Locked(offset);
        Ok(())
    }

    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        let mut data = self.write()?;
        let Access::Locked(offset) = data.access else {
            return Err(DDERR_NOTLOCKED.into());
        };
        if !address.is_null() && address != data.pixels.as_mut_ptr().wrapping_add(offset).cast() {
            return Err(E_INVALIDARG.into());
        }
        data.access = Access::Ready;
        drop(data);
        self.refresh()
    }

    fn IsLost(&self) -> Result<()> {
        Ok(())
    }
    fn Restore(&self) -> Result<()> {
        Ok(())
    }
    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        match flags as i32 {
            DDGBS_ISBLTDONE => Ok(()), // Blt always completes before returning.
            DDGBS_CANBLT => self.read()?.ready(),
            _ => Err(E_INVALIDARG.into()),
        }
    }
    fn GetFlipStatus(&self, flags: u32) -> Result<()> {
        let back = self.back.as_ref().ok_or(DDERR_NOTFLIPPABLE)?;
        match flags as i32 {
            DDGFS_ISFLIPDONE => Ok(()),
            DDGFS_CANFLIP => {
                self.read()?.ready()?;
                back.cast_object_ref::<Surface>()?.read()?.ready()
            }
            _ => Err(E_INVALIDARG.into()),
        }
    }
    fn Initialize(&self, _: Ref<IDirectDraw>, _: *mut DDSURFACEDESC) -> Result<()> {
        Err(DDERR_ALREADYINITIALIZED.into())
    }
    fn GetClipper(&self) -> Result<IDirectDrawClipper> {
        Err(DDERR_NOCLIPPERATTACHED.into())
    }
    fn SetClipper(&self, _: Ref<IDirectDrawClipper>) -> Result<()> {
        unsupported()
    }
    fn AddAttachedSurface(&self, _: Ref<IDirectDrawSurface>) -> Result<()> {
        unsupported()
    }
    fn DeleteAttachedSurface(&self, _: u32, _: Ref<IDirectDrawSurface>) -> Result<()> {
        unsupported()
    }
    fn AddOverlayDirtyRect(&self, _: *mut RECT) -> Result<()> {
        unsupported()
    }
    fn BltBatch(&self, _: *mut DDBLTBATCH, _: u32, _: u32) -> Result<()> {
        unsupported()
    }
    fn EnumOverlayZOrders(
        &self,
        _: u32,
        _: *mut c_void,
        _: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        unsupported()
    }
    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        if out.is_null() {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            out.write(HDC::default());
        }
        let mut data = self.write()?;
        if matches!(data.access, Access::Gdi(_)) {
            return Err(DDERR_DCALREADYCREATED.into());
        }
        data.ready()?;
        let dc = SurfaceDc::new(&data)?;
        unsafe {
            out.write(dc.handle);
        }
        data.access = Access::Gdi(dc);
        Ok(())
    }

    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        let mut data = self.write()?;
        let Access::Gdi(dc) = &data.access else {
            return Err(DDERR_NODC.into());
        };
        if handle != dc.handle {
            return Err(E_INVALIDARG.into());
        }
        let bits = dc.bits;
        unsafe {
            // GDI queues draws on the calling thread. Flush before reading the DIB.
            let _ = GdiFlush();
            std::ptr::copy_nonoverlapping(bits.cast(), data.pixels.as_mut_ptr(), data.pixels.len());
        }
        data.access = Access::Ready;
        drop(data);
        self.refresh()
    }
    fn GetOverlayPosition(&self, _: *mut i32, _: *mut i32) -> Result<()> {
        unsupported()
    }
    fn SetOverlayPosition(&self, _: i32, _: i32) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlay(
        &self,
        _: *mut RECT,
        _: Ref<IDirectDrawSurface>,
        _: *mut RECT,
        _: u32,
        _: *mut DDOVERLAYFX,
    ) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlayDisplay(&self, _: u32) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface>) -> Result<()> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb555_and_rgb565_expand_with_their_own_masks_and_skip_pitch_padding() {
        for (format, green_sample, green_channel, red) in [
            (Format::Rgb555, 0x0200u16, 131, 0x7c00u16),
            (Format::Rgb565, 0x0200u16, 64, 0xf800u16),
        ] {
            let mut surface = SurfaceData::new(1, 3, format, 0).unwrap();
            assert_eq!(surface.pitch, 4);
            for (row, color) in
                surface
                    .pixels
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip([red, green_sample, 0x001f])
            {
                row[..2].copy_from_slice(&color.to_le_bytes());
                row[2..].fill(0xad);
            }
            let mut rgba = Vec::new();
            frame_rgba(&surface, &mut rgba).unwrap();
            assert_eq!(
                rgba,
                [255, 0, 0, 255, 0, green_channel, 0, 255, 0, 0, 255, 255]
            );
        }
    }

    #[test]
    fn xrgb_swaps_red_and_blue_and_ignores_the_reserved_byte() {
        let mut surface = SurfaceData::new(2, 1, Format::Xrgb, 0).unwrap();
        surface
            .pixels
            .copy_from_slice(&[8, 32, 128, 0, 1, 2, 3, 96]);
        let mut rgba = Vec::new();
        frame_rgba(&surface, &mut rgba).unwrap();
        assert_eq!(rgba, [128, 32, 8, 255, 3, 2, 1, 255]);
    }
}
