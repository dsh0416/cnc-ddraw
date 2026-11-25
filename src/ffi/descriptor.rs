//! Size-prefixed DirectDraw structures and union field accessors at the COM boundary.
use std::mem::{offset_of, size_of, MaybeUninit};

use windows::core::Result;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::DirectDraw::*;

mod sealed {
    pub trait Sealed {}
}

pub(crate) trait Descriptor: Copy + sealed::Sealed {}

macro_rules! descriptor {
    ($ty:ty) => {
        const _: () = assert!(offset_of!($ty, dwSize) == 0);
        const _: fn(&$ty) -> &u32 = |d| &d.dwSize;
        impl sealed::Sealed for $ty {}
        impl Descriptor for $ty {}
    };
}

descriptor!(DDSURFACEDESC);
descriptor!(DDSURFACEDESC2);
descriptor!(DDPIXELFORMAT);

pub(crate) fn rgb_layout(format: &DDPIXELFORMAT) -> (u32, [u32; 3]) {
    // SAFETY: Each member occupies an entire DWORD (including the pair of WORDs
    // in MultiSampleCaps). The RGB fields can represent every bit pattern.
    unsafe {
        (
            format.Anonymous1.dwRGBBitCount,
            [
                format.Anonymous2.dwRBitMask,
                format.Anonymous3.dwGBitMask,
                format.Anonymous4.dwBBitMask,
            ],
        )
    }
}

pub(crate) fn supplied_pitch(desc: &DDSURFACEDESC) -> Option<i32> {
    if desc.dwFlags & DDSD_PITCH as u32 != 0 {
        // SAFETY: `DDSD_PITCH` selects the signed `lPitch` member of this DWORD union.
        Some(unsafe { desc.Anonymous1.lPitch })
    } else {
        None
    }
}

pub(crate) fn refresh_rate(desc: &DDSURFACEDESC) -> Option<u32> {
    if desc.dwFlags & DDSD_REFRESHRATE as u32 != 0 {
        // SAFETY: `DDSD_REFRESHRATE` selects the `dwRefreshRate` DWORD union member.
        Some(unsafe { desc.Anonymous2.dwRefreshRate })
    } else {
        None
    }
}

pub(crate) fn bltfx_fill_color(fx: &DDBLTFX) -> u32 {
    // SAFETY: `Anonymous5` is a union of DWORD-sized fill/pointer values; `dwFillColor` is valid
    // for all bit patterns when `DDBLT_COLORFILL` or `PATCOPY` is requested.
    unsafe { fx.Anonymous5.dwFillColor }
}

pub(crate) fn bltfx_fill_depth(fx: &DDBLTFX) -> u32 {
    // SAFETY: `dwFillDepth` is a DWORD union member valid for all bit patterns.
    unsafe { fx.Anonymous5.dwFillDepth }
}

pub(crate) fn desc_to_desc2(d: &DDSURFACEDESC) -> DDSURFACEDESC2 {
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
    // SAFETY: `d` is an initialized Rust-owned `DDSURFACEDESC` whose DWORD unions hold valid bits.
    unsafe {
        d2.Anonymous1.lPitch = d.Anonymous1.lPitch;
        d2.Anonymous3.dwMipMapCount = d.Anonymous2.dwMipMapCount;
    }
    d2.Anonymous2.dwBackBufferCount = d.dwBackBufferCount;
    d2.Anonymous4.ddckCKDestOverlay = d.ddckCKDestOverlay;
    d2.Anonymous5.ddpfPixelFormat = d.ddpfPixelFormat;
    d2
}

/// Validates a caller-provided `DDCAPS_DX1..DX7` header and returns its byte size if non-null.
///
/// # Safety
/// If non-null, `output` must have a readable, aligned `u32` size header at offset 0.
pub(crate) unsafe fn read_caps_size(output: *mut DDCAPS_DX7) -> Result<Option<u32>> {
    if output.is_null() {
        return Ok(None);
    }
    // SAFETY: Caller guarantees a readable, aligned `u32` dwSize header at offset 0.
    let size = unsafe { output.cast::<u32>().read() };
    if ![
        size_of::<DDCAPS_DX1>(),
        size_of::<DDCAPS_DX3>(),
        size_of::<DDCAPS_DX5>(),
        size_of::<DDCAPS_DX6>(),
        size_of::<DDCAPS_DX7>(),
    ]
    .contains(&(size as usize))
    {
        return Err(E_INVALIDARG.into());
    }
    Ok(Some(size))
}

