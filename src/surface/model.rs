//! Pure Rust DirectDraw surface data model, pixel formats, guard-padded storage, and synchronization.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use windows::core::{Error, IUnknown, Result, GUID, HRESULT};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, RECT};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{HDC, PALETTEENTRY};

use super::dc::{SendHdc, SurfaceDc};
use crate::ffi::{descriptor, ptr::SendCom};

pub(super) const fn dd_hresult(code: u32) -> HRESULT {
    HRESULT((0x8876_0000 | code) as i32)
}

pub(super) const DDERR_ALREADYINITIALIZED: HRESULT = dd_hresult(5);
pub(super) const DDERR_INVALIDCAPS: HRESULT = dd_hresult(100);
pub(super) const DDERR_INVALIDPIXELFORMAT: HRESULT = dd_hresult(145);
pub(super) const DDERR_INVALIDRECT: HRESULT = dd_hresult(150);
pub(super) const DDERR_NOCOLORKEY: HRESULT = dd_hresult(215);
pub(super) const DDERR_NOTFOUND: HRESULT = dd_hresult(255);
pub(super) const DDERR_SURFACEALREADYATTACHED: HRESULT = dd_hresult(410);
pub(super) const DDERR_SURFACEBUSY: HRESULT = dd_hresult(430);
pub(super) const DDERR_SURFACENOTATTACHED: HRESULT = dd_hresult(460);
pub(super) const DDERR_NOCLIPPERATTACHED: HRESULT = dd_hresult(568);
pub(super) const DDERR_NOPALETTEATTACHED: HRESULT = dd_hresult(572);
pub(super) const DDERR_NOTAOVERLAYSURFACE: HRESULT = dd_hresult(580);
pub(super) const DDERR_NOTFLIPPABLE: HRESULT = dd_hresult(582);
pub(super) const DDERR_CANTDUPLICATE: HRESULT = dd_hresult(583);
pub(super) const DDERR_NOTLOCKED: HRESULT = dd_hresult(584);
pub(super) const DDERR_CANTCREATEDC: HRESULT = dd_hresult(585);
pub(super) const DDERR_NODC: HRESULT = dd_hresult(586);
pub(super) const DDERR_DCALREADYCREATED: HRESULT = dd_hresult(620);
pub(super) const DDERR_MOREDATA: HRESULT = dd_hresult(690);

pub(super) const MAX_BACKBUFFERS: u32 = 8;
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

pub(super) fn unsupported<T>() -> Result<T> {
    Err(E_NOTIMPL.into())
}

pub(super) fn zeros(size: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    data.try_reserve_exact(size)
        .map_err(|_| Error::from(E_OUTOFMEMORY))?;
    data.resize(size, 0);
    Ok(data)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    Indexed = 0,
    Rgb555 = 1,
    Rgb565 = 2,
    Xrgb = 3,
}

pub(super) type Format = PixelFormat;

impl PixelFormat {
    pub(super) fn from_depth(bpp: u32) -> Result<Self> {
        match bpp {
            8 => Ok(Self::Indexed),
            16 => Ok(Self::Rgb565),
            32 => Ok(Self::Xrgb),
            _ => Err(DDERR_INVALIDPIXELFORMAT.into()),
        }
    }

