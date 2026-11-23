//! CPU-backed DirectDraw 1-7 surfaces. A lock exposes stable storage until Unlock.
#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::rc::Rc;

use windows::core::{implement, Error, IUnknown, Interface, OutRef, Ref, Result, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HANDLE, RECT};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_BITFIELDS, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    PALETTEENTRY,
};

use crate::clipper;
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
const DDERR_MOREDATA: HRESULT = dd_hresult(690);

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

pub fn desc2_to_desc(d2: &DDSURFACEDESC2) -> Result<DDSURFACEDESC> {
    if d2.dwSize as usize != size_of::<DDSURFACEDESC2>() {
        return Err(E_INVALIDARG.into());
    }
    let mut d = DDSURFACEDESC {
        dwSize: size_of::<DDSURFACEDESC>() as u32,
        dwFlags: d2.dwFlags,
        dwHeight: d2.dwHeight,
        dwWidth: d2.dwWidth,
        dwBackBufferCount: unsafe { d2.Anonymous2.dwBackBufferCount },
        dwAlphaBitDepth: d2.dwAlphaBitDepth,
        dwReserved: d2.dwReserved,
        lpSurface: d2.lpSurface,
        ddckCKDestOverlay: unsafe { d2.Anonymous4.ddckCKDestOverlay },
        ddckCKDestBlt: d2.ddckCKDestBlt,
        ddckCKSrcOverlay: d2.ddckCKSrcOverlay,
        ddckCKSrcBlt: d2.ddckCKSrcBlt,
        ddpfPixelFormat: unsafe { d2.Anonymous5.ddpfPixelFormat },
        ddsCaps: DDSCAPS {
            dwCaps: d2.ddsCaps.dwCaps,
        },
        ..Default::default()
    };
    d.Anonymous1.lPitch = unsafe { d2.Anonymous1.lPitch };
    d.Anonymous2.dwMipMapCount = unsafe { d2.Anonymous3.dwMipMapCount };
    Ok(d)
}

pub fn desc_to_desc2(d: &DDSURFACEDESC) -> DDSURFACEDESC2 {
    let mut d2 = DDSURFACEDESC2 {
        dwSize: size_of::<DDSURFACEDESC2>() as u32,
        dwFlags: d.dwFlags,
        dwHeight: d.dwHeight,
        dwWidth: d.dwWidth,
        dwAlphaBitDepth: d.dwAlphaBitDepth,
        dwReserved: d.dwReserved,
        lpSurface: d.lpSurface,
        ddckCKDestBlt: d.ddckCKDestBlt,
        ddckCKSrcOverlay: d.ddckCKSrcOverlay,
        ddckCKSrcBlt: d.ddckCKSrcBlt,
        ddsCaps: DDSCAPS2 {
            dwCaps: d.ddsCaps.dwCaps,
            ..Default::default()
        },
        ..Default::default()
    };
    d2.Anonymous1.lPitch = unsafe { d.Anonymous1.lPitch };
    d2.Anonymous2.dwBackBufferCount = d.dwBackBufferCount;
    d2.Anonymous3.dwMipMapCount = unsafe { d.Anonymous2.dwMipMapCount };
    d2.Anonymous4.ddckCKDestOverlay = d.ddckCKDestOverlay;
    d2.Anonymous5.ddpfPixelFormat = d.ddpfPixelFormat;
    d2
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
            _ => Err(DDERR_INVALIDPIXELFORMAT.into()),
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
    Locked { offset: usize, rect: RECT },
    Gdi(SurfaceDc),
}

enum PrivateData {
    Bytes(Vec<u8>),
    Unknown(IUnknown),
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
    clipper: Option<IDirectDrawClipper>,
    source_key: Option<DDCOLORKEY>,
    gamma: Option<DDGAMMARAMP>,
    private_data: HashMap<GUID, PrivateData>,
    uniqueness: u32,
    priority: u32,
    lod: u32,
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
            clipper: None,
            access: Access::Ready,
            source_key: None,
            gamma: None,
            private_data: HashMap::new(),
            uniqueness: 1,
            priority: 0,
            lod: 0,
        })
    }

    pub fn byte_len(&self) -> usize {
        self.pixels.len()
    }

    pub fn has_dc(&self, hdc: HDC) -> bool {
        matches!(&self.access, Access::Gdi(dc) if dc.handle == hdc)
    }

    fn bump_uniqueness(&mut self) {
        self.uniqueness = self.uniqueness.wrapping_add(1);
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
            return Err(DDERR_INVALIDRECT.into());
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
    let gamma = data.gamma.as_ref();
    for (source, dst) in data
        .pixels
        .chunks_exact(data.pitch)
        .zip(rgba.chunks_exact_mut(width * 4))
    {
        for (pixel, out) in source[..width * bytes]
            .chunks_exact(bytes)
            .zip(dst.as_chunks_mut::<4>().0.iter_mut())
        {
            let mut rgb = match data.format {
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
            if let Some(ramp) = gamma {
                rgb = [
                    (ramp.red[rgb[0] as usize] >> 8) as u8,
                    (ramp.green[rgb[1] as usize] >> 8) as u8,
                    (ramp.blue[rgb[2] as usize] >> 8) as u8,
                ];
            }
            out[..3].copy_from_slice(&rgb);
            out[3] = 255;
        }
    }
    Ok(())
}

fn identity_gamma_ramp() -> DDGAMMARAMP {
    let mut ramp = DDGAMMARAMP::default();
    for i in 0..256 {
        let value = (i * 257) as u16;
        ramp.red[i] = value;
        ramp.green[i] = value;
        ramp.blue[i] = value;
    }
    ramp
}

#[implement(
    IDirectDrawSurface,
    IDirectDrawSurface2,
    IDirectDrawSurface3,
    IDirectDrawSurface4,
    IDirectDrawSurface7,
    IDirectDrawGammaControl,
    Agile = false
)]
struct Surface {
    parent: Option<IDirectDraw>,
    display: Rc<RefCell<Display>>,
    data: Rc<RefCell<SurfaceData>>,
    back: Option<IDirectDrawSurface>,
    self_raw: Cell<*mut c_void>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        let raw = self.self_raw.get();
        if !raw.is_null() {
            if let Ok(mut d) = self.display.try_borrow_mut() {
                d.unregister_surface(raw);
            }
        }
    }
}

