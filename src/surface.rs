//! CPU-backed DirectDraw 1-7 surfaces. A lock exposes stable storage until Unlock.
#![allow(non_snake_case)]

mod blit;
mod dc;
use blit::blit;
use dc::SurfaceDc;

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop, MaybeUninit};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use windows::core::{implement, Error, IUnknown, Interface, OutRef, Ref, Result, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HANDLE, RECT};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{BLACKNESS, HDC, PALETTEENTRY, PATCOPY, SRCCOPY, WHITENESS};

use crate::clipper;
use crate::draw::DisplayShared;
use crate::simd_blit::{self, KeyRange};

const fn dd_hresult(code: u32) -> HRESULT {
    HRESULT((0x8876_0000 | code) as i32)
}

const DDERR_ALREADYINITIALIZED: HRESULT = dd_hresult(5);
const DDERR_INVALIDCAPS: HRESULT = dd_hresult(100);
const DDERR_INVALIDPIXELFORMAT: HRESULT = dd_hresult(145);
const DDERR_INVALIDRECT: HRESULT = dd_hresult(150);
const DDERR_NOCOLORKEY: HRESULT = dd_hresult(215);
const DDERR_NOTFOUND: HRESULT = dd_hresult(255);
const DDERR_SURFACEALREADYATTACHED: HRESULT = dd_hresult(410);
const DDERR_SURFACEBUSY: HRESULT = dd_hresult(430);
const DDERR_SURFACENOTATTACHED: HRESULT = dd_hresult(460);
const DDERR_NOCLIPPERATTACHED: HRESULT = dd_hresult(568);
const DDERR_NOPALETTEATTACHED: HRESULT = dd_hresult(572);
const DDERR_NOTAOVERLAYSURFACE: HRESULT = dd_hresult(580);
const DDERR_NOTFLIPPABLE: HRESULT = dd_hresult(582);
const DDERR_CANTDUPLICATE: HRESULT = dd_hresult(583);
const DDERR_NOTLOCKED: HRESULT = dd_hresult(584);
const DDERR_CANTCREATEDC: HRESULT = dd_hresult(585);
const DDERR_NODC: HRESULT = dd_hresult(586);
const DDERR_DCALREADYCREATED: HRESULT = dd_hresult(620);
const DDERR_MOREDATA: HRESULT = dd_hresult(690);

const MAX_BACKBUFFERS: u32 = 8;
const TOP_GUARD_LINES: usize = 64;
const BOTTOM_GUARD_LINES: usize = 200;
const MAX_TOP_GUARD_BYTES: usize = 256 * 1024;
const MAX_BOTTOM_GUARD_BYTES: usize = 512 * 1024;

fn guard_padding(pitch: usize) -> (usize, usize) {
    let top = (pitch
        .saturating_mul(TOP_GUARD_LINES)
        .min(MAX_TOP_GUARD_BYTES)
        + 15)
        & !15;
    let bottom = pitch
        .saturating_mul(BOTTOM_GUARD_LINES)
        .min(MAX_BOTTOM_GUARD_BYTES)
        .max(
            pitch
                .saturating_mul(TOP_GUARD_LINES)
                .min(MAX_BOTTOM_GUARD_BYTES),
        );
    (top, bottom)
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    Indexed = 0,
    Rgb555 = 1,
    Rgb565 = 2,
    Xrgb = 3,
}

type Format = PixelFormat;

impl PixelFormat {
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
            || pf.dwFlags & !(DDPF_RGB | DDPF_PALETTEINDEXED8 | DDPF_ALPHAPIXELS) as u32 != 0
        {
            return Err(DDERR_INVALIDPIXELFORMAT.into());
        }
        match crate::descriptor::rgb_layout(pf) {
            (8, _) if pf.dwFlags & DDPF_PALETTEINDEXED8 as u32 != 0 => Ok(Self::Indexed),
            (16, [0xf800, 0x07e0, 0x001f]) => Ok(Self::Rgb565),
            (16, [0x7c00, 0x03e0, 0x001f]) => Ok(Self::Rgb555),
            (32, [0xff0000, 0xff00, 0xff]) => Ok(Self::Xrgb),
            _ => Err(DDERR_INVALIDPIXELFORMAT.into()),
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Self::Indexed => 1,
            Self::Rgb565 | Self::Rgb555 => 2,
            Self::Xrgb => 4,
        }
    }

    fn rgb_masks(self) -> [u32; 3] {
        match self {
            Self::Indexed => [0, 0, 0],
            Self::Rgb555 => [0x7c00, 0x03e0, 0x001f],
            Self::Rgb565 => [0xf800, 0x07e0, 0x001f],
            Self::Xrgb => [0xff0000, 0xff00, 0xff],
        }
    }

    fn description(self) -> DDPIXELFORMAT {
        let [red, green, blue] = self.rgb_masks();
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

    fn decode_rgb(self, pixel: &[u8], palette: &[PALETTEENTRY; 256]) -> [u8; 3] {
        match self {
            Self::Indexed => {
                let e = palette[pixel[0] as usize];
                [e.peRed, e.peGreen, e.peBlue]
            }
            Self::Xrgb => [pixel[2], pixel[1], pixel[0]],
            Self::Rgb555 | Self::Rgb565 => {
                let value = u16::from_le_bytes([pixel[0], pixel[1]]);
                let green_bits = if self == Self::Rgb565 { 6 } else { 5 };
                let green_max = (1 << green_bits) - 1;
                [
                    (((value >> (5 + green_bits)) & 31) * 255 / 31) as u8,
                    (((value >> 5) & green_max) * 255 / green_max) as u8,
                    ((value & 31) * 255 / 31) as u8,
                ]
            }
        }
    }

    fn encode_rgb(self, rgb: [u8; 3], out: &mut [u8]) {
        match self {
            Self::Indexed => {
                // Approximate luminance index when targeting an indexed surface from RGB.
                out[0] =
                    ((rgb[0] as u16 * 77 + rgb[1] as u16 * 150 + rgb[2] as u16 * 29) >> 8) as u8;
            }
            Self::Rgb555 => {
                let v = (((rgb[0] as u16) >> 3) << 10)
                    | (((rgb[1] as u16) >> 3) << 5)
                    | ((rgb[2] as u16) >> 3);
                out[..2].copy_from_slice(&v.to_le_bytes());
            }
            Self::Rgb565 => {
                let v = (((rgb[0] as u16) >> 3) << 11)
                    | (((rgb[1] as u16) >> 2) << 5)
                    | ((rgb[2] as u16) >> 3);
                out[..2].copy_from_slice(&v.to_le_bytes());
            }
            Self::Xrgb => {
                out[0] = rgb[2];
                out[1] = rgb[1];
                out[2] = rgb[0];
                out[3] = 0;
            }
        }
    }
}

pub fn pixel_format(bpp: u32) -> Result<DDPIXELFORMAT> {
    Ok(Format::from_depth(bpp)?.description())
}

// A DirectDraw lock and a GDI DC both grant exclusive write access.
enum Access {
    Ready,
    Locked {
        rect: RECT,
        owner_tid: ThreadId,
    },
    Gdi {
        handle: HDC,
        saved_state: i32,
        owner_tid: ThreadId,
    },
}

const CROSS_THREAD_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Thread-safe synchronization wrapper around `SurfaceData` supporting cross-thread
/// `DDLOCK_WAIT` / `DDBLT_WAIT` / `DDFLIP_WAIT` coordination without spurious `DDERR_SURFACEBUSY`.
pub struct SurfaceSlot {
    data: Mutex<SurfaceData>,
    ready_cv: Condvar,
}

// All fields inside `SurfaceData` are accessed strictly under `SurfaceSlot::data` Mutex;
// GDI memory DCs and COM interface references (atomic refcount) are exclusively synchronized.
unsafe impl Send for SurfaceSlot {}
unsafe impl Sync for SurfaceSlot {}

impl SurfaceSlot {
    fn new(data: SurfaceData) -> Self {
        Self {
            data: Mutex::new(data),
            ready_cv: Condvar::new(),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, SurfaceData> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Acquires `SurfaceData` when its `Access` is `Ready`.
    /// - Same-thread conflict (e.g. `GetDC` followed by `Lock` on the same thread) immediately
    ///   returns `DDERR_SURFACEBUSY` to match DirectDraw semantics and prevent self-deadlock.
    /// - Cross-thread conflict (e.g. a background cursor thread calling `Lock` while the main
    ///   thread briefly holds `Lock` or `GetDC`) waits on `ready_cv` when `wait` is true.
    fn lock_ready(&self, wait: bool) -> Result<MutexGuard<'_, SurfaceData>> {
        let mut guard = self.lock();
        if !guard.locked() {
            return Ok(guard);
        }
        let tid = Some(thread::current().id());
        if !wait || guard.lock_owner_tid() == tid {
            return Err(DDERR_SURFACEBUSY.into());
        }
        let deadline = Instant::now() + CROSS_THREAD_WAIT_TIMEOUT;
        while guard.locked() {
            if guard.lock_owner_tid() == tid {
                return Err(DDERR_SURFACEBUSY.into());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(DDERR_SURFACEBUSY.into());
            };
            let (g, timeout) = self
                .ready_cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
            if timeout.timed_out() && guard.locked() {
                return Err(DDERR_SURFACEBUSY.into());
            }
        }
        Ok(guard)
    }

    fn notify_ready(&self) {
        self.ready_cv.notify_all();
    }
}

/// Acquires two distinct `SurfaceSlot`s in canonical pointer-address order and verifies both
/// are `Access::Ready` without ever holding one mutex while waiting on the other's `Condvar`.
fn lock_pair_ready<'a>(
    first: &'a SurfaceSlot,
    second: &'a SurfaceSlot,
    wait: bool,
) -> Result<(MutexGuard<'a, SurfaceData>, MutexGuard<'a, SurfaceData>)> {
    let swap = std::ptr::from_ref(second) < std::ptr::from_ref(first);
    let (lo, hi) = if swap {
        (second, first)
    } else {
        (first, second)
    };
    loop {
        let g_lo = lo.lock_ready(wait)?;
        let g_hi = hi.lock();
        if !g_hi.locked() {
            return Ok(if swap { (g_hi, g_lo) } else { (g_lo, g_hi) });
        }
        let hi_owner = g_hi.lock_owner_tid();
        drop(g_hi);
        drop(g_lo);
        if !wait || hi_owner == Some(thread::current().id()) {
            return Err(DDERR_SURFACEBUSY.into());
        }
        drop(hi.lock_ready(true)?);
    }
}