/// Copies the leading `size` bytes of `caps` into a legacy `DDCAPS_DX1..DX7` output buffer.
///
/// # Safety
/// `output` must be exclusively writable for `size` bytes, and `size <= size_of::<DDCAPS_DX7>()`.
pub(crate) unsafe fn write_caps_prefix(output: *mut DDCAPS_DX7, caps: &DDCAPS_DX7, size: u32) {
    debug_assert!((size as usize) <= size_of::<DDCAPS_DX7>());
    // SAFETY: Legacy DDCAPS layouts are prefixes of DX7; `output` is writable for `size` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&raw const *caps).cast::<u8>(),
            output.cast::<u8>(),
            size as usize,
        );
    }
}

// A caller supplying a different version may have allocated only that version's size.
// Read the common DWORD header before accessing or borrowing the complete structure.
unsafe fn check_size<T: Descriptor>(ptr: *const T) -> Result<()> {
    // SAFETY: Caller guarantees a non-null `ptr` has a readable, aligned `u32` dwSize at offset 0.
    if ptr.is_null() || unsafe { ptr.cast::<u32>().read() } as usize != size_of::<T>() {
        return Err(E_INVALIDARG.into());
    }
    Ok(())
}

// Only the RGB variants are supported. Indexed colors have no RGB masks.
unsafe fn read_pixel_format(ptr: *const DDPIXELFORMAT) -> DDPIXELFORMAT {
    // SAFETY: Caller guarantees `ptr` points to a readable `DDPIXELFORMAT` with flagged fields set.
    unsafe {
        let mut value = DDPIXELFORMAT {
            dwSize: (*ptr).dwSize,
            dwFlags: (*ptr).dwFlags,
            ..Default::default()
        };
        if value.dwFlags & DDPF_RGB as u32 != 0 {
            value.Anonymous1.dwRGBBitCount = (*ptr).Anonymous1.dwRGBBitCount;
            if value.dwFlags & DDPF_PALETTEINDEXED8 as u32 == 0 {
                value.Anonymous2.dwRBitMask = (*ptr).Anonymous2.dwRBitMask;
                value.Anonymous3.dwGBitMask = (*ptr).Anonymous3.dwGBitMask;
                value.Anonymous4.dwBBitMask = (*ptr).Anonymous4.dwBBitMask;
            }
        } else if value.dwFlags & DDPF_PALETTEINDEXED8 as u32 != 0 {
            value.Anonymous1.dwRGBBitCount = 8;
        }
        value
    }
}

pub(crate) trait SurfaceDescriptor: Descriptor {
    /// # Safety
    /// `ptr` must span an aligned `Self` with `dwFlags` and all flagged fields initialized.
    unsafe fn read_fields(ptr: *const Self) -> DDSURFACEDESC;
}

// Both SDK versions carry the same surface inputs, with some fields moved into
// different unions. Generate typed field accesses against each SDK definition.
macro_rules! surface_descriptor {
    ($ty:ty, $($count:ident).+, $rate:ident, $($key:ident).+, $($pf:ident).+) => {
        impl SurfaceDescriptor for $ty {
            unsafe fn read_fields(ptr: *const Self) -> DDSURFACEDESC {
                // SAFETY: C clients can leave inactive fields uninitialized. Never borrow or
                // copy the input structure as a whole, including its nested unions; only read
                // fields selected by `dwFlags`.
                unsafe {
                    let flags = (*ptr).dwFlags;
                    let has = |flag: i32| flags & flag as u32 != 0;
                    let mut desc = DDSURFACEDESC {
                        dwSize: size_of::<DDSURFACEDESC>() as u32,
                        dwFlags: flags,
                        ..Default::default()
                    };
                    if has(DDSD_WIDTH) {
                        desc.dwWidth = (*ptr).dwWidth;
                    }
                    if has(DDSD_HEIGHT) {
                        desc.dwHeight = (*ptr).dwHeight;
                    }
                    if has(DDSD_CAPS) {
                        desc.ddsCaps.dwCaps = (*ptr).ddsCaps.dwCaps;
                    }
                    if has(DDSD_PITCH) {
                        desc.Anonymous1.lPitch = (*ptr).Anonymous1.lPitch;
                    }
                    if has(DDSD_BACKBUFFERCOUNT) {
                        desc.dwBackBufferCount = (*ptr).$($count).+;
                    }
                    if has(DDSD_REFRESHRATE) {
                        desc.Anonymous2.dwRefreshRate = (*ptr).$rate.dwRefreshRate;
                    }
                    if has(DDSD_LPSURFACE) {
                        desc.lpSurface = (*ptr).lpSurface;
                    }
                    if has(DDSD_CKDESTOVERLAY) {
                        desc.ddckCKDestOverlay = (*ptr).$($key).+;
                    }
                    if has(DDSD_CKDESTBLT) {
                        desc.ddckCKDestBlt = (*ptr).ddckCKDestBlt;
                    }
                    if has(DDSD_CKSRCOVERLAY) {
                        desc.ddckCKSrcOverlay = (*ptr).ddckCKSrcOverlay;
                    }
                    if has(DDSD_CKSRCBLT) {
                        desc.ddckCKSrcBlt = (*ptr).ddckCKSrcBlt;
                    }
                    if has(DDSD_PIXELFORMAT) {
                        desc.ddpfPixelFormat = read_pixel_format(&raw const (*ptr).$($pf).+);
                    }
                    desc
                }
            }
        }
    };
}