fn register_surface_object(
    parent: Option<IDirectDraw>,
    display: Rc<RefCell<Display>>,
    data: Rc<RefCell<SurfaceData>>,
    back: Option<IDirectDrawSurface>,
) -> Result<IDirectDrawSurface> {
    let iface: IDirectDrawSurface = Surface {
        parent,
        display: display.clone(),
        data: data.clone(),
        back,
        self_raw: Cell::new(std::ptr::null_mut()),
    }
    .into();
    let raw = iface.as_raw();
    iface.cast_object_ref::<Surface>()?.self_raw.set(raw);
    display
        .try_borrow_mut()
        .map_err(|_| DDERR_SURFACEBUSY)?
        .register_surface(raw, Rc::downgrade(&data));
    Ok(iface)
}

pub fn create_surface(
    parent: Option<IDirectDraw>,
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
        let back_data = Rc::new(RefCell::new(SurfaceData::new(
            width,
            height,
            format,
            (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP | DDSCAPS_SYSTEMMEMORY) as u32,
        )?));
        Some(register_surface_object(
            parent.clone(),
            display.clone(),
            back_data,
            None,
        )?)
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
    register_surface_object(parent, display, data, back)
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
        source: &Surface_Impl,
        src_rect: *const RECT,
        dest_rect: RECT,
        keyed: bool,
    ) -> Result<()> {
        let src = source.read()?;
        src.ready()?;
        let src_rect = src.bounds(src_rect)?;
        let format = src.format;
        let key = if keyed {
            Some(src.source_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        // Snapshot overlapping self-blits before borrowing the destination mutably.
        if Rc::ptr_eq(&source.data, &self.data) {
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
            let mut dst = self.write()?;
            blit(&mut dst, dest_rect, &snapshot, row_bytes, local, key);
            dst.bump_uniqueness();
        } else {
            let mut dst = self.write()?;
            dst.ready()?;
            if dst.format != format {
                return unsupported();
            }
            blit(&mut dst, dest_rect, &src.pixels, src.pitch, src_rect, key);
            dst.bump_uniqueness();
        }
        Ok(())
    }

    fn blt_colorfill_rect(&self, rect: RECT, color: [u8; 4]) -> Result<()> {
        let mut dst = self.write()?;
        dst.ready()?;
        let bytes = dst.format.bytes();
        let pitch = dst.pitch;
        for y in rect.top as usize..rect.bottom as usize {
            let row = &mut dst.pixels
                [y * pitch + rect.left as usize * bytes..y * pitch + rect.right as usize * bytes];
            for pixel in row.chunks_exact_mut(bytes) {
                pixel.copy_from_slice(&color[..bytes]);
            }
        }
        dst.bump_uniqueness();
        Ok(())
    }

    fn blt_impl<S: Interface>(
        &self,
        dest: *mut RECT,
        source: Ref<S>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        if flags & !((DDBLT_WAIT | DDBLT_DONOTWAIT | DDBLT_COLORFILL | DDBLT_KEYSRC) as u32) != 0 {
            return unsupported();
        }
        let rect = self.read()?.bounds(dest)?;
        let clip_rects = {
            let clipper_opt = self.read()?.clipper.clone();
            match clipper_opt {
                Some(ref c) => clipper::clip_rects(c, rect)?,
                None => None,
            }
        };

        if flags & DDBLT_COLORFILL as u32 != 0 {
            if !source.is_null() || flags & DDBLT_KEYSRC as u32 != 0 {
                return Err(E_INVALIDARG.into());
            }
            let fx = unsafe { fx.as_ref() }.ok_or(E_INVALIDARG)?;
            if fx.dwSize as usize != size_of::<DDBLTFX>() {
                return Err(E_INVALIDARG.into());
            }
            let color = unsafe { fx.Anonymous5.dwFillColor }.to_le_bytes();
            match clip_rects {
                Some(rects) => {
                    for sub in rects {
                        self.blt_colorfill_rect(sub, color)?;
                    }
                }
                None => self.blt_colorfill_rect(rect, color)?,
            }
            self.refresh()
        } else {
            let source_obj = source.ok()?.cast_object_ref::<Surface>()?;
            let keyed = flags & DDBLT_KEYSRC as u32 != 0;
            match clip_rects {
                Some(rects) => {
                    if !rects.is_empty() {
                        let full_src = source_obj.read()?.bounds(src)?;
                        let dw = (rect.right - rect.left) as i64;
                        let dh = (rect.bottom - rect.top) as i64;
                        let sw = (full_src.right - full_src.left) as i64;
                        let sh = (full_src.bottom - full_src.top) as i64;
                        for sub in rects {
                            let s_left =
                                full_src.left + ((sub.left - rect.left) as i64 * sw / dw) as i32;
                            let s_top =
                                full_src.top + ((sub.top - rect.top) as i64 * sh / dh) as i32;
                            let s_right = (full_src.left
                                + (((sub.right - rect.left) as i64 * sw + dw - 1) / dw) as i32)
                                .max(s_left + 1)
                                .min(full_src.right);
                            let s_bottom = (full_src.top
                                + (((sub.bottom - rect.top) as i64 * sh + dh - 1) / dh) as i32)
                                .max(s_top + 1)
                                .min(full_src.bottom);
                            let sub_src = RECT {
                                left: s_left,
                                top: s_top,
                                right: s_right,
                                bottom: s_bottom,
                            };
                            self.copy(source_obj, &sub_src, sub, keyed)?;
                        }
                    }
                }
                None => self.copy(source_obj, src, rect, keyed)?,
            }
            self.refresh()
        }
    }

    fn blt_fast_impl<S: Interface>(
        &self,
        x: u32,
        y: u32,
        source: Ref<S>,
        src_rect: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        if flags & !(DDBLTFAST_WAIT | DDBLTFAST_DONOTWAIT | DDBLTFAST_SRCCOLORKEY) != 0 {
            return unsupported();
        }
        let source_obj = source.ok()?.cast_object_ref::<Surface>()?;
        let s = source_obj.read()?.bounds(src_rect)?;
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
        self.copy(source_obj, &s, rect, flags & DDBLTFAST_SRCCOLORKEY != 0)?;
        self.refresh()
    }

    fn flip_impl<S: Interface>(&self, target: Ref<S>, flags: u32) -> Result<()> {
        if flags & !((DDFLIP_WAIT | DDFLIP_DONOTWAIT | DDFLIP_NOVSYNC) as u32) != 0 {
            return unsupported();
        }
        let back = self.back.as_ref().ok_or(DDERR_NOTFLIPPABLE)?;
        let object = back.cast_object_ref::<Surface>()?;
        if let Some(t) = target.as_ref() {
            let t_obj = t
                .cast_object_ref::<Surface>()
                .map_err(|_| DDERR_SURFACENOTATTACHED)?;
            if !Rc::ptr_eq(&t_obj.data, &object.data) {
                return Err(DDERR_SURFACENOTATTACHED.into());
            }
        }
        {
            let mut front = self.write()?;
            let mut back_data = object.write()?;
            front.ready()?;
            back_data.ready()?;
            std::mem::swap(&mut front.pixels, &mut back_data.pixels);
            front.bump_uniqueness();
            back_data.bump_uniqueness();
        }
        self.refresh()
    }

    fn get_attached_surface_impl<S>(&self, caps_val: u32, out: OutRef<S>) -> Result<()>
    where
        S: Interface + windows_core::Type<S, Default = Option<S>>,
    {
        let attachment = (|| -> Result<S> {
            let back = self.back.as_ref().ok_or(DDERR_NOTFOUND)?;
            let actual = back.cast_object_ref::<Surface>()?.read()?.caps;
            if caps_val == 0 || actual & caps_val != caps_val {
                return Err(DDERR_NOTFOUND.into());
            }
            back.cast::<S>()
        })();
        match attachment {
            Ok(back) => out.write(Some(back)),
            Err(error) => {
                out.write(None)?;
                Err(error)
            }
        }
    }

    fn lock_desc(&self, rect: *mut RECT, flags: u32) -> Result<DDSURFACEDESC> {
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
        let mut data = self.write()?;
        data.ready()?;
        let r = data.bounds(rect)?;
        let mut desc = data.desc(self.back.is_some());
        let offset = r.top as usize * data.pitch + r.left as usize * data.format.bytes();
        desc.lpSurface = data.pixels.as_mut_ptr().wrapping_add(offset).cast();
        desc.dwFlags |= DDSD_LPSURFACE as u32;
        data.access = Access::Locked { offset, rect: r };
        Ok(desc)
    }

    fn unlock_ptr_impl(&self, address: *mut c_void) -> Result<()> {
        let mut data = self.write()?;
        let Access::Locked { offset, .. } = data.access else {
            return Err(DDERR_NOTLOCKED.into());
        };
        if !address.is_null() && address != data.pixels.as_mut_ptr().wrapping_add(offset).cast() {
            return Err(E_INVALIDARG.into());
        }
        data.access = Access::Ready;
        data.bump_uniqueness();
        drop(data);
        self.refresh()
    }

    fn unlock_rect_impl(&self, rect: *mut RECT) -> Result<()> {
        let mut data = self.write()?;
        let Access::Locked { rect: locked, .. } = data.access else {
            return Err(DDERR_NOTLOCKED.into());
        };
        if let Some(r) = unsafe { rect.as_ref() } {
            if *r != locked {
                return Err(DDERR_INVALIDRECT.into());
            }
        }
        data.access = Access::Ready;
        data.bump_uniqueness();
        drop(data);
        self.refresh()
    }

    fn get_clipper_impl(&self) -> Result<IDirectDrawClipper> {
        self.read()?
            .clipper
            .clone()
            .ok_or_else(|| Error::from(DDERR_NOCLIPPERATTACHED))
    }

    fn set_clipper_impl(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.write()?.clipper = clipper.cloned();
        Ok(())
    }

    fn get_dd_interface_impl(&self, out: *mut *mut c_void) -> Result<()> {
        if out.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { out.write(std::ptr::null_mut()) };
        let parent = self.parent.clone().ok_or(E_INVALIDARG)?;
        unsafe { out.write(parent.into_raw()) };
        Ok(())
    }

    fn set_surface_desc_impl(&self, desc: &DDSURFACEDESC, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut data = self.write()?;
        data.ready()?;
        if desc.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
            data.format = Format::from_dd(&desc.ddpfPixelFormat)?;
        }
        if desc.dwFlags & DDSD_WIDTH as u32 != 0 {
            if desc.dwWidth == 0 || desc.dwWidth > i32::MAX as u32 {
                return Err(E_INVALIDARG.into());
            }
            data.width = desc.dwWidth;
        }
        if desc.dwFlags & DDSD_HEIGHT as u32 != 0 {
            if desc.dwHeight == 0 || desc.dwHeight > i32::MAX as u32 {
                return Err(E_INVALIDARG.into());
            }
            data.height = desc.dwHeight;
        }
        let min_pitch = (data.width as usize)
            .checked_mul(data.format.bytes())
            .ok_or(E_OUTOFMEMORY)?;
        let pitch = if desc.dwFlags & DDSD_PITCH as u32 != 0 {
            let p = unsafe { desc.Anonymous1.lPitch };
            if p <= 0 || (p as usize) < min_pitch {
                return Err(E_INVALIDARG.into());
            }
            p as usize
        } else {
            min_pitch
                .checked_add(3)
                .map(|v| v & !3)
                .filter(|v| *v <= i32::MAX as usize)
                .ok_or(E_OUTOFMEMORY)?
        };
        let len = pitch
            .checked_mul(data.height as usize)
            .ok_or(E_OUTOFMEMORY)?;
        data.pitch = pitch;
        data.pixels = zeros(len)?;
        if desc.dwFlags & DDSD_LPSURFACE as u32 != 0 && !desc.lpSurface.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    desc.lpSurface.cast::<u8>(),
                    data.pixels.as_mut_ptr(),
                    len,
                );
            }
        }
        data.bump_uniqueness();
        Ok(())
    }

    fn set_private_data_impl(
        &self,
        tag: *const GUID,
        data_ptr: *mut c_void,
        size: u32,
        flags: u32,
    ) -> Result<()> {
        let tag = unsafe { tag.as_ref() }.ok_or(E_INVALIDARG)?;
        let valid_flags = (DDSPD_IUNKNOWNPOINTER | DDSPD_VOLATILE) as u32;
        if flags & !valid_flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if data_ptr.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let entry = if flags & DDSPD_IUNKNOWNPOINTER as u32 != 0 {
            if size as usize != size_of::<*mut c_void>() {
                return Err(E_INVALIDARG.into());
            }
            let unk = unsafe { IUnknown::from_raw_borrowed(&data_ptr) }
                .ok_or(E_INVALIDARG)?
                .clone();
            PrivateData::Unknown(unk)
        } else {
            let slice = unsafe { std::slice::from_raw_parts(data_ptr.cast::<u8>(), size as usize) };
            PrivateData::Bytes(slice.to_vec())
        };
        self.write()?.private_data.insert(*tag, entry);
        Ok(())
    }

    fn get_private_data_impl(
        &self,
        tag: *const GUID,
        buffer: *mut c_void,
        size: *mut u32,
    ) -> Result<()> {
        let tag = unsafe { tag.as_ref() }.ok_or(E_INVALIDARG)?;
        let size = unsafe { size.as_mut() }.ok_or(E_INVALIDARG)?;
        let data = self.read()?;
        let entry = data.private_data.get(tag).ok_or(DDERR_NOTFOUND)?;
        match entry {
            PrivateData::Bytes(bytes) => {
                let needed = bytes.len() as u32;
                if *size < needed || buffer.is_null() {
                    *size = needed;
                    return Err(DDERR_MOREDATA.into());
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), bytes.len());
                }
                *size = needed;
                Ok(())
            }
            PrivateData::Unknown(unk) => {
                let needed = size_of::<*mut c_void>() as u32;
                if *size < needed || buffer.is_null() {
                    *size = needed;
                    return Err(DDERR_MOREDATA.into());
                }
                unsafe {
                    buffer.cast::<*mut c_void>().write(unk.clone().into_raw());
                }
                *size = needed;
                Ok(())
            }
        }
    }

    fn free_private_data_impl(&self, tag: *const GUID) -> Result<()> {
        let tag = unsafe { tag.as_ref() }.ok_or(E_INVALIDARG)?;
        self.write()?.private_data.remove(tag);
        Ok(())
    }

    fn get_uniqueness_value_impl(&self, out: *mut u32) -> Result<()> {
        *unsafe { out.as_mut() }.ok_or(E_INVALIDARG)? = self.read()?.uniqueness;
        Ok(())
    }

    fn change_uniqueness_value_impl(&self) -> Result<()> {
        self.write()?.bump_uniqueness();
        Ok(())
    }

    fn set_priority_impl(&self, priority: u32) -> Result<()> {
        self.write()?.priority = priority;
        Ok(())
    }

    fn get_priority_impl(&self, out: *mut u32) -> Result<()> {
        *unsafe { out.as_mut() }.ok_or(E_INVALIDARG)? = self.read()?.priority;
        Ok(())
    }

    fn set_lod_impl(&self, lod: u32) -> Result<()> {
        self.write()?.lod = lod;
        Ok(())
    }

    fn get_lod_impl(&self, out: *mut u32) -> Result<()> {
        *unsafe { out.as_mut() }.ok_or(E_INVALIDARG)? = self.read()?.lod;
        Ok(())
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

impl IDirectDrawGammaControl_Impl for Surface_Impl {
    fn GetGammaRamp(&self, flags: u32, ramp: *mut DDGAMMARAMP) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let out = unsafe { ramp.as_mut() }.ok_or(E_INVALIDARG)?;
        *out = self.read()?.gamma.unwrap_or_else(identity_gamma_ramp);
        Ok(())
    }

    fn SetGammaRamp(&self, flags: u32, ramp: *mut DDGAMMARAMP) -> Result<()> {
        if flags & !(DDSGR_CALIBRATE as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        let ramp = unsafe { ramp.as_ref() }.ok_or(E_INVALIDARG)?;
        self.write()?.gamma = Some(*ramp);
        self.refresh()
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
        self.blt_impl(dest, source, src, flags, fx)
    }

    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface>,
        src_rect: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        self.blt_fast_impl(x, y, source, src_rect, flags)
    }

    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS,
        out: OutRef<IDirectDrawSurface>,
    ) -> Result<()> {
        let caps_val = unsafe { caps.as_ref() }.ok_or(E_INVALIDARG)?.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }

    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        if let Some(s) = &self.back {
            let mut desc = s.cast_object_ref::<Surface>()?.read()?.desc(false);
            let owned = ManuallyDrop::new(s.clone());
            unsafe {
                callback((&*owned).into(), &mut desc, context).ok()?;
            }
        }
        Ok(())
    }

    fn Flip(&self, target: Ref<IDirectDrawSurface>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
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
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        if out.dwSize as usize != size_of::<DDSURFACEDESC>() {
            return Err(E_INVALIDARG.into());
        }
        *out = self.lock_desc(rect, flags)?;
        Ok(())
    }

    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        self.unlock_ptr_impl(address)
    }

    fn IsLost(&self) -> Result<()> {
        Ok(())
    }
    fn Restore(&self) -> Result<()> {
        Ok(())
    }
    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        match flags as i32 {
            DDGBS_ISBLTDONE => Ok(()),
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
        self.get_clipper_impl()
    }
    fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.set_clipper_impl(clipper)
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
        data.bump_uniqueness();
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