/// Atomically locks a flip chain of distinct `SurfaceSlot`s in canonical pointer-address order,
/// waiting if another thread temporarily holds `Lock`/`GetDC` on any member.
fn lock_chain_ready<'a>(
    slots: &[&'a SurfaceSlot],
    wait: bool,
) -> Result<Vec<MutexGuard<'a, SurfaceData>>> {
    let mut order: Vec<usize> = (0..slots.len()).collect();
    order.sort_by_key(|&i| std::ptr::from_ref(slots[i]) as usize);
    loop {
        let mut guards: Vec<Option<MutexGuard<'a, SurfaceData>>> =
            (0..slots.len()).map(|_| None).collect();
        let mut busy_idx: Option<usize> = None;
        for &idx in &order {
            let g = slots[idx].lock();
            if g.locked() {
                let same_thread = g.lock_owner_tid() == Some(thread::current().id());
                drop(g);
                if !wait || same_thread {
                    return Err(DDERR_SURFACEBUSY.into());
                }
                busy_idx = Some(idx);
                break;
            }
            guards[idx] = Some(g);
        }
        if let Some(idx) = busy_idx {
            drop(guards);
            drop(slots[idx].lock_ready(true)?);
            continue;
        }
        return Ok(guards.into_iter().map(Option::unwrap).collect());
    }
}

enum PrivateData {
    Bytes(Vec<u8>),
    Unknown(IUnknown),
}

#[derive(Default)]
struct OverlayState {
    x: i32,
    y: i32,
    visible: bool,
}

/// The byte allocation and the layout used to address it travel together when flipping or resizing.
struct PixelStorage {
    pitch: usize,
    pixel_offset: usize,
    image_len: usize,
    pixels: Vec<u8>,
}

impl PixelStorage {
    fn new(width: u32, height: u32, format: Format, supplied_pitch: Option<i32>) -> Result<Self> {
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err(E_INVALIDARG.into());
        }
        let row_bytes = (width as usize)
            .checked_mul(format.bytes())
            .ok_or(E_OUTOFMEMORY)?;
        let pitch = match supplied_pitch {
            Some(p) if p > 0 && p as usize >= row_bytes => p as usize,
            Some(_) => return Err(E_INVALIDARG.into()),
            None => row_bytes
                .checked_next_multiple_of(4)
                .filter(|&p| p <= i32::MAX as usize)
                .ok_or(E_OUTOFMEMORY)?,
        };
        let image_len = pitch.checked_mul(height as usize).ok_or(E_OUTOFMEMORY)?;
        let (pixel_offset, bottom_guard) = guard_padding(pitch);
        let alloc_len = pixel_offset
            .checked_add(image_len)
            .and_then(|v| v.checked_add(bottom_guard))
            .ok_or(E_OUTOFMEMORY)?;
        Ok(Self {
            pitch,
            pixel_offset,
            image_len,
            pixels: zeros(alloc_len)?,
        })
    }

    /// # Safety
    /// If DDSD_LPSURFACE is set and non-null, the caller must provide row_bytes
    /// initialized bytes at each row start. Pitch padding is never accessed.
    unsafe fn import_pixels(&mut self, desc: &DDSURFACEDESC, row_bytes: usize) -> Result<()> {
        if desc.dwFlags & DDSD_LPSURFACE as u32 == 0 || desc.lpSurface.is_null() {
            return Ok(());
        }
        let src_pitch = match supplied_pitch(desc) {
            Some(p) if p > 0 && p as usize >= row_bytes => p as usize,
            Some(_) => return Err(E_INVALIDARG.into()),
            None => self.pitch,
        };
        let height = self.image_len / self.pitch;
        (height - 1)
            .checked_mul(src_pitch)
            .and_then(|n| n.checked_add(row_bytes))
            .filter(|&len| len <= isize::MAX as usize)
            .ok_or(E_INVALIDARG)?;
        let image = &mut self.pixels[self.pixel_offset..self.pixel_offset + self.image_len];
        for (y, row) in image.chunks_exact_mut(self.pitch).enumerate() {
            // SAFETY: the checked offset addresses a caller-provided row. No
            // slice spans a gap between rows or extends past the final pixel.
            let src = unsafe {
                std::slice::from_raw_parts(
                    desc.lpSurface.cast::<u8>().add(y * src_pitch),
                    row_bytes,
                )
            };
            row[..row_bytes].copy_from_slice(src);
        }
        Ok(())
    }
}

fn supplied_pitch(desc: &DDSURFACEDESC) -> Option<i32> {
    if desc.dwFlags & DDSD_PITCH as u32 != 0 {
        // DDSD_PITCH selects the signed pitch member of this DWORD union.
        Some(unsafe { desc.Anonymous1.lPitch })
    } else {
        None
    }
}

pub struct SurfaceData {
    pub width: u32,
    pub height: u32,
    access: Access,
    format: Format,
    caps: u32,
    storage: PixelStorage,
    cached_dc: Option<SurfaceDc>,
    palette: Option<IDirectDrawPalette>,
    clipper: Option<IDirectDrawClipper>,
    source_key: Option<DDCOLORKEY>,
    dest_key: Option<DDCOLORKEY>,
    src_overlay_key: Option<DDCOLORKEY>,
    dest_overlay_key: Option<DDCOLORKEY>,
    gamma: Option<DDGAMMARAMP>,
    private_data: HashMap<GUID, PrivateData>,
    overlay: OverlayState,
    uniqueness: u32,
    priority: u32,
    lod: u32,
}

impl SurfaceData {
    fn new(width: u32, height: u32, format: Format, caps: u32) -> Result<Self> {
        Self::with_pitch(width, height, format, caps, None)
    }

    fn with_pitch(
        width: u32,
        height: u32,
        format: Format,
        caps: u32,
        pitch: Option<i32>,
    ) -> Result<Self> {
        Ok(Self {
            width,
            height,
            format,
            caps,
            storage: PixelStorage::new(width, height, format, pitch)?,
            cached_dc: None,
            palette: None,
            clipper: None,
            access: Access::Ready,
            source_key: None,
            dest_key: None,
            src_overlay_key: None,
            dest_overlay_key: None,
            gamma: None,
            private_data: HashMap::new(),
            overlay: OverlayState::default(),
            uniqueness: 1,
            priority: 0,
            lod: 0,
        })
    }

    pub fn pixel_slice(&self) -> &[u8] {
        &self.storage.pixels
            [self.storage.pixel_offset..self.storage.pixel_offset + self.storage.image_len]
    }

    pub fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    pub fn pitch(&self) -> u32 {
        self.storage.pitch as u32
    }

    fn pixel_slice_mut(&mut self) -> &mut [u8] {
        let start = self.storage.pixel_offset;
        &mut self.storage.pixels[start..start + self.storage.image_len]
    }

    fn surface_ptr(&mut self) -> *mut u8 {
        self.storage
            .pixels
            .as_mut_ptr()
            .wrapping_add(self.storage.pixel_offset)
    }

    fn surface_ptr_const(&self) -> *const u8 {
        self.storage
            .pixels
            .as_ptr()
            .wrapping_add(self.storage.pixel_offset)
    }

    fn ensure_cached_dc(&mut self) -> Result<()> {
        if self.cached_dc.is_none() {
            self.cached_dc = Some(SurfaceDc::new(self)?);
        }
        Ok(())
    }

    pub fn byte_len(&self) -> usize {
        self.storage.image_len
    }

    fn has_same_layout(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.storage.pitch == other.storage.pitch
            && self.format == other.format
    }

    pub fn has_dc(&self, hdc: HDC) -> bool {
        matches!(self.access, Access::Gdi { handle, .. } if handle == hdc)
    }

    pub fn is_primary(&self) -> bool {
        self.caps & DDSCAPS_PRIMARYSURFACE as u32 != 0
    }

    fn is_overlay(&self) -> bool {
        self.caps & DDSCAPS_OVERLAY as u32 != 0
    }

    fn bump_uniqueness(&mut self) {
        self.uniqueness = self.uniqueness.wrapping_add(1);
    }

    fn full_rect(&self) -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        }
    }

    fn clamp_rect(&self, rect: Option<RECT>) -> Option<RECT> {
        let r = rect.unwrap_or_else(|| self.full_rect());
        let left = r.left.clamp(0, self.width as i32);
        let top = r.top.clamp(0, self.height as i32);
        let right = r.right.clamp(0, self.width as i32);
        let bottom = r.bottom.clamp(0, self.height as i32);
        if right <= left || bottom <= top {
            None
        } else {
            Some(RECT {
                left,
                top,
                right,
                bottom,
            })
        }
    }

    fn bounds(&self, rect: Option<RECT>) -> Result<RECT> {
        let r = rect.unwrap_or_else(|| self.full_rect());
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

    fn desc(&self, backbuffer_count: u32) -> DDSURFACEDESC {
        let mut desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_PIXELFORMAT) as u32,
            dwWidth: self.width,
            dwHeight: self.height,
            lpSurface: self.surface_ptr_const().cast_mut().cast(),
            ddsCaps: DDSCAPS { dwCaps: self.caps },
            ddpfPixelFormat: self.format.description(),
            ..Default::default()
        };
        desc.Anonymous1.lPitch = self.storage.pitch as i32;
        if backbuffer_count > 0 {
            desc.dwFlags |= DDSD_BACKBUFFERCOUNT as u32;
            desc.dwBackBufferCount = backbuffer_count;
        }
        if let Some(key) = self.source_key {
            desc.dwFlags |= DDSD_CKSRCBLT as u32;
            desc.ddckCKSrcBlt = key;
        }
        if let Some(key) = self.dest_key {
            desc.dwFlags |= DDSD_CKDESTBLT as u32;
            desc.ddckCKDestBlt = key;
        }
        if let Some(key) = self.src_overlay_key {
            desc.dwFlags |= DDSD_CKSRCOVERLAY as u32;
            desc.ddckCKSrcOverlay = key;
        }
        if let Some(key) = self.dest_overlay_key {
            desc.dwFlags |= DDSD_CKDESTOVERLAY as u32;
            desc.ddckCKDestOverlay = key;
        }
        desc
    }

    pub fn locked(&self) -> bool {
        !matches!(self.access, Access::Ready)
    }

    fn lock_owner_tid(&self) -> Option<ThreadId> {
        match self.access {
            Access::Ready => None,
            Access::Locked { owner_tid, .. } | Access::Gdi { owner_tid, .. } => Some(owner_tid),
        }
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
        if self.format == Format::Indexed {
            if let Some(palette) = &self.palette {
                return crate::palette::snapshot(palette);
            }
        }
        Ok([PALETTEENTRY::default(); 256])
    }

    /// Builds the 256-entry RGBA lookup table uploaded to the WGSL presentation shader.
    /// For `Indexed`, this contains the 256-color palette (with optional gamma pre-baked).
    /// For RGB formats, this contains the per-channel gamma ramp (or identity 0..=255).
    pub fn build_lut(&self) -> Result<[[u8; 4]; 256]> {
        let gamma = self.gamma.as_ref();
        let mut lut = [[0u8; 4]; 256];
        if self.format == Format::Indexed {
            let entries = self.palette_entries()?;
            for (slot, e) in lut.iter_mut().zip(entries.iter()) {
                let (r, g, b) = if let Some(ramp) = gamma {
                    (
                        (ramp.red[e.peRed as usize] >> 8) as u8,
                        (ramp.green[e.peGreen as usize] >> 8) as u8,
                        (ramp.blue[e.peBlue as usize] >> 8) as u8,
                    )
                } else {
                    (e.peRed, e.peGreen, e.peBlue)
                };
                *slot = [r, g, b, 255];
            }
        } else if let Some(ramp) = gamma {
            for (i, slot) in lut.iter_mut().enumerate() {
                *slot = [
                    (ramp.red[i] >> 8) as u8,
                    (ramp.green[i] >> 8) as u8,
                    (ramp.blue[i] >> 8) as u8,
                    255,
                ];
            }
        } else {
            for (i, slot) in lut.iter_mut().enumerate() {
                let v = i as u8;
                *slot = [v, v, v, 255];
            }
        }
        Ok(lut)
    }
}