    pub(super) fn from_dd(pf: &DDPIXELFORMAT) -> Result<Self> {
        if pf.dwSize as usize != size_of::<DDPIXELFORMAT>()
            || pf.dwFlags & !(DDPF_RGB | DDPF_PALETTEINDEXED8 | DDPF_ALPHAPIXELS) as u32 != 0
        {
            return Err(DDERR_INVALIDPIXELFORMAT.into());
        }
        match descriptor::rgb_layout(pf) {
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

    pub(super) fn rgb_masks(self) -> [u32; 3] {
        match self {
            Self::Indexed => [0, 0, 0],
            Self::Rgb555 => [0x7c00, 0x03e0, 0x001f],
            Self::Rgb565 => [0xf800, 0x07e0, 0x001f],
            Self::Xrgb => [0xff0000, 0xff00, 0xff],
        }
    }

    pub(super) fn description(self) -> DDPIXELFORMAT {
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

    pub(super) fn decode_rgb(self, pixel: &[u8], palette: &[PALETTEENTRY; 256]) -> [u8; 3] {
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

    pub(super) fn encode_rgb(self, rgb: [u8; 3], out: &mut [u8]) {
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
pub(super) enum Access {
    Ready,
    Locked {
        rect: RECT,
        owner_tid: ThreadId,
    },
    Gdi {
        handle: SendHdc,
        saved_state: i32,
        owner_tid: ThreadId,
    },
}

const CROSS_THREAD_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Thread-safe synchronization wrapper around `SurfaceData` supporting cross-thread
/// `DDLOCK_WAIT` / `DDBLT_WAIT` / `DDFLIP_WAIT` coordination without spurious `DDERR_SURFACEBUSY`.
/// `Send + Sync` are derived automatically by the compiler because every field inside
/// `SurfaceData` is `Send` and guarded by `Mutex`.
pub struct SurfaceSlot {
    data: Mutex<SurfaceData>,
    ready_cv: Condvar,
}

impl SurfaceSlot {
    pub(super) fn new(data: SurfaceData) -> Self {
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
    pub(super) fn lock_ready(&self, wait: bool) -> Result<MutexGuard<'_, SurfaceData>> {
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

    pub(super) fn notify_ready(&self) {
        self.ready_cv.notify_all();
    }
}

/// Acquires two distinct `SurfaceSlot`s in canonical pointer-address order and verifies both
/// are `Access::Ready` without ever holding one mutex while waiting on the other's `Condvar`.
pub(super) fn lock_pair_ready<'a>(
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
pub(super) fn lock_chain_ready<'a>(
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

pub(super) enum PrivateData {
    Bytes(Vec<u8>),
    Unknown(SendCom<IUnknown>),
}

#[derive(Default)]
pub(super) struct OverlayState {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) visible: bool,
}

/// The byte allocation and the layout used to address it travel together when flipping or resizing.
pub(super) struct PixelStorage {
    pub(super) pitch: usize,
    pub(super) pixel_offset: usize,
    pub(super) image_len: usize,
    pub(super) pixels: Vec<u8>,
}

impl PixelStorage {
    pub(super) fn new(
        width: u32,
        height: u32,
        format: Format,
        supplied_pitch: Option<i32>,
    ) -> Result<Self> {
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

    pub(super) fn image_mut(&mut self) -> &mut [u8] {
        &mut self.pixels[self.pixel_offset..self.pixel_offset + self.image_len]
    }
}

pub struct SurfaceData {
    pub width: u32,
    pub height: u32,
    pub(super) access: Access,
    pub(super) format: Format,
    pub(super) caps: u32,
    pub(super) storage: PixelStorage,
    pub(super) cached_dc: Option<SurfaceDc>,
    pub(super) palette: Option<SendCom<IDirectDrawPalette>>,
    pub(super) clipper: Option<SendCom<IDirectDrawClipper>>,
    pub(super) source_key: Option<DDCOLORKEY>,
    pub(super) dest_key: Option<DDCOLORKEY>,
    pub(super) src_overlay_key: Option<DDCOLORKEY>,
    pub(super) dest_overlay_key: Option<DDCOLORKEY>,
    pub(super) gamma: Option<DDGAMMARAMP>,
    pub(super) private_data: HashMap<GUID, PrivateData>,
    pub(super) overlay: OverlayState,
    pub(super) uniqueness: u32,
    pub(super) priority: u32,
    pub(super) lod: u32,
}

impl SurfaceData {
    pub(super) fn new(width: u32, height: u32, format: Format, caps: u32) -> Result<Self> {
        Self::with_pitch(width, height, format, caps, None)
    }

    pub(super) fn with_pitch(
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

    pub(super) fn pixel_slice_mut(&mut self) -> &mut [u8] {
        self.storage.image_mut()
    }

    pub(super) fn surface_ptr(&mut self) -> *mut u8 {
        self.storage
            .pixels
            .as_mut_ptr()
            .wrapping_add(self.storage.pixel_offset)
    }

    pub(super) fn surface_ptr_const(&self) -> *const u8 {
        self.storage
            .pixels
            .as_ptr()
            .wrapping_add(self.storage.pixel_offset)
    }

    pub(super) fn ensure_cached_dc(&mut self) -> Result<()> {
        if self.cached_dc.is_none() {
            self.cached_dc = Some(SurfaceDc::new(self)?);
        }
        Ok(())
    }

    pub fn byte_len(&self) -> usize {
        self.storage.image_len
    }

    pub(super) fn has_same_layout(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.storage.pitch == other.storage.pitch
            && self.format == other.format
    }

    pub fn has_dc(&self, hdc: HDC) -> bool {
        matches!(self.access, Access::Gdi { handle, .. } if handle.0 == hdc)
    }

    pub fn is_primary(&self) -> bool {
        self.caps & DDSCAPS_PRIMARYSURFACE as u32 != 0
    }

    pub(super) fn is_overlay(&self) -> bool {
        self.caps & DDSCAPS_OVERLAY as u32 != 0
    }

    pub(super) fn bump_uniqueness(&mut self) {
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

    pub(super) fn clamp_rect(&self, rect: Option<RECT>) -> Option<RECT> {
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

    pub(super) fn bounds(&self, rect: Option<RECT>) -> Result<RECT> {
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

    pub(super) fn desc(&self, backbuffer_count: u32) -> DDSURFACEDESC {
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

    pub fn locked_by_other_thread(&self) -> bool {
        self.locked() && self.lock_owner_tid() != Some(thread::current().id())
    }

    fn lock_owner_tid(&self) -> Option<ThreadId> {
        match self.access {
            Access::Ready => None,
            Access::Locked { owner_tid, .. } | Access::Gdi { owner_tid, .. } => Some(owner_tid),
        }
    }

    pub(super) fn ready(&self) -> Result<()> {
        if self.locked() {
            Err(DDERR_SURFACEBUSY.into())
        } else {
            Ok(())
        }
    }

    pub fn uses_palette(&self, palette: &IDirectDrawPalette) -> bool {
        self.palette.as_deref() == Some(palette)
    }

    pub(super) fn palette_entries(&self) -> Result<[PALETTEENTRY; 256]> {
        if self.format == Format::Indexed {
            if let Some(palette) = self.palette.as_deref() {
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

pub(super) fn identity_gamma_ramp() -> DDGAMMARAMP {
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
pub(super) struct BltOptions {
    pub(super) src_key: Option<DDCOLORKEY>,
    pub(super) dst_key: Option<DDCOLORKEY>,
    pub(super) mirror_lr: bool,
    pub(super) mirror_ud: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::blit::blit;

    const LUT5: [u8; 32] = {
        let mut table = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            table[i] = (i * 255 / 31) as u8;
            i += 1;
        }
        table
    };

    const LUT6: [u8; 64] = {
        let mut table = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            table[i] = (i * 255 / 63) as u8;
            i += 1;
        }
        table
    };

    fn frame_rgba(data: &SurfaceData, rgba: &mut Vec<u8>) -> Result<()> {
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
}