impl IDirectDrawSurface2_Impl for Surface_Impl {
    fn AddAttachedSurface(&self, _: Ref<IDirectDrawSurface2>) -> Result<()> {
        unsupported()
    }
    fn AddOverlayDirtyRect(&self, r: *mut RECT) -> Result<()> {
        IDirectDrawSurface_Impl::AddOverlayDirtyRect(self, r)
    }
    fn Blt(
        &self,
        dest: *mut RECT,
        source: Ref<IDirectDrawSurface2>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        self.blt_impl(dest, source, src, flags, fx)
    }
    fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::BltBatch(self, batch, count, flags)
    }
    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface2>,
        src: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        self.blt_fast_impl(x, y, source, src, flags)
    }
    fn DeleteAttachedSurface(&self, _: u32, _: Ref<IDirectDrawSurface2>) -> Result<()> {
        unsupported()
    }
    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        IDirectDrawSurface_Impl::EnumAttachedSurfaces(self, context, callback)
    }
    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        IDirectDrawSurface_Impl::EnumOverlayZOrders(self, flags, context, callback)
    }
    fn Flip(&self, target: Ref<IDirectDrawSurface2>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
    }
    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS,
        out: OutRef<IDirectDrawSurface2>,
    ) -> Result<()> {
        let caps_val = unsafe { caps.as_ref() }.ok_or(E_INVALIDARG)?.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }
    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetBltStatus(self, flags)
    }
    fn GetCaps(&self, caps: *mut DDSCAPS) -> Result<()> {
        IDirectDrawSurface_Impl::GetCaps(self, caps)
    }
    fn GetClipper(&self) -> Result<IDirectDrawClipper> {
        self.get_clipper_impl()
    }
    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::GetColorKey(self, flags, key)
    }
    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        IDirectDrawSurface_Impl::GetDC(self, out)
    }
    fn GetFlipStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetFlipStatus(self, flags)
    }
    fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        IDirectDrawSurface_Impl::GetOverlayPosition(self, x, y)
    }
    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        IDirectDrawSurface_Impl::GetPalette(self)
    }
    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        IDirectDrawSurface_Impl::GetPixelFormat(self, out)
    }
    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC) -> Result<()> {
        IDirectDrawSurface_Impl::GetSurfaceDesc(self, out)
    }
    fn Initialize(&self, dd: Ref<IDirectDraw>, desc: *mut DDSURFACEDESC) -> Result<()> {
        IDirectDrawSurface_Impl::Initialize(self, dd, desc)
    }
    fn IsLost(&self) -> Result<()> {
        IDirectDrawSurface_Impl::IsLost(self)
    }
    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC, flags: u32, ev: HANDLE) -> Result<()> {
        IDirectDrawSurface_Impl::Lock(self, rect, out, flags, ev)
    }
    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        IDirectDrawSurface_Impl::ReleaseDC(self, handle)
    }
    fn Restore(&self) -> Result<()> {
        IDirectDrawSurface_Impl::Restore(self)
    }
    fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.set_clipper_impl(clipper)
    }
    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::SetColorKey(self, flags, key)
    }
    fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
        IDirectDrawSurface_Impl::SetOverlayPosition(self, x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        self.unlock_ptr_impl(address)
    }
    fn UpdateOverlay(
        &self,
        _: *mut RECT,
        _: Ref<IDirectDrawSurface2>,
        _: *mut RECT,
        _: u32,
        _: *mut DDOVERLAYFX,
    ) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface2>) -> Result<()> {
        unsupported()
    }
    fn GetDDInterface(&self, out: *mut *mut c_void) -> Result<()> {
        self.get_dd_interface_impl(out)
    }
    fn PageLock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn PageUnlock(&self, _: u32) -> Result<()> {
        Ok(())
    }
}