#[cfg(test)]
const LUT5: [u8; 32] = {
    let mut table = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        table[i] = (i * 255 / 31) as u8;
        i += 1;
    }
    table
};

#[cfg(test)]
const LUT6: [u8; 64] = {
    let mut table = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        table[i] = (i * 255 / 63) as u8;
        i += 1;
    }
    table
};

#[cfg(test)]
pub fn frame_rgba(data: &SurfaceData, rgba: &mut Vec<u8>) -> Result<()> {
    data.ready()?;
    let width = data.width as usize;
    let size = width
        .checked_mul(data.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(E_OUTOFMEMORY)?;
    rgba.try_reserve_exact(size.saturating_sub(rgba.len()))
        .map_err(|_| E_OUTOFMEMORY)?;
    rgba.resize(size, 0);
    let gamma = data.gamma.as_ref();
    let src_rows = data.pixel_slice().chunks_exact(data.storage.pitch);
    let dst_rows = rgba.chunks_exact_mut(width * 4);

    match data.format {
        Format::Rgb565 => {
            let (mut r_lut, mut g_lut, mut b_lut) = (LUT5, LUT6, LUT5);
            if let Some(ramp) = gamma {
                for r in &mut r_lut {
                    *r = (ramp.red[*r as usize] >> 8) as u8;
                }
                for g in &mut g_lut {
                    *g = (ramp.green[*g as usize] >> 8) as u8;
                }
                for b in &mut b_lut {
                    *b = (ramp.blue[*b as usize] >> 8) as u8;
                }
            }
            for (source, dst) in src_rows.zip(dst_rows) {
                for (pixel, out) in source[..width * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(dst.as_chunks_mut::<4>().0.iter_mut())
                {
                    let v = u16::from_le_bytes(*pixel) as usize;
                    *out = [r_lut[v >> 11], g_lut[(v >> 5) & 63], b_lut[v & 31], 255];
                }
            }
        }
        Format::Rgb555 => {
            let (mut r_lut, mut g_lut, mut b_lut) = (LUT5, LUT5, LUT5);
            if let Some(ramp) = gamma {
                for r in &mut r_lut {
                    *r = (ramp.red[*r as usize] >> 8) as u8;
                }
                for g in &mut g_lut {
                    *g = (ramp.green[*g as usize] >> 8) as u8;
                }
                for b in &mut b_lut {
                    *b = (ramp.blue[*b as usize] >> 8) as u8;
                }
            }
            for (source, dst) in src_rows.zip(dst_rows) {
                for (pixel, out) in source[..width * 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(dst.as_chunks_mut::<4>().0.iter_mut())
                {
                    let v = u16::from_le_bytes(*pixel) as usize;
                    *out = [
                        r_lut[(v >> 10) & 31],
                        g_lut[(v >> 5) & 31],
                        b_lut[v & 31],
                        255,
                    ];
                }
            }
        }
        Format::Indexed => {
            let entries = data.palette_entries()?;
            let mut pal_rgba = [[0u8; 4]; 256];
            for (slot, e) in pal_rgba.iter_mut().zip(entries.iter()) {
                let (r, g, b) = if let Some(ramp) = gamma {
                    (
                        (ramp.red[e.peRed as usize] >> 8) as u8,
                        (ramp.green[e.peGreen as usize] >> 8) as u8,
                        (ramp.blue[e.peBlue as usize] >> 8) as u8,
                    )
                } else {
                    (e.peRed, e.peGreen, e.peBlue)
                };
                *slot = [r, g, b, 255];
            }
            for (source, dst) in src_rows.zip(dst_rows) {
                for (&idx, out) in source[..width]
                    .iter()
                    .zip(dst.as_chunks_mut::<4>().0.iter_mut())
                {
                    *out = pal_rgba[idx as usize];
                }
            }
        }
        Format::Xrgb => {
            if let Some(ramp) = gamma {
                for (source, dst) in src_rows.zip(dst_rows) {
                    for (pixel, out) in source[..width * 4]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
                    {
                        *out = [
                            (ramp.red[pixel[2] as usize] >> 8) as u8,
                            (ramp.green[pixel[1] as usize] >> 8) as u8,
                            (ramp.blue[pixel[0] as usize] >> 8) as u8,
                            255,
                        ];
                    }
                }
            } else {
                for (source, dst) in src_rows.zip(dst_rows) {
                    for (pixel, out) in source[..width * 4]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
                    {
                        *out = [pixel[2], pixel[1], pixel[0], 255];
                    }
                }
            }
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

#[derive(Clone, Copy, Default)]
struct BltOptions {
    src_key: Option<DDCOLORKEY>,
    dst_key: Option<DDCOLORKEY>,
    mirror_lr: bool,
    mirror_ud: bool,
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
    display: Arc<DisplayShared>,
    data: Arc<SurfaceSlot>,
    is_primary: bool,
    attached: Mutex<Vec<IDirectDrawSurface>>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.display.unregister_surface(&self.data);
    }
}

fn register_surface_object(
    display: Arc<DisplayShared>,
    data: Arc<SurfaceSlot>,
    is_primary: bool,
    attached: Vec<IDirectDrawSurface>,
) -> Result<IDirectDrawSurface> {
    let iface: IDirectDrawSurface = Surface {
        display: display.clone(),
        data: data.clone(),
        is_primary,
        attached: Mutex::new(attached),
    }
    .into();
    display.register_surface(iface.downgrade()?, Arc::downgrade(&data), is_primary);
    Ok(iface)
}

/// # Safety
/// When DDSD_LPSURFACE supplies an image, its pointer must be readable for
/// every row described by the source pitch, pixel format, and dimensions.
pub unsafe fn create_surface(
    display: Arc<DisplayShared>,
    desc: &DDSURFACEDESC,
) -> Result<IDirectDrawSurface> {
    if desc.dwSize as usize != size_of::<DDSURFACEDESC>() {
        return Err(E_INVALIDARG.into());
    }
    let caps = desc.ddsCaps.dwCaps;
    let supported_caps = DDSCAPS_PRIMARYSURFACE
        | DDSCAPS_OFFSCREENPLAIN
        | DDSCAPS_SYSTEMMEMORY
        | DDSCAPS_VIDEOMEMORY
        | DDSCAPS_LOCALVIDMEM
        | DDSCAPS_NONLOCALVIDMEM
        | DDSCAPS_FLIP
        | DDSCAPS_COMPLEX
        | DDSCAPS_VISIBLE
        | DDSCAPS_FRONTBUFFER
        | DDSCAPS_BACKBUFFER
        | DDSCAPS_3DDEVICE
        | DDSCAPS_TEXTURE
        | DDSCAPS_ZBUFFER
        | DDSCAPS_OWNDC
        | DDSCAPS_WRITEONLY
        | DDSCAPS_ALLOCONLOAD
        | DDSCAPS_STANDARDVGAMODE
        | DDSCAPS_MODEX
        | DDSCAPS_HWCODEC
        | DDSCAPS_OVERLAY
        | DDSCAPS_MIPMAP
        | DDSCAPS_ALPHA
        | DDSCAPS_PALETTE;
    let supported_flags = DDSD_CAPS
        | DDSD_WIDTH
        | DDSD_HEIGHT
        | DDSD_PITCH
        | DDSD_PIXELFORMAT
        | DDSD_BACKBUFFERCOUNT
        | DDSD_CKSRCBLT
        | DDSD_CKDESTBLT
        | DDSD_CKSRCOVERLAY
        | DDSD_CKDESTOVERLAY
        | DDSD_LPSURFACE
        | DDSD_ALPHABITDEPTH
        | DDSD_ZBUFFERBITDEPTH
        | DDSD_MIPMAPCOUNT
        | DDSD_REFRESHRATE
        | DDSD_TEXTURESTAGE
        | DDSD_LINEARSIZE;
    if caps & !(supported_caps as u32) != 0 || desc.dwFlags & !(supported_flags as u32) != 0 {
        return unsupported();
    }
    let primary = caps & DDSCAPS_PRIMARYSURFACE as u32 != 0;
    let d = display.lock();
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
    if count > MAX_BACKBUFFERS {
        return unsupported();
    }
    if (count > 0) != (caps & DDSCAPS_FLIP as u32 != 0) {
        return Err(DDERR_INVALIDCAPS.into());
    }
    let mem_caps = if caps & DDSCAPS_SYSTEMMEMORY as u32 != 0 {
        caps
    } else if primary {
        caps | DDSCAPS_SYSTEMMEMORY as u32
    } else {
        caps | DDSCAPS_VIDEOMEMORY as u32
    };
    let mut data = SurfaceData::new(width, height, format, mem_caps)?;
    if primary {
        data.caps |= DDSCAPS_VISIBLE as u32;
    }
    if desc.dwFlags & DDSD_CKSRCBLT as u32 != 0 {
        data.source_key = Some(desc.ddckCKSrcBlt);
    }
    if desc.dwFlags & DDSD_CKDESTBLT as u32 != 0 {
        data.dest_key = Some(desc.ddckCKDestBlt);
    }
    if desc.dwFlags & DDSD_CKSRCOVERLAY as u32 != 0 {
        data.src_overlay_key = Some(desc.ddckCKSrcOverlay);
    }
    if desc.dwFlags & DDSD_CKDESTOVERLAY as u32 != 0 {
        data.dest_overlay_key = Some(desc.ddckCKDestOverlay);
    }
    unsafe {
        data.storage
            .import_pixels(desc, width as usize * format.bytes())?
    };

    let mut attached = Vec::new();
    if count > 0 {
        data.caps |= DDSCAPS_FRONTBUFFER as u32;
        let back_mem = if caps & DDSCAPS_VIDEOMEMORY as u32 != 0 {
            (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP | DDSCAPS_VIDEOMEMORY) as u32
        } else {
            (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP | DDSCAPS_SYSTEMMEMORY) as u32
        };
        // Build backbuffer chain from tail to head so back[0] -> back[1] -> ... -> back[count-1].
        let mut next: Option<IDirectDrawSurface> = None;
        for _ in 0..count {
            let back_data = Arc::new(SurfaceSlot::new(SurfaceData::new(
                width, height, format, back_mem,
            )?));
            let chain = next.into_iter().collect();
            next = Some(register_surface_object(
                display.clone(),
                back_data,
                false,
                chain,
            )?);
        }
        if let Some(first_back) = next {
            attached.push(first_back);
        }
    }

    let data = Arc::new(SurfaceSlot::new(data));
    let surface = register_surface_object(display, data, primary, attached)?;
    if primary {
        crate::window::set_mode(width, height, (format.bytes() * 8) as u32, true);
    }
    Ok(surface)
}

pub fn duplicate_surface(
    display: Arc<DisplayShared>,
    source: &IDirectDrawSurface,
) -> Result<IDirectDrawSurface> {
    let src_obj = source.cast_object_ref::<Surface>()?;
    let has_attached = !src_obj.attached_lock().is_empty();
    let src = src_obj.data.lock_ready(true)?;
    if src.caps & (DDSCAPS_PRIMARYSURFACE | DDSCAPS_FLIP | DDSCAPS_COMPLEX | DDSCAPS_ZBUFFER) as u32
        != 0
        || has_attached
    {
        return Err(DDERR_CANTDUPLICATE.into());
    }
    let mut copy = SurfaceData::with_pitch(
        src.width,
        src.height,
        src.format,
        src.caps,
        Some(src.storage.pitch as i32),
    )?;
    copy.pixel_slice_mut().copy_from_slice(src.pixel_slice());
    copy.palette = src.palette.clone();
    copy.clipper = src.clipper.clone();
    copy.source_key = src.source_key;
    copy.dest_key = src.dest_key;
    copy.src_overlay_key = src.src_overlay_key;
    copy.dest_overlay_key = src.dest_overlay_key;
    copy.gamma = src.gamma;
    drop(src);
    register_surface_object(display, Arc::new(SurfaceSlot::new(copy)), false, Vec::new())
}

pub fn surface_desc(surface: &IDirectDrawSurface) -> Result<DDSURFACEDESC> {
    let obj = surface.cast_object_ref::<Surface>()?;
    let count = obj.backbuffer_chain_len();
    Ok(obj.read()?.desc(count))
}

pub fn matches_desc(surface: &IDirectDrawSurface, filter: &DDSURFACEDESC) -> Result<bool> {
    let desc = surface_desc(surface)?;
    if filter.dwFlags & DDSD_CAPS as u32 != 0
        && desc.ddsCaps.dwCaps & filter.ddsCaps.dwCaps != filter.ddsCaps.dwCaps
    {
        return Ok(false);
    }
    if filter.dwFlags & DDSD_WIDTH as u32 != 0 && desc.dwWidth != filter.dwWidth {
        return Ok(false);
    }
    if filter.dwFlags & DDSD_HEIGHT as u32 != 0 && desc.dwHeight != filter.dwHeight {
        return Ok(false);
    }
    if filter.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
        let a = &filter.ddpfPixelFormat;
        let b = &desc.ddpfPixelFormat;
        if a.dwFlags != b.dwFlags
            || crate::descriptor::rgb_layout(a) != crate::descriptor::rgb_layout(b)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Surface {
    fn read(&self) -> Result<MutexGuard<'_, SurfaceData>> {
        Ok(self.data.lock())
    }

    fn write(&self) -> Result<MutexGuard<'_, SurfaceData>> {
        Ok(self.data.lock())
    }

    fn attached_lock(&self) -> MutexGuard<'_, Vec<IDirectDrawSurface>> {
        self.attached.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn refresh(&self) -> Result<()> {
        if self.is_primary {
            self.display.request_refresh()?;
        }
        Ok(())
    }

    fn present_now(&self) -> Result<()> {
        if self.is_primary {
            self.display.request_present_now()?;
        }
        Ok(())
    }

    /// Walks the attached flip chain (excluding self) without cycling.
    fn flip_chain(&self) -> Result<Vec<IDirectDrawSurface>> {
        let mut chain = Vec::new();
        let mut current = self
            .attached_lock()
            .iter()
            .find(|s| {
                s.cast_object_ref::<Surface>().is_ok_and(|o| {
                    o.data.lock().caps & (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) as u32 != 0
                })
            })
            .cloned();
        while let Some(surf) = current {
            let obj = surf.cast_object_ref::<Surface>()?;
            if Arc::ptr_eq(&obj.data, &self.data)
                || chain.iter().any(|existing: &IDirectDrawSurface| {
                    existing
                        .cast_object_ref::<Surface>()
                        .is_ok_and(|e| Arc::ptr_eq(&e.data, &obj.data))
                })
            {
                break;
            }
            let next = obj
                .attached_lock()
                .iter()
                .find(|s| {
                    s.cast_object_ref::<Surface>().is_ok_and(|o| {
                        o.data.lock().caps & (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) as u32 != 0
                    })
                })
                .cloned();
            chain.push(surf);
            current = next;
        }
        Ok(chain)
    }

    fn backbuffer_chain_len(&self) -> u32 {
        self.flip_chain().map(|c| c.len() as u32).unwrap_or(0)
    }

    fn add_attached_surface_impl<S: Interface>(&self, target: Ref<S>) -> Result<()> {
        let target_ref = target.ok()?;
        let target_obj = target_ref.cast_object_ref::<Surface>()?;
        if Arc::ptr_eq(&target_obj.data, &self.data) {
            return Err(E_INVALIDARG.into());
        }
        let target_v1 = target_ref.cast::<IDirectDrawSurface>()?;
        let mut attached = self.attached_lock();
        if attached.iter().any(|s| {
            s.cast_object_ref::<Surface>()
                .is_ok_and(|o| Arc::ptr_eq(&o.data, &target_obj.data))
        }) {
            return Err(DDERR_SURFACEALREADYATTACHED.into());
        }
        let is_front = self.data.lock().caps & DDSCAPS_FRONTBUFFER as u32 != 0;
        if is_front {
            let mut target_data = target_obj.data.lock();
            target_data.caps |= (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) as u32;
        }
        attached.push(target_v1);
        Ok(())
    }

    fn delete_attached_surface_impl<S: Interface>(&self, flags: u32, target: Ref<S>) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut attached = self.attached_lock();
        match target.as_ref() {
            None => {
                attached.clear();
                Ok(())
            }
            Some(target_ref) => {
                let target_obj = target_ref.cast_object_ref::<Surface>()?;
                let before = attached.len();
                attached.retain(|s| {
                    s.cast_object_ref::<Surface>()
                        .is_ok_and(|o| !Arc::ptr_eq(&o.data, &target_obj.data))
                });
                if attached.len() == before {
                    Err(DDERR_SURFACENOTATTACHED.into())
                } else {
                    Ok(())
                }
            }
        }
    }

    fn copy(
        &self,
        source: &Surface,
        src_rect: Option<RECT>,
        dest_rect: RECT,
        opts: BltOptions,
        wait: bool,
    ) -> Result<()> {
        // Snapshot overlapping self-blits before mutably writing the destination.
        if Arc::ptr_eq(&source.data, &self.data) {
            let mut dst = self.data.lock_ready(wait)?;
            dst.bounds(Some(dest_rect))?;
            let Some(src_rect) = dst.clamp_rect(src_rect) else {
                return Ok(());
            };
            let src_format = dst.format;
            let src_palette = if src_format == Format::Indexed {
                Some(dst.palette_entries()?)
            } else {
                None
            };
            let row_bytes = (src_rect.right - src_rect.left) as usize * src_format.bytes();
            let mut snapshot = zeros(row_bytes * (src_rect.bottom - src_rect.top) as usize)?;
            let src_pixels = dst.pixel_slice();
            for (y, row) in snapshot.chunks_exact_mut(row_bytes).enumerate() {
                let start = (src_rect.top as usize + y) * dst.storage.pitch
                    + src_rect.left as usize * src_format.bytes();
                row.copy_from_slice(&src_pixels[start..start + row_bytes]);
            }
            let local = RECT {
                left: 0,
                top: 0,
                right: src_rect.right - src_rect.left,
                bottom: src_rect.bottom - src_rect.top,
            };
            blit(
                &mut dst,
                dest_rect,
                &snapshot,
                row_bytes,
                src_format,
                src_palette.as_ref(),
                local,
                opts,
            );
            dst.bump_uniqueness();
        } else {
            let (src, mut dst) = lock_pair_ready(&source.data, &self.data, wait)?;
            dst.bounds(Some(dest_rect))?;
            let Some(src_rect) = src.clamp_rect(src_rect) else {
                return Ok(());
            };
            let src_format = src.format;
            let src_palette = if src_format == Format::Indexed {
                Some(src.palette_entries()?)
            } else {
                None
            };
            blit(
                &mut dst,
                dest_rect,
                src.pixel_slice(),
                src.storage.pitch,
                src_format,
                src_palette.as_ref(),
                src_rect,
                opts,
            );
            dst.bump_uniqueness();
        }
        Ok(())
    }

    fn blt_colorfill_rect(
        &self,
        rect: RECT,
        color: [u8; 4],
        dst_key: Option<DDCOLORKEY>,
        wait: bool,
    ) -> Result<()> {
        let mut dst = self.data.lock_ready(wait)?;
        dst.bounds(Some(rect))?;
        let bytes = dst.format.bytes();
        let pitch = dst.storage.pitch;
        let width = (rect.right - rect.left) as usize;
        let height = (rect.bottom - rect.top) as usize;
        let dk = dst_key.map(|k| KeyRange {
            lo: k.dwColorSpaceLowValue,
            hi: k.dwColorSpaceHighValue,
        });
        let pixels = dst.pixel_slice_mut();
        simd_blit::colorfill_rect(
            pixels,
            pitch,
            rect.left as usize,
            rect.top as usize,
            width,
            height,
            bytes,
            color,
            dk,
        );
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
        let supported = (DDBLT_WAIT
            | DDBLT_DONOTWAIT
            | DDBLT_ASYNC
            | DDBLT_COLORFILL
            | DDBLT_DEPTHFILL
            | DDBLT_KEYSRC
            | DDBLT_KEYSRCOVERRIDE
            | DDBLT_KEYDEST
            | DDBLT_KEYDESTOVERRIDE
            | DDBLT_DDFX
            | DDBLT_ROP) as u32;
        if flags & !supported != 0 {
            return unsupported();
        }
        let (dest, fx_ref) = unsafe { (dest.as_ref().copied(), fx.as_ref()) };
        if let Some(fx_val) = fx_ref {
            let needs_fx = flags
                & ((DDBLT_COLORFILL
                    | DDBLT_DEPTHFILL
                    | DDBLT_KEYSRCOVERRIDE
                    | DDBLT_KEYDESTOVERRIDE
                    | DDBLT_DDFX
                    | DDBLT_ROP) as u32)
                != 0;
            if needs_fx && fx_val.dwSize as usize != size_of::<DDBLTFX>() {
                return Err(E_INVALIDARG.into());
            }
        }
        let Some(rect) = self.read()?.clamp_rect(dest) else {
            return Ok(());
        };
        let clip_rects = {
            let clipper_opt = self.read()?.clipper.clone();
            match clipper_opt {
                Some(ref c) => clipper::clip_rects(c, rect)?,
                None => None,
            }
        };

        let dst_key = if flags & DDBLT_KEYDESTOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.ddckDestColorkey)
        } else if flags & DDBLT_KEYDEST as u32 != 0 {
            Some(self.read()?.dest_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };

        // Handle ROP fills (BLACKNESS / WHITENESS / PATCOPY).
        let mut fill_color: Option<[u8; 4]> = None;
        if flags & (DDBLT_COLORFILL | DDBLT_DEPTHFILL) as u32 != 0 {
            if !source.is_null() || flags & (DDBLT_KEYSRC | DDBLT_KEYSRCOVERRIDE) as u32 != 0 {
                return Err(E_INVALIDARG.into());
            }
            let fx_val = fx_ref.ok_or(E_INVALIDARG)?;
            let raw = if flags & DDBLT_COLORFILL as u32 != 0 {
                unsafe { fx_val.Anonymous5.dwFillColor }
            } else {
                unsafe { fx_val.Anonymous5.dwFillDepth }
            };
            fill_color = Some(raw.to_le_bytes());
        } else if flags & DDBLT_ROP as u32 != 0 {
            let fx_val = fx_ref.ok_or(E_INVALIDARG)?;
            let rop = fx_val.dwROP;
            if rop == BLACKNESS.0 {
                fill_color = Some([0, 0, 0, 0]);
            } else if rop == WHITENESS.0 {
                fill_color = Some([0xff, 0xff, 0xff, 0xff]);
            } else if rop == PATCOPY.0 {
                fill_color = Some(unsafe { fx_val.Anonymous5.dwFillColor }.to_le_bytes());
            } else if rop != SRCCOPY.0 {
                return unsupported();
            }
        }

        let wait = flags & DDBLT_DONOTWAIT as u32 == 0;
        if let Some(color) = fill_color {
            match clip_rects {
                Some(rects) => {
                    for sub in rects {
                        self.blt_colorfill_rect(sub, color, dst_key, wait)?;
                    }
                }
                None => self.blt_colorfill_rect(rect, color, dst_key, wait)?,
            }
            return self.refresh();
        }

        let source_obj = source.ok()?.cast_object_ref::<Surface>()?;
        let src = unsafe { src.as_ref() }.copied();
        let src_key = if flags & DDBLT_KEYSRCOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.ddckSrcColorkey)
        } else if flags & DDBLT_KEYSRC as u32 != 0 {
            Some(source_obj.read()?.source_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };

        let (mirror_lr, mirror_ud) = if flags & DDBLT_DDFX as u32 != 0 {
            let ddfx = fx_ref.ok_or(E_INVALIDARG)?.dwDDFX;
            let allowed_ddfx = (DDBLTFX_MIRRORLEFTRIGHT
                | DDBLTFX_MIRRORUPDOWN
                | DDBLTFX_NOTEARING
                | DDBLTFX_ARITHSTRETCHY) as u32;
            if ddfx & !allowed_ddfx != 0 {
                return unsupported();
            }
            (
                ddfx & DDBLTFX_MIRRORLEFTRIGHT as u32 != 0,
                ddfx & DDBLTFX_MIRRORUPDOWN as u32 != 0,
            )
        } else {
            (false, false)
        };

        let opts = BltOptions {
            src_key,
            dst_key,
            mirror_lr,
            mirror_ud,
        };

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
                        let s_top = full_src.top + ((sub.top - rect.top) as i64 * sh / dh) as i32;
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
                        self.copy(source_obj, Some(sub_src), sub, opts, wait)?;
                    }
                }
            }
            None => self.copy(source_obj, src, rect, opts, wait)?,
        }
        self.refresh()
    }

    fn blt_fast_impl<S: Interface>(
        &self,
        x: u32,
        y: u32,
        source: Ref<S>,
        src_rect: *mut RECT,
        flags: u32,
    ) -> Result<()> {
        if flags
            & !(DDBLTFAST_WAIT
                | DDBLTFAST_DONOTWAIT
                | DDBLTFAST_SRCCOLORKEY
                | DDBLTFAST_DESTCOLORKEY)
            != 0
        {
            return unsupported();
        }
        let source_obj = source.ok()?.cast_object_ref::<Surface>()?;
        let s = source_obj
            .read()?
            .bounds(unsafe { src_rect.as_ref() }.copied())?;
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
        self.read()?.bounds(Some(rect))?;
        let src_key = if flags & DDBLTFAST_SRCCOLORKEY != 0 {
            Some(source_obj.read()?.source_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        let dst_key = if flags & DDBLTFAST_DESTCOLORKEY != 0 {
            Some(self.read()?.dest_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        let wait = flags & DDBLTFAST_DONOTWAIT == 0;
        self.copy(
            source_obj,
            Some(s),
            rect,
            BltOptions {
                src_key,
                dst_key,
                mirror_lr: false,
                mirror_ud: false,
            },
            wait,
        )?;
        self.refresh()
    }

    fn blt_batch_impl(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        if count == 0 {
            return Ok(());
        }
        if batch.is_null() {
            return Err(E_INVALIDARG.into());
        }
        for i in 0..count as usize {
            let entry = unsafe { &*batch.add(i) };
            let src_ref: Ref<IDirectDrawSurface> = (&*entry.lpDDSSrc).into();
            self.blt_impl(
                entry.lprDest,
                src_ref,
                entry.lprSrc,
                entry.dwFlags,
                entry.lpDDBltFx,
            )?;
        }
        Ok(())
    }

    fn flip_impl<S: Interface>(&self, target: Ref<S>, flags: u32) -> Result<()> {
        if flags & !((DDFLIP_WAIT | DDFLIP_DONOTWAIT | DDFLIP_NOVSYNC) as u32) != 0 {
            return unsupported();
        }
        let wait = flags & DDFLIP_DONOTWAIT as u32 == 0;
        let chain = self.flip_chain()?;
        if chain.is_empty() {
            return Err(DDERR_NOTFLIPPABLE.into());
        }
        if let Some(t) = target.as_ref() {
            let t_obj = t
                .cast_object_ref::<Surface>()
                .map_err(|_| DDERR_SURFACENOTATTACHED)?;
            if !chain.iter().any(|s| {
                s.cast_object_ref::<Surface>()
                    .is_ok_and(|o| Arc::ptr_eq(&o.data, &t_obj.data))
            }) {
                return Err(DDERR_SURFACENOTATTACHED.into());
            }
            {
                let (mut front, mut back_data) = lock_pair_ready(&self.data, &t_obj.data, wait)?;
                if !front.has_same_layout(&back_data) {
                    return Err(DDERR_NOTFLIPPABLE.into());
                }
                std::mem::swap(&mut front.storage, &mut back_data.storage);
                std::mem::swap(&mut front.cached_dc, &mut back_data.cached_dc);
                front.bump_uniqueness();
                back_data.bump_uniqueness();
            }
            return self.present_now();
        }

        // Lock the entire flip chain atomically in canonical pointer order before rotating buffers.
        let mut slot_refs: Vec<&SurfaceSlot> = Vec::with_capacity(chain.len() + 1);
        slot_refs.push(&self.data);
        for surf in &chain {
            slot_refs.push(&surf.cast_object_ref::<Surface>()?.data);
        }
        {
            let mut guards = lock_chain_ready(&slot_refs, wait)?;
            if guards[1..]
                .iter()
                .any(|back| !guards[0].has_same_layout(back))
            {
                return Err(DDERR_NOTFLIPPABLE.into());
            }
            let (front, backs) = guards.split_first_mut().expect("flip chain has a front");
            for back in backs.iter_mut().rev() {
                std::mem::swap(&mut front.storage, &mut back.storage);
                std::mem::swap(&mut front.cached_dc, &mut back.cached_dc);
                back.bump_uniqueness();
            }
            front.bump_uniqueness();
        }
        self.present_now()
    }

    fn get_attached_surface_impl<S>(&self, caps_val: u32, out: OutRef<S>) -> Result<()>
    where
        S: Interface + windows_core::Type<S, Default = Option<S>>,
    {
        let attachment = (|| -> Result<S> {
            if caps_val == 0 {
                return Err(DDERR_NOTFOUND.into());
            }
            let attached = self.attached_lock();
            for s in attached.iter() {
                let actual = s.cast_object_ref::<Surface>()?.read()?.caps;
                if actual & caps_val == caps_val {
                    return s.cast::<S>();
                }
            }
            Err(DDERR_NOTFOUND.into())
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
                | DDLOCK_EVENT
                | DDLOCK_READONLY
                | DDLOCK_WRITEONLY
                | DDLOCK_NOSYSLOCK
                | DDLOCK_DISCARDCONTENTS
                | DDLOCK_NOOVERWRITE) as u32)
            != 0
        {
            return unsupported();
        }
        let wait = flags & DDLOCK_DONOTWAIT as u32 == 0;
        let count = self.backbuffer_chain_len();
        let mut data = self.data.lock_ready(wait)?;
        let r = data.bounds(unsafe { rect.as_ref() }.copied())?;
        let mut desc = data.desc(count);
        let offset = r.top as usize * data.storage.pitch + r.left as usize * data.format.bytes();
        desc.lpSurface = data.surface_ptr().wrapping_add(offset).cast();
        desc.dwFlags |= DDSD_LPSURFACE as u32;
        data.access = Access::Locked {
            rect: r,
            owner_tid: thread::current().id(),
        };
        Ok(desc)
    }

    fn unlock_ptr_impl(&self, address: *mut c_void) -> Result<()> {
        let mut data = self.write()?;
        let Access::Locked { .. } = data.access else {
            return Err(DDERR_NOTLOCKED.into());
        };
        if !address.is_null() {
            let base = data.surface_ptr() as usize;
            let end = base.saturating_add(data.storage.image_len);
            let ptr = address as usize;
            if ptr < base || ptr >= end {
                return Err(E_INVALIDARG.into());
            }
        }
        data.access = Access::Ready;
        data.bump_uniqueness();
        drop(data);
        self.data.notify_ready();
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
        self.data.notify_ready();
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
        // The caller supplies an output slot which may be uninitialized.
        let out = unsafe { out.cast::<MaybeUninit<*mut c_void>>().as_mut() }.ok_or(E_POINTER)?;
        out.write(std::ptr::null_mut());
        let parent = self.display.clone_owner_draw().ok_or(E_INVALIDARG)?;
        out.write(parent.into_raw());
        Ok(())
    }

    /// # Safety
    /// A supplied DDSD_LPSURFACE image must contain each requested row of pixels.
    unsafe fn set_surface_desc_impl(&self, desc: &DDSURFACEDESC, flags: u32) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut data = self.data.lock_ready(true)?;
        let format = if desc.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
            Format::from_dd(&desc.ddpfPixelFormat)?
        } else {
            data.format
        };
        let width = if desc.dwFlags & DDSD_WIDTH as u32 != 0 {
            desc.dwWidth
        } else {
            data.width
        };
        let height = if desc.dwFlags & DDSD_HEIGHT as u32 != 0 {
            desc.dwHeight
        } else {
            data.height
        };
        let mut storage = PixelStorage::new(width, height, format, supplied_pitch(desc))?;
        // The input can point to the old surface. Keep it alive until all rows have been copied.
        unsafe { storage.import_pixels(desc, width as usize * format.bytes())? };
        data.width = width;
        data.height = height;
        data.format = format;
        data.storage = storage;
        data.cached_dc = None;
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
        unsafe { out.cast::<MaybeUninit<u32>>().as_mut() }
            .ok_or(E_INVALIDARG)?
            .write(self.read()?.uniqueness);
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
        unsafe { out.cast::<MaybeUninit<u32>>().as_mut() }
            .ok_or(E_INVALIDARG)?
            .write(self.read()?.priority);
        Ok(())
    }

    fn set_lod_impl(&self, lod: u32) -> Result<()> {
        self.write()?.lod = lod;
        Ok(())
    }

    fn get_lod_impl(&self, out: *mut u32) -> Result<()> {
        unsafe { out.cast::<MaybeUninit<u32>>().as_mut() }
            .ok_or(E_INVALIDARG)?
            .write(self.read()?.lod);
        Ok(())
    }

    fn ensure_overlay(&self) -> Result<()> {
        if !self.read()?.is_overlay() {
            return Err(DDERR_NOTAOVERLAYSURFACE.into());
        }
        Ok(())
    }

    fn get_overlay_position_impl(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        self.ensure_overlay()?;
        if x.is_null() || y.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let data = self.read()?;
        // Output slots may be uninitialized or coincide.
        unsafe {
            x.write(data.overlay.x);
            y.write(data.overlay.y);
        }
        Ok(())
    }

    fn set_overlay_position_impl(&self, x: i32, y: i32) -> Result<()> {
        self.ensure_overlay()?;
        let mut data = self.write()?;
        data.overlay.x = x;
        data.overlay.y = y;
        Ok(())
    }

    fn update_overlay_impl<S: Interface>(
        &self,
        src_rect: *mut RECT,
        dest: Ref<S>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.ensure_overlay()?;
        let dest_obj = dest.ok()?.cast_object_ref::<Surface>()?;
        let (dest_rect, fx_ref) = unsafe { (dest_rect.as_ref().copied(), fx.as_ref()) };
        let d_rect = dest_obj.read()?.bounds(dest_rect)?;
        let src_key = if flags & DDOVER_KEYSRCOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.dckSrcColorkey)
        } else if flags & DDOVER_KEYSRC as u32 != 0 {
            Some(self.read()?.src_overlay_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        let dst_key = if flags & DDOVER_KEYDESTOVERRIDE as u32 != 0 {
            Some(fx_ref.ok_or(E_INVALIDARG)?.dckDestColorkey)
        } else if flags & DDOVER_KEYDEST as u32 != 0 {
            Some(dest_obj.read()?.dest_overlay_key.ok_or(DDERR_NOCOLORKEY)?)
        } else {
            None
        };
        {
            let mut data = self.write()?;
            data.overlay.x = d_rect.left;
            data.overlay.y = d_rect.top;
            if flags & DDOVER_HIDE as u32 != 0 {
                data.overlay.visible = false;
            } else if flags & DDOVER_SHOW as u32 != 0 {
                data.overlay.visible = true;
            }
        }
        if flags & DDOVER_HIDE as u32 == 0 && self.read()?.overlay.visible {
            dest_obj.copy(
                self,
                unsafe { src_rect.as_ref() }.copied(),
                d_rect,
                BltOptions {
                    src_key,
                    dst_key,
                    mirror_lr: false,
                    mirror_ud: false,
                },
                true,
            )?;
            dest_obj.refresh()?;
        }
        Ok(())
    }
}

impl IDirectDrawGammaControl_Impl for Surface_Impl {
    fn GetGammaRamp(&self, flags: u32, ramp: *mut DDGAMMARAMP) -> Result<()> {
        if flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let out =
            unsafe { ramp.cast::<MaybeUninit<DDGAMMARAMP>>().as_mut() }.ok_or(E_INVALIDARG)?;
        out.write(self.read()?.gamma.unwrap_or_else(identity_gamma_ramp));
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
        let snapshot: Vec<IDirectDrawSurface> = self.attached_lock().clone();
        for s in snapshot {
            let mut desc = surface_desc(&s)?;
            let owned = ManuallyDrop::new(s);
            if unsafe { callback((&*owned).into(), &mut desc, context) }.0
                == DDENUMRET_CANCEL as i32
            {
                break;
            }
        }
        Ok(())
    }

    fn Flip(&self, target: Ref<IDirectDrawSurface>, flags: u32) -> Result<()> {
        self.flip_impl(target, flags)
    }

    fn GetCaps(&self, caps: *mut DDSCAPS) -> Result<()> {
        let out = unsafe { caps.cast::<MaybeUninit<DDSCAPS>>().as_mut() }.ok_or(E_INVALIDARG)?;
        out.write(DDSCAPS {
            dwCaps: self.read()?.caps,
        });
        Ok(())
    }

    fn SetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        let which = flags & !(DDCKEY_COLORSPACE as u32);
        let mut key = unsafe { key.as_ref() }.copied();
        if flags & DDCKEY_COLORSPACE as u32 == 0 {
            if let Some(key) = &mut key {
                key.dwColorSpaceHighValue = key.dwColorSpaceLowValue;
            }
        }
        let mut data = self.write()?;
        match which as i32 {
            DDCKEY_SRCBLT => data.source_key = key,
            DDCKEY_DESTBLT => data.dest_key = key,
            DDCKEY_SRCOVERLAY => data.src_overlay_key = key,
            DDCKEY_DESTOVERLAY => data.dest_overlay_key = key,
            _ => return unsupported(),
        }
        Ok(())
    }

    fn GetColorKey(&self, flags: u32, key: *mut DDCOLORKEY) -> Result<()> {
        let out = unsafe { key.cast::<MaybeUninit<DDCOLORKEY>>().as_mut() }.ok_or(E_INVALIDARG)?;
        let data = self.read()?;
        let stored = match flags as i32 {
            DDCKEY_SRCBLT => data.source_key,
            DDCKEY_DESTBLT => data.dest_key,
            DDCKEY_SRCOVERLAY => data.src_overlay_key,
            DDCKEY_DESTOVERLAY => data.dest_overlay_key,
            _ => return unsupported(),
        };
        out.write(stored.ok_or(DDERR_NOCOLORKEY)?);
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
        unsafe { crate::descriptor::output(out)? }.write(self.read()?.format.description());
        Ok(())
    }

    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC) -> Result<()> {
        let out = unsafe { crate::descriptor::output(out)? };
        let count = self.backbuffer_chain_len();
        out.write(self.read()?.desc(count));
        Ok(())
    }

    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC, flags: u32, _: HANDLE) -> Result<()> {
        unsafe { crate::descriptor::output(out)? }.write(self.lock_desc(rect, flags)?);
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
        let chain = self.flip_chain()?;
        if chain.is_empty() {
            return Err(DDERR_NOTFLIPPABLE.into());
        }
        match flags as i32 {
            DDGFS_ISFLIPDONE => Ok(()),
            DDGFS_CANFLIP => {
                self.read()?.ready()?;
                for s in &chain {
                    s.cast_object_ref::<Surface>()?.read()?.ready()?;
                }
                Ok(())
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
    fn AddAttachedSurface(&self, surface: Ref<IDirectDrawSurface>) -> Result<()> {
        self.add_attached_surface_impl(surface)
    }
    fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<IDirectDrawSurface>) -> Result<()> {
        self.delete_attached_surface_impl(flags, surface)
    }
    fn AddOverlayDirtyRect(&self, rect: *mut RECT) -> Result<()> {
        self.ensure_overlay()?;
        self.read()?.bounds(unsafe { rect.as_ref() }.copied())?;
        Ok(())
    }
    fn BltBatch(&self, batch: *mut DDBLTBATCH, count: u32, flags: u32) -> Result<()> {
        self.blt_batch_impl(batch, count, flags)
    }
    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        _: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let _ = callback.ok_or(E_INVALIDARG)?;
        if flags != DDENUMOVERLAYZ_FRONTTOBACK as u32 && flags != DDENUMOVERLAYZ_BACKTOFRONT as u32
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }
    fn GetDC(&self, out: *mut HDC) -> Result<()> {
        let out = unsafe { out.cast::<MaybeUninit<HDC>>().as_mut() }.ok_or(E_INVALIDARG)?;
        out.write(HDC::default());
        let tid = thread::current().id();
        {
            let data = self.data.lock();
            if matches!(data.access, Access::Gdi { owner_tid, .. } if owner_tid == tid) {
                return Err(DDERR_DCALREADYCREATED.into());
            }
        }
        let mut data = self.data.lock_ready(true)?;
        let entries = if data.format == Format::Indexed {
            Some(data.palette_entries()?)
        } else {
            None
        };
        let own_dc = data.caps & DDSCAPS_OWNDC as u32 != 0;
        let uniqueness = data.uniqueness;
        let len = data.storage.image_len;
        let pitch = data.storage.pitch;
        data.ensure_cached_dc()?;
        let src_start = data.storage.pixel_offset;
        let SurfaceData {
            ref storage,
            ref mut cached_dc,
            ..
        } = *data;
        let dc = cached_dc.as_mut().expect("cached_dc initialized");
        let (handle, saved_state) = dc.begin_drawing(
            &storage.pixels[src_start..src_start + len],
            pitch,
            uniqueness,
            entries.as_ref(),
            own_dc,
        );
        out.write(handle);
        data.access = Access::Gdi {
            handle,
            saved_state,
            owner_tid: tid,
        };
        Ok(())
    }

    fn ReleaseDC(&self, handle: HDC) -> Result<()> {
        let mut data = self.write()?;
        let Access::Gdi {
            handle: dc_handle,
            saved_state,
            ..
        } = data.access
        else {
            return Err(DDERR_NODC.into());
        };
        if handle != dc_handle {
            return Err(E_INVALIDARG.into());
        }
        let len = data.storage.image_len;
        let pitch = data.storage.pitch;
        let dst_start = data.storage.pixel_offset;
        data.bump_uniqueness();
        let new_uniqueness = data.uniqueness;
        let SurfaceData {
            ref mut storage,
            ref mut cached_dc,
            ..
        } = *data;
        if let Some(dc) = cached_dc.as_mut() {
            dc.finish_drawing(
                &mut storage.pixels[dst_start..dst_start + len],
                pitch,
                new_uniqueness,
                saved_state,
            );
        }
        data.access = Access::Ready;
        drop(data);
        self.data.notify_ready();
        self.refresh()
    }
    fn GetOverlayPosition(&self, x: *mut i32, y: *mut i32) -> Result<()> {
        self.get_overlay_position_impl(x, y)
    }
    fn SetOverlayPosition(&self, x: i32, y: i32) -> Result<()> {
        self.set_overlay_position_impl(x, y)
    }
    fn UpdateOverlay(
        &self,
        src_rect: *mut RECT,
        dest: Ref<IDirectDrawSurface>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
    }
    fn UpdateOverlayDisplay(&self, _: u32) -> Result<()> {
        self.refresh()
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface>) -> Result<()> {
        self.ensure_overlay()
    }
}

