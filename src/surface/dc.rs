//! Owns the GDI drawing context, selected bitmap, and the bitmap's scanline layout.
use std::ffi::c_void;
use std::mem::size_of;

use super::model::{Format, SurfaceData, DDERR_CANTCREATEDC};
use crate::simd_blit;
use windows::core::Result;
use windows::Win32::Foundation::E_OUTOFMEMORY;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, RestoreDC, SaveDC,
    SelectObject, SetDIBColorTable, BITMAPINFO, BITMAPINFOHEADER, BI_BITFIELDS, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PALETTEENTRY, RGBQUAD,
};

#[repr(C)]
struct DibInfo {
    header: BITMAPINFOHEADER,
    // GDI interprets the trailing bytes as either RGBQUADs or three DWORD masks.
    table: [u32; 256],
}

/// Opaque GDI `HDC` wrapper implementing `Send + Sync` so `SurfaceData` and `SurfaceSlot`
/// automatically derive thread-safety without broad `unsafe impl Send/Sync`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct SendHdc(pub(super) HDC);

// SAFETY: A GDI `HDC` is an opaque handle; concurrent access is serialized by `SurfaceSlot`'s Mutex.
unsafe impl Send for SendHdc {}
// SAFETY: Sharing an opaque `HDC` identifier across threads carries no Rust aliasing hazard.
unsafe impl Sync for SendHdc {}

pub(super) struct SurfaceDc {
    handle: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
    pitch: usize,
    row_bytes: usize,
    byte_len: usize,
    synced_uniqueness: u32,
}

// SAFETY: `SurfaceDc` exclusively owns its memory DC, DIB bitmap, and DIB section memory.
// It is accessed strictly under `SurfaceSlot`'s Mutex outside active GDI drawing intervals.
unsafe impl Send for SurfaceDc {}

impl SurfaceDc {
    pub(super) fn new(surface: &SurfaceData) -> Result<Self> {
        // A DIB always uses DWORD-aligned scanlines, regardless of a game's DDSD_PITCH.
        let row_bytes = surface.width as usize * surface.format.bytes();
        let pitch = row_bytes.next_multiple_of(4);
        let byte_len = pitch
            .checked_mul(surface.height as usize)
            .filter(|&len| len <= isize::MAX as usize)
            .ok_or(E_OUTOFMEMORY)?;
        let mut info = DibInfo {
            header: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: surface.width as i32,
                biHeight: -(surface.height as i32), // top-down
                biPlanes: 1,
                biBitCount: (surface.format.bytes() * 8) as u16,
                biCompression: BI_RGB.0,
                biSizeImage: byte_len as u32,
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
                info.table[..3].copy_from_slice(&surface.format.rgb_masks());
            }
            Format::Xrgb => {}
        }
        let mut dc = Self {
            // SAFETY: Passing None creates a memory DC compatible with the screen.
            handle: unsafe { CreateCompatibleDC(None) },
            bitmap: HBITMAP::default(),
            previous: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            pitch,
            row_bytes,
            byte_len,
            synced_uniqueness: 0,
        };
        if dc.handle.is_invalid() {
            return Err(DDERR_CANTCREATEDC.into());
        }
        // SAFETY: `info` is a valid `BITMAPINFOHEADER` + color table/bitfields; `dc.handle` is a
        // valid memory DC; `CreateDIBSection` initializes `dc.bits` with `byte_len` writable bytes.
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
            // Initialize scanline padding too, before exposing the DIB as a Rust byte slice.
            dc.bits.cast::<u8>().write_bytes(0, byte_len);
        }
        Ok(dc)
    }

    // Called only during GetDC/ReleaseDC while holding the surface lock, outside
    // the interval in which the caller is permitted to draw with the HDC.
    pub(super) fn begin_drawing(
        &mut self,
        pixels: &[u8],
        pitch: usize,
        uniqueness: u32,
        entries: Option<&[PALETTEENTRY; 256]>,
        own_dc: bool,
    ) -> (HDC, i32) {
        if self.synced_uniqueness != uniqueness {
            // SAFETY: new() owns a live DIB of exactly byte_len bytes until Drop.
            let bits =
                unsafe { std::slice::from_raw_parts_mut(self.bits.cast::<u8>(), self.byte_len) };
            copy_rows(pixels, pitch, bits, self.pitch, self.row_bytes);
            self.synced_uniqueness = uniqueness;
        }
        if let Some(entries) = entries {
            self.sync_palette(entries);
        }
        let saved_state = if own_dc {
            0
        } else {
            // SAFETY: `self.handle` is a valid memory DC owned by `self`.
            unsafe { SaveDC(self.handle) }
        };
        (self.handle, saved_state)
    }

    pub(super) fn finish_drawing(
        &mut self,
        pixels: &mut [u8],
        pitch: usize,
        uniqueness: u32,
        saved_state: i32,
    ) {
        // SAFETY: Flush queued GDI draws on this thread and restore DC state if saved.
        unsafe {
            let _ = GdiFlush();
            if saved_state != 0 {
                let _ = RestoreDC(self.handle, saved_state);
            }
        }
        // SAFETY: The caller has finished drawing and flushed GDI, and the DIB is live.
        let bits = unsafe { std::slice::from_raw_parts(self.bits.cast::<u8>(), self.byte_len) };
        copy_rows(bits, self.pitch, pixels, pitch, self.row_bytes);
        self.synced_uniqueness = uniqueness;
    }

    fn sync_palette(&self, entries: &[PALETTEENTRY; 256]) {
        let mut quads = [RGBQUAD::default(); 256];
        for (q, e) in quads.iter_mut().zip(entries.iter()) {
            q.rgbBlue = e.peBlue;
            q.rgbGreen = e.peGreen;
            q.rgbRed = e.peRed;
        }
        // SAFETY: `self.handle` has an 8-bit DIB selected and `quads` has 256 initialized entries.
        unsafe {
            let _ = SetDIBColorTable(self.handle, 0, &quads);
        }
    }
}

fn copy_rows(src: &[u8], src_pitch: usize, dst: &mut [u8], dst_pitch: usize, row_bytes: usize) {
    if src_pitch == row_bytes && dst_pitch == row_bytes {
        simd_blit::copy_bytes(src, dst);
    } else {
        for (from, to) in src
            .chunks_exact(src_pitch)
            .zip(dst.chunks_exact_mut(dst_pitch))
        {
            to[..row_bytes].copy_from_slice(&from[..row_bytes]);
        }
    }
}

impl Drop for SurfaceDc {
    fn drop(&mut self) {
        // SAFETY: Restores the original bitmap before deleting the memory DC and DIB section.
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