impl IDirectDrawSurface3_Impl for Surface_Impl {
    fn AddAttachedSurface(&self, _: Ref<IDirectDrawSurface3>) -> Result<()> {
        unsupported()
    }
    fn AddOverlayDirtyRect(&self, r: *mut RECT) -> Result<()> {
        IDirectDrawSurface_Impl::AddOverlayDirtyRect(self, r)
    }
    fn Blt(
        &self,
        dest: *mut RECT,
        source: Ref<IDirectDrawSurface3>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        self.blt_impl(dest, source, src, flags, fx)
    }
    fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::BltBatch(self, batch, count, flags)
    }
    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface3>,
        src: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        self.blt_fast_impl(x, y, source, src, flags)
    }
    fn DeleteAttachedSurface(&self, _: u32, _: Ref<IDirectDrawSurface3>) -> Result<()> {
        unsupported()
    }
    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        IDirectDrawSurface_Impl::EnumAttachedSurfaces(self, context, callback)
    }
    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        IDirectDrawSurface_Impl::EnumOverlayZOrders(self, flags, context, callback)
    }
    fn Flip(&self, target: Ref<IDirectDrawSurface3>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
    }
    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS,
        out: OutRef<IDirectDrawSurface3>,
    ) -> Result<()> {
        let caps_val = unsafe { caps.as_ref() }.ok_or(E_INVALIDARG)?.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }
    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetBltStatus(self, flags)
    }
    fn GetCaps(&self, caps: *mut DDSCAPS) -> Result<()> {
        IDirectDrawSurface_Impl::GetCaps(self, caps)
    }
    fn GetClipper(&self) -> Result<IDirectDrawClipper> {
        self.get_clipper_impl()
    }
    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::GetColorKey(self, flags, key)
    }
    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        IDirectDrawSurface_Impl::GetDC(self, out)
    }
    fn GetFlipStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetFlipStatus(self, flags)
    }
    fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        IDirectDrawSurface_Impl::GetOverlayPosition(self, x, y)
    }
    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        IDirectDrawSurface_Impl::GetPalette(self)
    }
    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        IDirectDrawSurface_Impl::GetPixelFormat(self, out)
    }
    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC) -> Result<()> {
        IDirectDrawSurface_Impl::GetSurfaceDesc(self, out)
    }
    fn Initialize(&self, dd: Ref<IDirectDraw>, desc: *mut DDSURFACEDESC) -> Result<()> {
        IDirectDrawSurface_Impl::Initialize(self, dd, desc)
    }
    fn IsLost(&self) -> Result<()> {
        IDirectDrawSurface_Impl::IsLost(self)
    }
    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC, flags: u32, ev: HANDLE) -> Result<()> {
        IDirectDrawSurface_Impl::Lock(self, rect, out, flags, ev)
    }
    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        IDirectDrawSurface_Impl::ReleaseDC(self, handle)
    }
    fn Restore(&self) -> Result<()> {
        IDirectDrawSurface_Impl::Restore(self)
    }
    fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.set_clipper_impl(clipper)
    }
    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::SetColorKey(self, flags, key)
    }
    fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
        IDirectDrawSurface_Impl::SetOverlayPosition(self, x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        self.unlock_ptr_impl(address)
    }
    fn UpdateOverlay(
        &self,
        _: *mut RECT,
        _: Ref<IDirectDrawSurface3>,
        _: *mut RECT,
        _: u32,
        _: *mut DDOVERLAYFX,
    ) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface3>) -> Result<()> {
        unsupported()
    }
    fn GetDDInterface(&self, out: *mut *mut c_void) -> Result<()> {
        self.get_dd_interface_impl(out)
    }
    fn PageLock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn PageUnlock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn SetSurfaceDesc(&self, desc: *mut DDSURFACEDESC, flags: u32) -> Result<()> {
        let desc = unsafe { desc.as_ref() }.ok_or(E_INVALIDARG)?;
        if desc.dwSize as usize != size_of::<DDSURFACEDESC>() {
            return Err(E_INVALIDARG.into());
        }
        self.set_surface_desc_impl(desc, flags)
    }
}