impl IDirectDrawSurface2_Impl for Surface_Impl {
    fn AddAttachedSurface(&self, surface: Ref<IDirectDrawSurface2>) -> Result<()> {
        self.add_attached_surface_impl(surface)
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
        self.blt_batch_impl(batch, count, flags)
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
    fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<IDirectDrawSurface2>) -> Result<()> {
        self.delete_attached_surface_impl(flags, surface)
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
        self.get_overlay_position_impl(x, y)
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
        self.set_overlay_position_impl(x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        self.unlock_ptr_impl(address)
    }
    fn UpdateOverlay(
        &self,
        src_rect: *mut RECT,
        dest: Ref<IDirectDrawSurface2>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface2>) -> Result<()> {
        self.ensure_overlay()
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
    fn AddAttachedSurface(&self, surface: Ref<IDirectDrawSurface3>) -> Result<()> {
        self.add_attached_surface_impl(surface)
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
        self.blt_batch_impl(batch, count, flags)
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
    fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<IDirectDrawSurface3>) -> Result<()> {
        self.delete_attached_surface_impl(flags, surface)
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
        self.get_overlay_position_impl(x, y)
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
        self.set_overlay_position_impl(x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, address: *mut c_void) -> Result<()> {
        self.unlock_ptr_impl(address)
    }
    fn UpdateOverlay(
        &self,
        src_rect: *mut RECT,
        dest: Ref<IDirectDrawSurface3>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface3>) -> Result<()> {
        self.ensure_overlay()
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
        // The caller supplies the descriptor and any backing image requested by DDSD_LPSURFACE.
        unsafe { self.set_surface_desc_impl(&crate::descriptor::read(desc)?, flags) }
    }
}

impl IDirectDrawSurface4_Impl for Surface_Impl {
    fn AddAttachedSurface(&self, surface: Ref<IDirectDrawSurface4>) -> Result<()> {
        self.add_attached_surface_impl(surface)
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
        self.blt_batch_impl(batch, count, flags)
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
    fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<IDirectDrawSurface4>) -> Result<()> {
        self.delete_attached_surface_impl(flags, surface)
    }
    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let snapshot: Vec<IDirectDrawSurface> = self.attached_lock().clone();
        for s in snapshot {
            let mut desc = desc_to_desc2(&surface_desc(&s)?);
            let owned = ManuallyDrop::new(s.cast::<IDirectDrawSurface4>()?);
            if unsafe { callback((&*owned).into(), &mut desc, context) }.0
                == DDENUMRET_CANCEL as i32
            {
                break;
            }
        }
        Ok(())
    }
    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        _: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let _ = callback.ok_or(E_INVALIDARG)?;
        if flags != DDENUMOVERLAYZ_FRONTTOBACK as u32 && flags != DDENUMOVERLAYZ_BACKTOFRONT as u32
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
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
        let out = unsafe { caps.cast::<MaybeUninit<DDSCAPS2>>().as_mut() }.ok_or(E_INVALIDARG)?;
        out.write(DDSCAPS2 {
            dwCaps: self.read()?.caps,
            ..Default::default()
        });
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
        self.get_overlay_position_impl(x, y)
    }
    fn GetPalette(&self) -> Result<IDirectDrawPalette> {
        IDirectDrawSurface_Impl::GetPalette(self)
    }
    fn GetPixelFormat(&self, out: *mut DDPIXELFORMAT) -> Result<()> {
        IDirectDrawSurface_Impl::GetPixelFormat(self, out)
    }
    fn GetSurfaceDesc(&self, out: *mut DDSURFACEDESC2) -> Result<()> {
        let out = unsafe { crate::descriptor::output(out)? };
        let count = self.backbuffer_chain_len();
        out.write(desc_to_desc2(&self.read()?.desc(count)));
        Ok(())
    }
    fn Initialize(&self, _: Ref<IDirectDraw>, _: *mut DDSURFACEDESC2) -> Result<()> {
        Err(DDERR_ALREADYINITIALIZED.into())
    }
    fn IsLost(&self) -> Result<()> {
        IDirectDrawSurface_Impl::IsLost(self)
    }
    fn Lock(&self, rect: *mut RECT, out: *mut DDSURFACEDESC2, flags: u32, _: HANDLE) -> Result<()> {
        unsafe { crate::descriptor::output(out)? }
            .write(desc_to_desc2(&self.lock_desc(rect, flags)?));
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
        self.set_overlay_position_impl(x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, rect: *mut RECT) -> Result<()> {
        self.unlock_rect_impl(rect)
    }
    fn UpdateOverlay(
        &self,
        src_rect: *mut RECT,
        dest: Ref<IDirectDrawSurface4>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface4>) -> Result<()> {
        self.ensure_overlay()
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
        // The caller supplies the descriptor and any backing image requested by DDSD_LPSURFACE.
        unsafe { self.set_surface_desc_impl(&crate::descriptor::read(desc)?, flags) }
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
    fn AddAttachedSurface(&self, surface: Ref<IDirectDrawSurface7>) -> Result<()> {
        self.add_attached_surface_impl(surface)
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
        self.blt_batch_impl(batch, count, flags)
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
    fn DeleteAttachedSurface(&self, flags: u32, surface: Ref<IDirectDrawSurface7>) -> Result<()> {
        self.delete_attached_surface_impl(flags, surface)
    }
    fn EnumAttachedSurfaces(
        &self,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let snapshot: Vec<IDirectDrawSurface> = self.attached_lock().clone();
        for s in snapshot {
            let mut desc = desc_to_desc2(&surface_desc(&s)?);
            let owned = ManuallyDrop::new(s.cast::<IDirectDrawSurface7>()?);
            if unsafe { callback((&*owned).into(), &mut desc, context) }.0
                == DDENUMRET_CANCEL as i32
            {
                break;
            }
        }
        Ok(())
    }
    fn EnumOverlayZOrders(
        &self,
        flags: u32,
        _: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let _ = callback.ok_or(E_INVALIDARG)?;
        if flags != DDENUMOVERLAYZ_FRONTTOBACK as u32 && flags != DDENUMOVERLAYZ_BACKTOFRONT as u32
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
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
        self.get_overlay_position_impl(x, y)
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
        self.set_overlay_position_impl(x, y)
    }
    fn SetPalette(&self, palette: Ref<IDirectDrawPalette>) -> Result<()> {
        IDirectDrawSurface_Impl::SetPalette(self, palette)
    }
    fn Unlock(&self, rect: *mut RECT) -> Result<()> {
        self.unlock_rect_impl(rect)
    }
    fn UpdateOverlay(
        &self,
        src_rect: *mut RECT,
        dest: Ref<IDirectDrawSurface7>,
        dest_rect: *mut RECT,
        flags: u32,
        fx: *mut DDOVERLAYFX,
    ) -> Result<()> {
        self.update_overlay_impl(src_rect, dest, dest_rect, flags, fx)
    }
    fn UpdateOverlayDisplay(&self, flags: u32) -> Result<()> {
        IDirectDrawSurface_Impl::UpdateOverlayDisplay(self, flags)
    }
    fn UpdateOverlayZOrder(&self, _: u32, _: Ref<IDirectDrawSurface7>) -> Result<()> {
        self.ensure_overlay()
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
            assert_eq!(surface.storage.pitch, 4);
            for (row, color) in surface
                .pixel_slice_mut()
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
            .pixel_slice_mut()
            .copy_from_slice(&[8, 32, 128, 0, 1, 2, 3, 96]);
        let mut rgba = Vec::new();
        frame_rgba(&surface, &mut rgba).unwrap();
        assert_eq!(rgba, [128, 32, 8, 255, 3, 2, 1, 255]);
    }

    #[test]
    fn gamma_ramp_remaps_expanded_rgba_channels() {
        let mut surface = SurfaceData::new(1, 1, Format::Xrgb, 0).unwrap();
        surface.pixel_slice_mut().copy_from_slice(&[10, 20, 30, 0]);
        let mut ramp = identity_gamma_ramp();
        ramp.red[30] = 0x8000;
        ramp.green[20] = 0x4000;
        ramp.blue[10] = 0x2000;
        surface.gamma = Some(ramp);
        let mut rgba = Vec::new();
        frame_rgba(&surface, &mut rgba).unwrap();
        assert_eq!(rgba, [0x80, 0x40, 0x20, 255]);
    }

    #[test]
    fn cross_format_mirror_blit_converts_rgb565_to_xrgb() {
        let mut src = SurfaceData::new(2, 1, Format::Rgb565, 0).unwrap();
        // Pixel 0 = red (0xf800), Pixel 1 = blue (0x001f)
        src.pixel_slice_mut()[0..2].copy_from_slice(&0xf800u16.to_le_bytes());
        src.pixel_slice_mut()[2..4].copy_from_slice(&0x001fu16.to_le_bytes());
        let mut dst = SurfaceData::new(2, 1, Format::Xrgb, 0).unwrap();
        let rect = RECT {
            left: 0,
            top: 0,
            right: 2,
            bottom: 1,
        };
        blit(
            &mut dst,
            rect,
            src.pixel_slice(),
            src.storage.pitch,
            Format::Rgb565,
            None,
            rect,
            BltOptions {
                mirror_lr: true,
                ..Default::default()
            },
        );
        // Mirrored: Pixel 0 = blue (BGRA: 255,0,0,0), Pixel 1 = red (BGRA: 0,0,255,0)
        assert_eq!(&dst.pixel_slice()[0..4], &[255, 0, 0, 0]);
        assert_eq!(&dst.pixel_slice()[4..8], &[0, 0, 255, 0]);
    }

    #[test]
    fn surface_allocates_top_and_bottom_guard_padding() {
        let surface = SurfaceData::new(640, 480, Format::Rgb565, 0).unwrap();
        assert_eq!(surface.storage.pitch, 1280);
        assert_eq!(surface.storage.image_len, 1280 * 480);
        assert!(surface.storage.pixel_offset >= 1280 * 64);
        assert!(
            surface.storage.pixels.len()
                - (surface.storage.pixel_offset + surface.storage.image_len)
                >= 1280 * 200
        );
    }

    #[test]
    fn get_dc_preserves_stable_surface_pointer_and_guard_padding() {
        let mut surface = SurfaceData::new(1024, 768, Format::Rgb565, 0).unwrap();
        let ptr_before = surface.surface_ptr();
        let offset_before = surface.storage.pixel_offset;
        surface.ensure_cached_dc().unwrap();
        assert_eq!(surface.surface_ptr(), ptr_before);
        assert_eq!(surface.storage.pixel_offset, offset_before);
        assert!(surface.storage.pixel_offset >= surface.storage.pitch * 64);
    }

    #[test]
    fn custom_surface_pitch_round_trips_through_gdi_and_rejects_partial_resize() {
        use windows::Win32::Foundation::COLORREF;
        use windows::Win32::Graphics::Gdi::{GetPixel, SetPixelV};

        let display = DisplayShared::new(2, 2, 32);
        let mut desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_WIDTH | DDSD_HEIGHT | DDSD_CAPS) as u32,
            dwWidth: 2,
            dwHeight: 2,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_OFFSCREENPLAIN as u32,
            },
            ..Default::default()
        };
        // Two visible eight-byte rows at pitch 12. There is no final padding.
        let mut pixels = [0xadu8; 20];
        pixels[12..16].copy_from_slice(&[1, 2, 3, 0]);
        desc.dwFlags |= (DDSD_PITCH | DDSD_LPSURFACE) as u32;
        desc.Anonymous1.lPitch = 12;
        desc.lpSurface = pixels.as_mut_ptr().cast();
        // All COM input/output buffers live through the calls.
        unsafe {
            let surface: IDirectDrawSurface3 = create_surface(display.clone(), &desc)
                .unwrap()
                .cast()
                .unwrap();
            let obj = surface.cast_object_ref::<Surface>().unwrap();
            {
                let data = obj.read().unwrap();
                assert_eq!(data.pitch(), 8);
                assert_eq!(&data.pixel_slice()[8..12], &[1, 2, 3, 0]);
            }
            surface.SetSurfaceDesc(&mut desc, 0).unwrap();
            {
                let mut data = obj.write().unwrap();
                // Import skipped the external 0xad padding. External code
                // may subsequently use padding in the surface it has locked.
                assert_eq!(&data.pixel_slice()[8..12], &[0; 4]);
                assert_eq!(&data.pixel_slice()[20..24], &[0; 4]);
                data.pixel_slice_mut()[20..24].fill(0x57);
            }
            desc.dwFlags = (DDSD_WIDTH | DDSD_PITCH) as u32;
            desc.dwWidth = 4;
            assert_eq!(
                surface.SetSurfaceDesc(&mut desc, 0).unwrap_err().code(),
                E_INVALIDARG
            );
            let mut dc = HDC::default();
            surface.GetDC(&mut dc).unwrap();
            assert_eq!(GetPixel(dc, 0, 1), COLORREF(0x010203));
            assert!(SetPixelV(dc, 1, 1, COLORREF(0x080706)).as_bool());
            surface.ReleaseDC(dc).unwrap();
            surface.GetSurfaceDesc(&mut desc).unwrap();
            assert_eq!(
                (desc.dwWidth, desc.dwHeight, desc.Anonymous1.lPitch),
                (2, 2, 12)
            );
            let duplicate = duplicate_surface(display, &surface.cast().unwrap()).unwrap();
            let data = surface
                .cast_object_ref::<Surface>()
                .unwrap()
                .read()
                .unwrap();
            assert_eq!(&data.pixel_slice()[16..19], &[8, 7, 6]);
            assert_eq!(&data.pixel_slice()[20..24], &[0x57; 4]);
            let copy = duplicate
                .cast_object_ref::<Surface>()
                .unwrap()
                .read()
                .unwrap();
            assert_eq!(copy.pitch(), 12);
            assert_eq!(copy.pixel_slice(), data.pixel_slice());
        }
    }

    #[test]
    fn cross_thread_primary_lock_and_blt_never_fail_with_null_surface() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Barrier;

        let display = crate::draw::DisplayShared::new(1024, 768, 16);
        let prim_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: DDSD_CAPS as u32,
            ddsCaps: DDSCAPS {
                dwCaps: DDSCAPS_PRIMARYSURFACE as u32,
            },
            ..Default::default()
        };
        let back_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwFlags: (DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT) as u32,
            dwWidth: 1024,
            dwHeight: 768,
            ddsCaps: DDSCAPS {
                dwCaps: (DDSCAPS_OFFSCREENPLAIN | DDSCAPS_SYSTEMMEMORY) as u32,
            },
            ..Default::default()
        };
        // Both descriptors request owned surface memory.
        let primary = unsafe { create_surface(display.clone(), &prim_desc) }.unwrap();
        let back = unsafe { create_surface(display.clone(), &back_desc) }.unwrap();

        // Same-thread Lock while GetDC is held must still fail immediately (no self-deadlock).
        let mut dc = HDC::default();
        unsafe { primary.GetDC(&mut dc) }.unwrap();
        let mut same_thread_desc = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            ..Default::default()
        };
        assert_eq!(
            unsafe {
                primary.Lock(
                    std::ptr::null_mut(),
                    &mut same_thread_desc,
                    0,
                    HANDLE::default(),
                )
            }
            .unwrap_err()
            .code(),
            DDERR_SURFACEBUSY
        );
        unsafe { primary.ReleaseDC(dc) }.unwrap();

        // Cross-thread DONOTWAIT must still return DDERR_SURFACEBUSY immediately when locked.
        let mut main_lock = DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            ..Default::default()
        };
        unsafe { primary.Lock(std::ptr::null_mut(), &mut main_lock, 0, HANDLE::default()) }
            .unwrap();
        let weak_primary = primary.downgrade().unwrap();
        let worker_surface = weak_primary.clone();
        let dnw_code = std::thread::spawn(move || unsafe {
            let s = worker_surface.upgrade().unwrap();
            let mut d = DDSURFACEDESC {
                dwSize: size_of::<DDSURFACEDESC>() as u32,
                ..Default::default()
            };
            s.Lock(
                std::ptr::null_mut(),
                &mut d,
                DDLOCK_DONOTWAIT as u32,
                HANDLE::default(),
            )
            .unwrap_err()
            .code()
        })
        .join()
        .unwrap();
        assert_eq!(dnw_code, DDERR_SURFACEBUSY);
        unsafe { primary.Unlock(main_lock.lpSurface) }.unwrap();

        // Stress test: simulate RA2's background cursor worker thread calling Primary->Lock()
        // and reading a 37x28 rect at (82, 353) concurrently with main-thread Blt + Lock + present reads.
        let done = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(2));
        let done_worker = Arc::clone(&done);
        let start_worker = Arc::clone(&start);
        let worker = std::thread::spawn(move || unsafe {
            let s = weak_primary.upgrade().unwrap();
            start_worker.wait();
            let mut cursor_buf = [0u16; 37 * 28];
            let mut locks = 0u32;
            while !done_worker.load(Ordering::Relaxed) || locks < 2000 {
                let mut desc = DDSURFACEDESC {
                    dwSize: size_of::<DDSURFACEDESC>() as u32,
                    ..Default::default()
                };
                s.Lock(
                    std::ptr::null_mut(),
                    &mut desc,
                    DDLOCK_WAIT as u32,
                    HANDLE::default(),
                )
                .expect("cross-thread DDLOCK_WAIT must never fail with DDERR_SURFACEBUSY");
                assert!(!desc.lpSurface.is_null());
                let pitch = desc.Anonymous1.lPitch as usize;
                let base = desc.lpSurface.cast::<u8>();
                for row in 0..28usize {
                    let src_row = base.add((353 + row) * pitch + 82 * 2).cast::<u16>();
                    std::ptr::copy_nonoverlapping(
                        src_row,
                        cursor_buf.as_mut_ptr().add(row * 37),
                        37,
                    );
                }
                s.Unlock(desc.lpSurface).unwrap();
                locks += 1;
                if locks >= 4000 {
                    break;
                }
            }
            locks
        });

        let primary_slot = display.primary_slot().unwrap();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 256,
            bottom: 256,
        };
        start.wait();
        for i in 0..1000u32 {
            unsafe {
                primary
                    .Blt(
                        &mut rect,
                        &back,
                        &mut rect,
                        DDBLT_WAIT as u32,
                        std::ptr::null_mut(),
                    )
                    .unwrap();
            }
            if i % 4 == 0 {
                let mut desc = DDSURFACEDESC {
                    dwSize: size_of::<DDSURFACEDESC>() as u32,
                    ..Default::default()
                };
                unsafe {
                    primary
                        .Lock(
                            std::ptr::null_mut(),
                            &mut desc,
                            DDLOCK_WAIT as u32,
                            HANDLE::default(),
                        )
                        .unwrap();
                    *desc.lpSurface.cast::<u16>() = i as u16;
                    primary.Unlock(desc.lpSurface).unwrap();
                }
            }
            // Simulate present_surface brief pixel read under SurfaceSlot lock.
            let guard = primary_slot.lock();
            if !guard.locked() {
                let _ = guard.pixel_slice()[0];
            }
        }
        done.store(true, Ordering::Relaxed);
        let worker_locks = worker.join().unwrap();
        assert!(worker_locks >= 2000);
    }
}
