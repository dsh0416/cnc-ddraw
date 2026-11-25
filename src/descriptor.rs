//! Size-prefixed DirectDraw structures at the COM boundary.
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
    // Each member occupies an entire DWORD (including the pair of WORDs
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

// A caller supplying a different version may have allocated only that version's size.
// Read the common DWORD header before accessing or borrowing the complete structure.
unsafe fn check_size<T: Descriptor>(ptr: *const T) -> Result<()> {
    if ptr.is_null() || unsafe { ptr.cast::<u32>().read() } as usize != size_of::<T>() {
        return Err(E_INVALIDARG.into());
    }
    Ok(())
}

// Only the RGB variants are supported. Indexed colors have no RGB masks.
unsafe fn read_pixel_format(ptr: *const DDPIXELFORMAT) -> DDPIXELFORMAT {
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
    unsafe fn read_fields(ptr: *const Self) -> DDSURFACEDESC;
}

// Both SDK versions carry the same surface inputs, with some fields moved into
// different unions. Generate typed field accesses against each SDK definition.
macro_rules! surface_descriptor {
    ($ty:ty, $($count:ident).+, $rate:ident, $($key:ident).+, $($pf:ident).+) => {
        impl SurfaceDescriptor for $ty {
            unsafe fn read_fields(ptr: *const Self) -> DDSURFACEDESC {
                // C clients can leave inactive fields uninitialized. Never borrow or
                // copy the input structure as a whole, including its nested unions.
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
/// the allocation must span T, with flags and the fields they select initialized.
pub(crate) unsafe fn read<T: SurfaceDescriptor>(ptr: *const T) -> Result<DDSURFACEDESC> {
    unsafe { check_size(ptr)? };
    Ok(unsafe { T::read_fields(ptr) })
}

/// # Safety
/// A non-null pointer must have a readable, aligned size header. When the size
/// matches, the whole allocation must be aligned and exclusively writable for
/// the returned borrow. Only the header needs to be initialized.
pub(crate) unsafe fn output<'a, T: Descriptor>(ptr: *mut T) -> Result<&'a mut MaybeUninit<T>> {
    unsafe { check_size(ptr)? };
    Ok(unsafe { &mut *ptr.cast::<MaybeUninit<T>>() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_short_buffer_using_only_its_header() {
        let mut size = size_of::<u32>() as u32;
        let ptr = (&raw mut size).cast::<DDSURFACEDESC2>();
        assert_eq!(unsafe { read(ptr) }.err().unwrap().code(), E_INVALIDARG);
        assert_eq!(unsafe { output(ptr) }.err().unwrap().code(), E_INVALIDARG);
        assert_eq!(size, size_of::<u32>() as u32);
    }

    #[test]
    fn writes_to_a_descriptor_with_only_the_size_initialized() {
        let mut slot = MaybeUninit::<DDSURFACEDESC>::uninit();
        let raw = slot.as_mut_ptr();
        unsafe { (&raw mut (*raw).dwSize).write(size_of::<DDSURFACEDESC>() as u32) };
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