impl IDirectDrawSurface4_Impl for Surface_Impl {
    fn AddAttachedSurface(&self, _: Ref<IDirectDrawSurface4>) -> Result<()> {
        unsupported()
    }
    fn AddOverlayDirtyRect(&self, r: *mut RECT) -> Result<()> {
        IDirectDrawSurface_Impl::AddOverlayDirtyRect(self, r)
    }
    fn Blt(
        &self,
        dest: *mut RECT,
        source: Ref<IDirectDrawSurface4>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        self.blt_impl(dest, source, src, flags, fx)
    }
    fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::BltBatch(self, batch, count, flags)
    }
    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface4>,
        src: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        self.blt_fast_impl(x, y, source, src, flags)
    }
    fn DeleteAttachedSurface(&self, _: u32, _: Ref<IDirectDrawSurface4>) -> Result<()> {
        unsupported()
    }
    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        if let Some(s) = &self.back {
            let mut desc = desc_to_desc2(&s.cast_object_ref::<Surface>()?.read()?.desc(false));
            let owned = ManuallyDrop::new(s.cast::<IDirectDrawSurface4>()?);
            unsafe {
                callback((&*owned).into(), &mut desc, context).ok()?;
            }
        }
        Ok(())
    }
    fn EnumOverlayZOrders(
        &self,
        _: u32,
        _: *mut c_void,
        _: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        unsupported()
    }
    fn Flip(&self, target: Ref<IDirectDrawSurface4>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
    }
    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS2,
        out: OutRef<IDirectDrawSurface4>,
    ) -> Result<()> {
        let caps_val = unsafe { caps.as_ref() }.ok_or(E_INVALIDARG)?.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }
    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetBltStatus(self, flags)
    }
    fn GetCaps(&self, caps: *mut DDSCAPS2) -> Result<()> {
        let out = unsafe { caps.as_mut() }.ok_or(E_INVALIDARG)?;
        *out = DDSCAPS2 {
            dwCaps: self.read()?.caps,
            ..Default::default()
        };
        Ok(())
    }
    fn GetClipper(&self) -> Result<IDirectDrawClipper> {
        self.get_clipper_impl()
    }
    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::GetColorKey(self, flags, key)
    }
    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        IDirectDrawSurface_Impl::GetDC(self, out)
    }
    fn GetFlipStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetFlipStatus(self, flags)
    }
    fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        IDirectDrawSurface_Impl::GetOverlayPosition(self, x, y)
    }
    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        IDirectDrawSurface_Impl::GetPalette(self)
    }
    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        IDirectDrawSurface_Impl::GetPixelFormat(self, out)
    }
    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC2) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        if out.dwSize as usize != size_of::<DDSURFACEDESC2>() {
            return Err(E_INVALIDARG.into());
        }
        *out = desc_to_desc2(&self.read()?.desc(self.back.is_some()));
        Ok(())
    }
    fn Initialize(&self, _: Ref<IDirectDraw>, _: *mut DDSURFACEDESC2) -> Result<()> {
        Err(DDERR_ALREADYINITIALIZED.into())
    }
    fn IsLost(&self) -> Result<()> {
        IDirectDrawSurface_Impl::IsLost(self)
    }
    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC2, flags: u32, _: HANDLE) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        if out.dwSize as usize != size_of::<DDSURFACEDESC2>() {
            return Err(E_INVALIDARG.into());
        }
        *out = desc_to_desc2(&self.lock_desc(rect, flags)?);
        Ok(())
    }
    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        IDirectDrawSurface_Impl::ReleaseDC(self, handle)
    }
    fn Restore(&self) -> Result<()> {
        IDirectDrawSurface_Impl::Restore(self)
    }
    fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.set_clipper_impl(clipper)
    }
    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::SetColorKey(self, flags, key)
    }
    fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
        IDirectDrawSurface_Impl::SetOverlayPosition(self, x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, rect: *mut RECT) -> Result<()> {
        self.unlock_rect_impl(rect)
    }
    fn UpdateOverlay(
        &self,
        _: *mut RECT,
        _: Ref<IDirectDrawSurface4>,
        _: *mut RECT,
        _: u32,
        _: *mut DDOVERLAYFX,
    ) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface4>) -> Result<()> {
        unsupported()
    }
    fn GetDDInterface(&self, out: *mut *mut c_void) -> Result<()> {
        self.get_dd_interface_impl(out)
    }
    fn PageLock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn PageUnlock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn SetSurfaceDesc(&self, desc: *mut DDSURFACEDESC2, flags: u32) -> Result<()> {
        let d2 = unsafe { desc.as_ref() }.ok_or(E_INVALIDARG)?;
        let d = desc2_to_desc(d2)?;
        self.set_surface_desc_impl(&d, flags)
    }
    fn SetPrivateData(
        &self,
        tag: *const GUID,
        data: *mut c_void,
        size: u32,
        flags: u32,
    ) -> Result<()> {
        self.set_private_data_impl(tag, data, size, flags)
    }
    fn GetPrivateData(&self, tag: *const GUID, buffer: *mut c_void, size: *mut u32) -> Result<()> {
        self.get_private_data_impl(tag, buffer, size)
    }
    fn FreePrivateData(&self, tag: *const GUID) -> Result<()> {
        self.free_private_data_impl(tag)
    }
    fn GetUniquenessValue(&self, out: *mut u32) -> Result<()> {
        self.get_uniqueness_value_impl(out)
    }
    fn ChangeUniquenessValue(&self) -> Result<()> {
        self.change_uniqueness_value_impl()
    }
}