surface_descriptor!(
    DDSURFACEDESC,
    dwBackBufferCount,
    Anonymous2,
    ddckCKDestOverlay,
    ddpfPixelFormat
);
surface_descriptor!(
    DDSURFACEDESC2,
    Anonymous2.dwBackBufferCount,
    Anonymous3,
    Anonymous4.ddckCKDestOverlay,
    Anonymous5.ddpfPixelFormat
);

/// # Safety
/// A non-null input must have a readable, aligned size header. On a size match,
/// the allocation must span `T`, with flags and the fields they select initialized.
pub(crate) unsafe fn read<T: SurfaceDescriptor>(ptr: *const T) -> Result<DDSURFACEDESC> {
    // SAFETY: Preconditions match `check_size` and `SurfaceDescriptor::read_fields`.
    unsafe {
        check_size(ptr)?;
        Ok(T::read_fields(ptr))
    }
}

/// # Safety
/// A non-null pointer must have a readable, aligned size header. When the size
/// matches, the whole allocation must be aligned and exclusively writable for
/// the returned borrow. Only the header needs to be initialized.
pub(crate) unsafe fn output<'a, T: Descriptor>(ptr: *mut T) -> Result<&'a mut MaybeUninit<T>> {
    // SAFETY: `check_size` validates non-null and exact struct size before casting to `MaybeUninit`.
    unsafe {
        check_size(ptr)?;
        Ok(&mut *ptr.cast::<MaybeUninit<T>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_short_buffer_using_only_its_header() {
        let mut size = size_of::<u32>() as u32;
        let ptr = (&raw mut size).cast::<DDSURFACEDESC2>();
        // SAFETY: `size` provides a valid 4-byte `dwSize` header that mismatches `DDSURFACEDESC2`.
        assert_eq!(unsafe { read(ptr) }.err().unwrap().code(), E_INVALIDARG);
        // SAFETY: Same short-buffer header check for `output`.
        assert_eq!(unsafe { output(ptr) }.err().unwrap().code(), E_INVALIDARG);
        assert_eq!(size, size_of::<u32>() as u32);
    }

    #[test]
    fn writes_to_a_descriptor_with_only_the_size_initialized() {
        let mut slot = MaybeUninit::<DDSURFACEDESC>::uninit();
        let raw = slot.as_mut_ptr();
        // SAFETY: Writing only the `dwSize` header into the aligned stack `MaybeUninit`.
        unsafe { (&raw mut (*raw).dwSize).write(size_of::<DDSURFACEDESC>() as u32) };
        // SAFETY: `raw` spans a full writable `DDSURFACEDESC` with valid `dwSize`.
        let result = unsafe { output(raw) }.unwrap().write(DDSURFACEDESC {
            dwSize: size_of::<DDSURFACEDESC>() as u32,
            dwWidth: 640,
            ..Default::default()
        });
        assert_eq!(result.dwWidth, 640);
    }

    #[test]
    fn reads_only_flagged_v2_surface_and_indexed_format_fields() {
        let mut input = MaybeUninit::<DDSURFACEDESC2>::uninit();
        let ptr = input.as_mut_ptr();
        // SAFETY: Initializing `dwSize`, `dwFlags`, and each field selected by `dwFlags`.
        let desc = unsafe {
            (&raw mut (*ptr).dwSize).write(size_of::<DDSURFACEDESC2>() as u32);
            (&raw mut (*ptr).dwFlags).write((DDSD_WIDTH | DDSD_PIXELFORMAT) as u32);
            (&raw mut (*ptr).dwWidth).write(320);
            let pf = &raw mut (*ptr).Anonymous5.ddpfPixelFormat;
            (&raw mut (*pf).dwSize).write(size_of::<DDPIXELFORMAT>() as u32);
            (&raw mut (*pf).dwFlags).write(DDPF_PALETTEINDEXED8 as u32);
            read(ptr).unwrap()
        };
        assert_eq!(desc.dwWidth, 320);
        assert_eq!(desc.dwHeight, 0);
        assert!(desc.lpSurface.is_null());
        assert_eq!(rgb_layout(&desc.ddpfPixelFormat), (8, [0; 3]));
    }
}