impl IDirectDrawSurface7_Impl for Surface_Impl {
    fn AddAttachedSurface(&self, _: Ref<IDirectDrawSurface7>) -> Result<()> {
        unsupported()
    }
    fn AddOverlayDirtyRect(&self, r: *mut RECT) -> Result<()> {
        IDirectDrawSurface_Impl::AddOverlayDirtyRect(self, r)
    }
    fn Blt(
        &self,
        dest: *mut RECT,
        source: Ref<IDirectDrawSurface7>,
        src: *mut RECT,
        flags: u32,
        fx: *mut DDBLTFX,
    ) -> Result<()> {
        self.blt_impl(dest, source, src, flags, fx)
    }
    fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::BltBatch(self, batch, count, flags)
    }
    fn BltFast(
        &self,
        x: u32,
        y: u32,
        source: Ref<IDirectDrawSurface7>,
        src: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        self.blt_fast_impl(x, y, source, src, flags)
    }
    fn DeleteAttachedSurface(&self, _: u32, _: Ref<IDirectDrawSurface7>) -> Result<()> {
        unsupported()
    }
    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        if let Some(s) = &self.back {
            let mut desc = desc_to_desc2(&s.cast_object_ref::<Surface>()?.read()?.desc(false));
            let owned = ManuallyDrop::new(s.cast::<IDirectDrawSurface7>()?);
            unsafe {
                callback((&*owned).into(), &mut desc, context).ok()?;
            }
        }
        Ok(())
    }
    fn EnumOverlayZOrders(
        &self,
        _: u32,
        _: *mut c_void,
        _: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        unsupported()
    }
    fn Flip(&self, target: Ref<IDirectDrawSurface7>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
    }
    fn GetAttachedSurface(
        &self,
        caps: *mut DDSCAPS2,
        out: OutRef<IDirectDrawSurface7>,
    ) -> Result<()> {
        let caps_val = unsafe { caps.as_ref() }.ok_or(E_INVALIDARG)?.dwCaps;
        self.get_attached_surface_impl(caps_val, out)
    }
    fn GetBltStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetBltStatus(self, flags)
    }
    fn GetCaps(&self, caps: *mut DDSCAPS2) -> Result<()> {
        IDirectDrawSurface4_Impl::GetCaps(self, caps)
    }
    fn GetClipper(&self) -> Result<IDirectDrawClipper> {
        self.get_clipper_impl()
    }
    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::GetColorKey(self, flags, key)
    }
    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        IDirectDrawSurface_Impl::GetDC(self, out)
    }
    fn GetFlipStatus(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::GetFlipStatus(self, flags)
    }
    fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        IDirectDrawSurface_Impl::GetOverlayPosition(self, x, y)
    }
    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        IDirectDrawSurface_Impl::GetPalette(self)
    }
    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        IDirectDrawSurface_Impl::GetPixelFormat(self, out)
    }
    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC2) -> Result<()> {
        IDirectDrawSurface4_Impl::GetSurfaceDesc(self, out)
    }
    fn Initialize(&self, dd: Ref<IDirectDraw>, desc: *mut DDSURFACEDESC2) -> Result<()> {
        IDirectDrawSurface4_Impl::Initialize(self, dd, desc)
    }
    fn IsLost(&self) -> Result<()> {
        IDirectDrawSurface_Impl::IsLost(self)
    }
    fn Lock(
        &self,
        rect: *mut RECT,
        out: *mut DDSURFACEDESC2,
        flags: u32,
        ev: HANDLE,
    ) -> Result<()> {
        IDirectDrawSurface4_Impl::Lock(self, rect, out, flags, ev)
    }
    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        IDirectDrawSurface_Impl::ReleaseDC(self, handle)
    }
    fn Restore(&self) -> Result<()> {
        IDirectDrawSurface_Impl::Restore(self)
    }
    fn SetClipper(&self, clipper: Ref<IDirectDrawClipper>) -> Result<()> {
        self.set_clipper_impl(clipper)
    }
    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        IDirectDrawSurface_Impl::SetColorKey(self, flags, key)
    }
    fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
        IDirectDrawSurface_Impl::SetOverlayPosition(self, x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, rect: *mut RECT) -> Result<()> {
        self.unlock_rect_impl(rect)
    }
    fn UpdateOverlay(
        &self,
        _: *mut RECT,
        _: Ref<IDirectDrawSurface7>,
        _: *mut RECT,
        _: u32,
        _: *mut DDOVERLAYFX,
    ) -> Result<()> {
        unsupported()
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface7>) -> Result<()> {
        unsupported()
    }
    fn GetDDInterface(&self, out: *mut *mut c_void) -> Result<()> {
        self.get_dd_interface_impl(out)
    }
    fn PageLock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn PageUnlock(&self, _: u32) -> Result<()> {
        Ok(())
    }
    fn SetSurfaceDesc(&self, desc: *mut DDSURFACEDESC2, flags: u32) -> Result<()> {
        IDirectDrawSurface4_Impl::SetSurfaceDesc(self, desc, flags)
    }
    fn SetPrivateData(
        &self,
        tag: *const GUID,
        data: *mut c_void,
        size: u32,
        flags: u32,
    ) -> Result<()> {
        self.set_private_data_impl(tag, data, size, flags)
    }
    fn GetPrivateData(&self, tag: *const GUID, buffer: *mut c_void, size: *mut u32) -> Result<()> {
        self.get_private_data_impl(tag, buffer, size)
    }
    fn FreePrivateData(&self, tag: *const GUID) -> Result<()> {
        self.free_private_data_impl(tag)
    }
    fn GetUniquenessValue(&self, out: *mut u32) -> Result<()> {
        self.get_uniqueness_value_impl(out)
    }
    fn ChangeUniquenessValue(&self) -> Result<()> {
        self.change_uniqueness_value_impl()
    }
    fn SetPriority(&self, priority: u32) -> Result<()> {
        self.set_priority_impl(priority)
    }
    fn GetPriority(&self, out: *mut u32) -> Result<()> {
        self.get_priority_impl(out)
    }
    fn SetLOD(&self, lod: u32) -> Result<()> {
        self.set_lod_impl(lod)
    }
    fn GetLOD(&self, out: *mut u32) -> Result<()> {
        self.get_lod_impl(out)
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

    #[test]
    fn gamma_ramp_remaps_expanded_rgba_channels() {
        let mut surface = SurfaceData::new(1, 1, Format::Xrgb, 0).unwrap();
        surface.pixels.copy_from_slice(&[10, 20, 30, 0]);
        let mut ramp = identity_gamma_ramp();
        ramp.red[30] = 0x8000;
        ramp.green[20] = 0x4000;
        ramp.blue[10] = 0x2000;
        surface.gamma = Some(ramp);
        let mut rgba = Vec::new();
        frame_rgba(&surface, &mut rgba).unwrap();
        assert_eq!(rgba, [0x80, 0x40, 0x20, 255]);
    }
}
