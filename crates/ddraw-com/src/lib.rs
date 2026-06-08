//! Minimal DirectDraw COM ABI for the greenfield Rust DLL.

use ddraw_core::{DirectDrawState, DisplayMode};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

pub type HResult = i32;
pub type Dword = u32;
pub type Bool = i32;

pub const S_OK: HResult = 0;
pub const E_FAIL: HResult = 0x8000_4005u32 as i32;
pub const E_NOINTERFACE: HResult = 0x8000_4002u32 as i32;
pub const E_POINTER: HResult = 0x8000_4003u32 as i32;
pub const DDERR_INVALIDPARAMS: HResult = 0x8876_0064u32 as i32;
pub const DDERR_UNSUPPORTED: HResult = 0x8876_0032u32 as i32;

const DDSD_HEIGHT: Dword = 0x0000_0002;
const DDSD_WIDTH: Dword = 0x0000_0004;
const DDSD_PITCH: Dword = 0x0000_0008;
const DDSD_LPSURFACE: Dword = 0x0000_0800;
const DDSD_PIXELFORMAT: Dword = 0x0000_1000;
const DDPF_RGB: Dword = 0x0000_0040;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

pub const IID_IUNKNOWN: Guid = Guid::new(
    0x0000_0000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);
pub const IID_IDIRECTDRAW: Guid = Guid::new(
    0x6C14_DB80,
    0xA733,
    0x11CE,
    [0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60],
);
pub const IID_IDIRECTDRAW2: Guid = Guid::new(
    0xB3A6_F3E0,
    0x2B43,
    0x11CF,
    [0xA2, 0xDE, 0x00, 0xAA, 0x00, 0xB9, 0x33, 0x56],
);
pub const IID_IDIRECTDRAW4: Guid = Guid::new(
    0x9C59_509A,
    0x39BD,
    0x11D1,
    [0x8C, 0x4A, 0x00, 0xC0, 0x4F, 0xD9, 0x30, 0xC5],
);
pub const IID_IDIRECTDRAW7: Guid = Guid::new(
    0x15E6_5EC0,
    0x3B9C,
    0x11D2,
    [0xB9, 0x2F, 0x00, 0x60, 0x97, 0x97, 0xEA, 0x5B],
);
pub const IID_IDIRECTDRAWSURFACE7: Guid = Guid::new(
    0x0667_5A80,
    0x3B9B,
    0x11D2,
    [0xB9, 0x2F, 0x00, 0x60, 0x97, 0x97, 0xEA, 0x5B],
);
pub const IID_IDIRECTDRAWPALETTE: Guid = Guid::new(
    0x6C14_DB84,
    0xA733,
    0x11CE,
    [0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60],
);
pub const IID_IDIRECTDRAWCLIPPER: Guid = Guid::new(
    0x6C14_DB85,
    0xA733,
    0x11CE,
    [0xA5, 0x21, 0x00, 0x20, 0xAF, 0x0B, 0xE5, 0x60],
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectDrawVersion {
    V1,
    V7,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ColorKey {
    pub low: Dword,
    pub high: Dword,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PixelFormat {
    pub size: Dword,
    pub flags: Dword,
    pub four_cc: Dword,
    pub rgb_bit_count: Dword,
    pub r_bit_mask: Dword,
    pub g_bit_mask: Dword,
    pub b_bit_mask: Dword,
    pub rgb_alpha_bit_mask: Dword,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PaletteEntry {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Caps2 {
    pub caps: Dword,
    pub caps2: Dword,
    pub caps3: Dword,
    pub caps4: Dword,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SurfaceDesc2 {
    pub size: Dword,
    pub flags: Dword,
    pub height: Dword,
    pub width: Dword,
    pub pitch_or_linear_size: i32,
    pub backbuffer_count_or_depth: Dword,
    pub mipmap_count_or_refresh_rate: Dword,
    pub alpha_bit_depth: Dword,
    pub reserved: Dword,
    pub surface: *mut c_void,
    pub color_key_dest_overlay: ColorKey,
    pub color_key_dest_blt: ColorKey,
    pub color_key_src_overlay: ColorKey,
    pub color_key_src_blt: ColorKey,
    pub pixel_format: PixelFormat,
    pub caps: Caps2,
    pub texture_stage: Dword,
}

#[repr(C)]
pub struct DirectDrawObject {
    vtbl: *const c_void,
    refs: AtomicU32,
    state: Mutex<DirectDrawState>,
}

#[repr(C)]
pub struct SurfaceObject {
    vtbl: *const SurfaceVtbl,
    refs: AtomicU32,
    state: Mutex<DirectDrawState>,
    palette: Mutex<Option<usize>>,
}

#[repr(C)]
pub struct PaletteObject {
    vtbl: *const PaletteVtbl,
    refs: AtomicU32,
    caps: Dword,
    entries: Mutex<[PaletteEntry; 256]>,
}

#[repr(C)]
pub struct ClipperObject {
    vtbl: *const ClipperVtbl,
    refs: AtomicU32,
}

#[repr(C)]
pub struct DirectDrawVtblV1 {
    query_interface:
        unsafe extern "system" fn(*mut DirectDrawObject, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: unsafe extern "system" fn(*mut DirectDrawObject) -> u32,
    release: unsafe extern "system" fn(*mut DirectDrawObject) -> u32,
    compact: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    create_clipper: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut *mut c_void,
        *mut c_void,
    ) -> HResult,
    create_palette: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut c_void,
        *mut *mut c_void,
        *mut c_void,
    ) -> HResult,
    create_surface: unsafe extern "system" fn(
        *mut DirectDrawObject,
        *mut SurfaceDesc2,
        *mut *mut c_void,
        *mut c_void,
    ) -> HResult,
    duplicate_surface:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut c_void, *mut *mut c_void) -> HResult,
    enum_display_modes: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut SurfaceDesc2,
        *mut c_void,
        *mut c_void,
    ) -> HResult,
    enum_surfaces: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut SurfaceDesc2,
        *mut c_void,
        *mut c_void,
    ) -> HResult,
    flip_to_gdi_surface: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    get_caps: unsafe extern "system" fn(*mut DirectDrawObject, *mut c_void, *mut c_void) -> HResult,
    get_display_mode:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut SurfaceDesc2) -> HResult,
    get_four_cc_codes:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut Dword, *mut Dword) -> HResult,
    get_gdi_surface: unsafe extern "system" fn(*mut DirectDrawObject, *mut *mut c_void) -> HResult,
    get_monitor_frequency: unsafe extern "system" fn(*mut DirectDrawObject, *mut Dword) -> HResult,
    get_scan_line: unsafe extern "system" fn(*mut DirectDrawObject, *mut Dword) -> HResult,
    get_vertical_blank_status:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut Bool) -> HResult,
    initialize: unsafe extern "system" fn(*mut DirectDrawObject, *mut Guid) -> HResult,
    restore_display_mode: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    set_cooperative_level:
        unsafe extern "system" fn(*mut DirectDrawObject, isize, Dword) -> HResult,
    set_display_mode:
        unsafe extern "system" fn(*mut DirectDrawObject, Dword, Dword, Dword) -> HResult,
    wait_for_vertical_blank:
        unsafe extern "system" fn(*mut DirectDrawObject, Dword, isize) -> HResult,
}

#[repr(C)]
pub struct DirectDrawVtblX {
    query_interface:
        unsafe extern "system" fn(*mut DirectDrawObject, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: unsafe extern "system" fn(*mut DirectDrawObject) -> u32,
    release: unsafe extern "system" fn(*mut DirectDrawObject) -> u32,
    compact: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    create_clipper: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut *mut c_void,
        *mut c_void,
    ) -> HResult,
    create_palette: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut c_void,
        *mut *mut c_void,
        *mut c_void,
    ) -> HResult,
    create_surface: unsafe extern "system" fn(
        *mut DirectDrawObject,
        *mut SurfaceDesc2,
        *mut *mut c_void,
        *mut c_void,
    ) -> HResult,
    duplicate_surface:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut c_void, *mut *mut c_void) -> HResult,
    enum_display_modes: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut SurfaceDesc2,
        *mut c_void,
        *mut c_void,
    ) -> HResult,
    enum_surfaces: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        *mut SurfaceDesc2,
        *mut c_void,
        *mut c_void,
    ) -> HResult,
    flip_to_gdi_surface: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    get_caps: unsafe extern "system" fn(*mut DirectDrawObject, *mut c_void, *mut c_void) -> HResult,
    get_display_mode:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut SurfaceDesc2) -> HResult,
    get_four_cc_codes:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut Dword, *mut Dword) -> HResult,
    get_gdi_surface: unsafe extern "system" fn(*mut DirectDrawObject, *mut *mut c_void) -> HResult,
    get_monitor_frequency: unsafe extern "system" fn(*mut DirectDrawObject, *mut Dword) -> HResult,
    get_scan_line: unsafe extern "system" fn(*mut DirectDrawObject, *mut Dword) -> HResult,
    get_vertical_blank_status:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut Bool) -> HResult,
    initialize: unsafe extern "system" fn(*mut DirectDrawObject, *mut Guid) -> HResult,
    restore_display_mode: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    set_cooperative_level:
        unsafe extern "system" fn(*mut DirectDrawObject, isize, Dword) -> HResult,
    set_display_mode: unsafe extern "system" fn(
        *mut DirectDrawObject,
        Dword,
        Dword,
        Dword,
        Dword,
        Dword,
    ) -> HResult,
    wait_for_vertical_blank:
        unsafe extern "system" fn(*mut DirectDrawObject, Dword, isize) -> HResult,
    get_available_vid_mem: unsafe extern "system" fn(
        *mut DirectDrawObject,
        *mut c_void,
        *mut Dword,
        *mut Dword,
    ) -> HResult,
    get_surface_from_dc:
        unsafe extern "system" fn(*mut DirectDrawObject, isize, *mut *mut c_void) -> HResult,
    restore_all_surfaces: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    test_cooperative_level: unsafe extern "system" fn(*mut DirectDrawObject) -> HResult,
    get_device_identifier:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut c_void, Dword) -> HResult,
    start_mode_test:
        unsafe extern "system" fn(*mut DirectDrawObject, *mut c_void, Dword, Dword) -> HResult,
    evaluate_mode: unsafe extern "system" fn(*mut DirectDrawObject, Dword, *mut Dword) -> HResult,
}

#[repr(C)]
pub struct SurfaceVtbl {
    query_interface:
        unsafe extern "system" fn(*mut SurfaceObject, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: unsafe extern "system" fn(*mut SurfaceObject) -> u32,
    release: unsafe extern "system" fn(*mut SurfaceObject) -> u32,
    add_attached_surface: unsafe extern "system" fn(*mut SurfaceObject, *mut c_void) -> HResult,
    add_overlay_dirty_rect: unsafe extern "system" fn(*mut SurfaceObject, *mut Rect) -> HResult,
    blt: unsafe extern "system" fn(
        *mut SurfaceObject,
        *mut Rect,
        *mut SurfaceObject,
        *mut Rect,
        Dword,
        *mut c_void,
    ) -> HResult,
    blt_batch: unsafe extern "system" fn(*mut SurfaceObject, *mut c_void, Dword, Dword) -> HResult,
    blt_fast: unsafe extern "system" fn(
        *mut SurfaceObject,
        Dword,
        Dword,
        *mut SurfaceObject,
        *mut Rect,
        Dword,
    ) -> HResult,
    delete_attached_surface:
        unsafe extern "system" fn(*mut SurfaceObject, Dword, *mut c_void) -> HResult,
    enum_attached_surfaces:
        unsafe extern "system" fn(*mut SurfaceObject, *mut c_void, *mut c_void) -> HResult,
    enum_overlay_z_orders:
        unsafe extern "system" fn(*mut SurfaceObject, Dword, *mut c_void, *mut c_void) -> HResult,
    flip: unsafe extern "system" fn(*mut SurfaceObject, *mut SurfaceObject, Dword) -> HResult,
    get_attached_surface:
        unsafe extern "system" fn(*mut SurfaceObject, *mut Caps2, *mut *mut c_void) -> HResult,
    get_blt_status: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    get_caps: unsafe extern "system" fn(*mut SurfaceObject, *mut Caps2) -> HResult,
    get_clipper: unsafe extern "system" fn(*mut SurfaceObject, *mut *mut c_void) -> HResult,
    get_color_key: unsafe extern "system" fn(*mut SurfaceObject, Dword, *mut ColorKey) -> HResult,
    get_dc: unsafe extern "system" fn(*mut SurfaceObject, *mut isize) -> HResult,
    get_flip_status: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    get_overlay_position:
        unsafe extern "system" fn(*mut SurfaceObject, *mut i32, *mut i32) -> HResult,
    get_palette: unsafe extern "system" fn(*mut SurfaceObject, *mut *mut c_void) -> HResult,
    get_pixel_format: unsafe extern "system" fn(*mut SurfaceObject, *mut PixelFormat) -> HResult,
    get_surface_desc: unsafe extern "system" fn(*mut SurfaceObject, *mut SurfaceDesc2) -> HResult,
    initialize:
        unsafe extern "system" fn(*mut SurfaceObject, *mut c_void, *mut SurfaceDesc2) -> HResult,
    is_lost: unsafe extern "system" fn(*mut SurfaceObject) -> HResult,
    lock: unsafe extern "system" fn(
        *mut SurfaceObject,
        *mut Rect,
        *mut SurfaceDesc2,
        Dword,
        isize,
    ) -> HResult,
    release_dc: unsafe extern "system" fn(*mut SurfaceObject, isize) -> HResult,
    restore: unsafe extern "system" fn(*mut SurfaceObject) -> HResult,
    set_clipper: unsafe extern "system" fn(*mut SurfaceObject, *mut c_void) -> HResult,
    set_color_key: unsafe extern "system" fn(*mut SurfaceObject, Dword, *mut ColorKey) -> HResult,
    set_overlay_position: unsafe extern "system" fn(*mut SurfaceObject, i32, i32) -> HResult,
    set_palette: unsafe extern "system" fn(*mut SurfaceObject, *mut c_void) -> HResult,
    unlock: unsafe extern "system" fn(*mut SurfaceObject, *mut c_void) -> HResult,
    update_overlay: unsafe extern "system" fn(
        *mut SurfaceObject,
        *mut Rect,
        *mut c_void,
        *mut Rect,
        Dword,
        *mut c_void,
    ) -> HResult,
    update_overlay_display: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    update_overlay_z_order:
        unsafe extern "system" fn(*mut SurfaceObject, Dword, *mut c_void) -> HResult,
    get_dd_interface: unsafe extern "system" fn(*mut SurfaceObject, *mut *mut c_void) -> HResult,
    page_lock: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    page_unlock: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    set_surface_desc:
        unsafe extern "system" fn(*mut SurfaceObject, *mut SurfaceDesc2, Dword) -> HResult,
    set_private_data: unsafe extern "system" fn(
        *mut SurfaceObject,
        *const Guid,
        *mut c_void,
        Dword,
        Dword,
    ) -> HResult,
    get_private_data: unsafe extern "system" fn(
        *mut SurfaceObject,
        *const Guid,
        *mut c_void,
        *mut Dword,
    ) -> HResult,
    free_private_data: unsafe extern "system" fn(*mut SurfaceObject, *const Guid) -> HResult,
    get_uniqueness_value: unsafe extern "system" fn(*mut SurfaceObject, *mut Dword) -> HResult,
    change_uniqueness_value: unsafe extern "system" fn(*mut SurfaceObject) -> HResult,
    set_priority: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    get_priority: unsafe extern "system" fn(*mut SurfaceObject, *mut Dword) -> HResult,
    set_lod: unsafe extern "system" fn(*mut SurfaceObject, Dword) -> HResult,
    get_lod: unsafe extern "system" fn(*mut SurfaceObject, *mut Dword) -> HResult,
}

#[repr(C)]
pub struct ClipperVtbl {
    query_interface:
        unsafe extern "system" fn(*mut ClipperObject, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: unsafe extern "system" fn(*mut ClipperObject) -> u32,
    release: unsafe extern "system" fn(*mut ClipperObject) -> u32,
    get_clip_list: unsafe extern "system" fn(
        *mut ClipperObject,
        *mut Rect,
        *mut c_void,
        *mut Dword,
    ) -> HResult,
    get_hwnd: unsafe extern "system" fn(*mut ClipperObject, *mut isize) -> HResult,
    initialize: unsafe extern "system" fn(*mut ClipperObject, *mut c_void, Dword) -> HResult,
    is_clip_list_changed: unsafe extern "system" fn(*mut ClipperObject, *mut Bool) -> HResult,
    set_clip_list: unsafe extern "system" fn(*mut ClipperObject, *mut c_void, Dword) -> HResult,
    set_hwnd: unsafe extern "system" fn(*mut ClipperObject, Dword, isize) -> HResult,
}

#[repr(C)]
pub struct PaletteVtbl {
    query_interface:
        unsafe extern "system" fn(*mut PaletteObject, *const Guid, *mut *mut c_void) -> HResult,
    add_ref: unsafe extern "system" fn(*mut PaletteObject) -> u32,
    release: unsafe extern "system" fn(*mut PaletteObject) -> u32,
    get_caps: unsafe extern "system" fn(*mut PaletteObject, *mut Dword) -> HResult,
    get_entries: unsafe extern "system" fn(
        *mut PaletteObject,
        Dword,
        Dword,
        Dword,
        *mut PaletteEntry,
    ) -> HResult,
    initialize: unsafe extern "system" fn(
        *mut PaletteObject,
        *mut c_void,
        Dword,
        *mut PaletteEntry,
    ) -> HResult,
    set_entries: unsafe extern "system" fn(
        *mut PaletteObject,
        Dword,
        Dword,
        Dword,
        *const PaletteEntry,
    ) -> HResult,
}

pub unsafe fn create_direct_draw(version: DirectDrawVersion, out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }

    let vtbl = match version {
        DirectDrawVersion::V1 => &DDRAW_VTBL_V1 as *const _ as *const c_void,
        DirectDrawVersion::V7 => &DDRAW_VTBL_X as *const _ as *const c_void,
    };
    let object = Box::new(DirectDrawObject {
        vtbl,
        refs: AtomicU32::new(1),
        state: Mutex::new(DirectDrawState::default()),
    });

    unsafe {
        *out = Box::into_raw(object) as *mut c_void;
    }

    S_OK
}

pub unsafe fn create_clipper(out: *mut *mut c_void) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }

    let object = Box::new(ClipperObject {
        vtbl: &CLIPPER_VTBL,
        refs: AtomicU32::new(1),
    });

    unsafe {
        *out = Box::into_raw(object) as *mut c_void;
    }

    S_OK
}

unsafe fn create_palette_object(
    caps: Dword,
    entries: *const PaletteEntry,
    out: *mut *mut c_void,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }

    let mut palette_entries = [PaletteEntry::default(); 256];
    if !entries.is_null() {
        unsafe {
            ptr::copy_nonoverlapping(entries, palette_entries.as_mut_ptr(), 256);
        }
    }

    let object = Box::new(PaletteObject {
        vtbl: &PALETTE_VTBL,
        refs: AtomicU32::new(1),
        caps,
        entries: Mutex::new(palette_entries),
    });

    unsafe {
        *out = Box::into_raw(object) as *mut c_void;
    }

    S_OK
}

fn surface_from_desc(desc: *const SurfaceDesc2) -> DirectDrawState {
    let mut mode = DisplayMode::default();

    if !desc.is_null() {
        let desc = unsafe { &*desc };
        if desc.width > 0 {
            mode.width = desc.width;
        }
        if desc.height > 0 {
            mode.height = desc.height;
        }
        if desc.pixel_format.rgb_bit_count > 0 {
            mode.bpp = desc.pixel_format.rgb_bit_count;
        }
    }

    DirectDrawState::new(mode)
}

fn fill_surface_desc(desc: *mut SurfaceDesc2, state: &mut DirectDrawState) -> HResult {
    if desc.is_null() {
        return E_POINTER;
    }

    let mode = state.mode();
    unsafe {
        let desc = &mut *desc;
        desc.size = std::mem::size_of::<SurfaceDesc2>() as Dword;
        desc.flags = DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_LPSURFACE | DDSD_PIXELFORMAT;
        desc.width = mode.width;
        desc.height = mode.height;
        desc.pitch_or_linear_size = state.pitch() as i32;
        desc.surface = state.primary_ptr().cast();
        desc.pixel_format = pixel_format_for_bpp(mode.bpp);
    }

    S_OK
}

fn pixel_format_for_bpp(bpp: Dword) -> PixelFormat {
    match bpp {
        8 => PixelFormat {
            size: std::mem::size_of::<PixelFormat>() as Dword,
            flags: DDPF_RGB,
            rgb_bit_count: 8,
            ..PixelFormat::default()
        },
        16 => PixelFormat {
            size: std::mem::size_of::<PixelFormat>() as Dword,
            flags: DDPF_RGB,
            rgb_bit_count: 16,
            r_bit_mask: 0xF800,
            g_bit_mask: 0x07E0,
            b_bit_mask: 0x001F,
            ..PixelFormat::default()
        },
        _ => PixelFormat {
            size: std::mem::size_of::<PixelFormat>() as Dword,
            flags: DDPF_RGB,
            rgb_bit_count: 32,
            r_bit_mask: 0x00FF_0000,
            g_bit_mask: 0x0000_FF00,
            b_bit_mask: 0x0000_00FF,
            rgb_alpha_bit_mask: 0xFF00_0000,
            ..PixelFormat::default()
        },
    }
}

unsafe extern "system" fn dd_query_interface(
    this: *mut DirectDrawObject,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    if this.is_null() || riid.is_null() || out.is_null() {
        return E_POINTER;
    }

    let riid = unsafe { *riid };
    if matches!(
        riid,
        IID_IUNKNOWN | IID_IDIRECTDRAW | IID_IDIRECTDRAW2 | IID_IDIRECTDRAW4 | IID_IDIRECTDRAW7
    ) {
        unsafe {
            dd_add_ref(this);
            *out = this.cast();
        }
        S_OK
    } else {
        unsafe {
            *out = ptr::null_mut();
        }
        E_NOINTERFACE
    }
}

unsafe extern "system" fn dd_add_ref(this: *mut DirectDrawObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn dd_release(this: *mut DirectDrawObject) -> u32 {
    if this.is_null() {
        return 0;
    }

    let remaining = unsafe { (*this).refs.fetch_sub(1, Ordering::Release) - 1 };
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this));
        }
    }
    remaining
}

unsafe extern "system" fn dd_create_surface(
    _this: *mut DirectDrawObject,
    desc: *mut SurfaceDesc2,
    out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }

    let object = Box::new(SurfaceObject {
        vtbl: &SURFACE_VTBL,
        refs: AtomicU32::new(1),
        state: Mutex::new(surface_from_desc(desc)),
        palette: Mutex::new(None),
    });

    unsafe {
        *out = Box::into_raw(object) as *mut c_void;
    }

    S_OK
}

unsafe extern "system" fn dd_get_display_mode(
    this: *mut DirectDrawObject,
    desc: *mut SurfaceDesc2,
) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    let mut state = unsafe { (*this).state.lock().expect("directdraw state poisoned") };
    fill_surface_desc(desc, &mut state)
}

unsafe extern "system" fn dd_set_cooperative_level(
    this: *mut DirectDrawObject,
    _hwnd: isize,
    flags: Dword,
) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    unsafe {
        (*this)
            .state
            .lock()
            .expect("directdraw state poisoned")
            .set_cooperative_level(flags);
    }
    S_OK
}

unsafe extern "system" fn dd_set_display_mode_v1(
    this: *mut DirectDrawObject,
    width: Dword,
    height: Dword,
    bpp: Dword,
) -> HResult {
    dd_set_mode(this, width, height, bpp, 0)
}

unsafe extern "system" fn dd_set_display_mode_x(
    this: *mut DirectDrawObject,
    width: Dword,
    height: Dword,
    bpp: Dword,
    refresh_rate: Dword,
    _flags: Dword,
) -> HResult {
    dd_set_mode(this, width, height, bpp, refresh_rate)
}

fn dd_set_mode(
    this: *mut DirectDrawObject,
    width: Dword,
    height: Dword,
    bpp: Dword,
    refresh_rate: Dword,
) -> HResult {
    if this.is_null() || width == 0 || height == 0 {
        return DDERR_INVALIDPARAMS;
    }

    unsafe {
        (*this)
            .state
            .lock()
            .expect("directdraw state poisoned")
            .set_mode(DisplayMode {
                width,
                height,
                bpp: if bpp == 0 { 32 } else { bpp },
                refresh_rate,
            });
    }
    S_OK
}

unsafe extern "system" fn dd_get_monitor_frequency(
    _this: *mut DirectDrawObject,
    freq: *mut Dword,
) -> HResult {
    if freq.is_null() {
        return E_POINTER;
    }
    unsafe {
        *freq = 60;
    }
    S_OK
}

unsafe extern "system" fn dd_get_vertical_blank_status(
    _this: *mut DirectDrawObject,
    status: *mut Bool,
) -> HResult {
    if status.is_null() {
        return E_POINTER;
    }
    unsafe {
        *status = 0;
    }
    S_OK
}

unsafe extern "system" fn dd_get_available_vid_mem(
    _this: *mut DirectDrawObject,
    _caps: *mut c_void,
    total: *mut Dword,
    free: *mut Dword,
) -> HResult {
    unsafe {
        if !total.is_null() {
            *total = 256 * 1024 * 1024;
        }
        if !free.is_null() {
            *free = 256 * 1024 * 1024;
        }
    }
    S_OK
}

unsafe extern "system" fn dd_create_clipper(
    _this: *mut DirectDrawObject,
    _flags: Dword,
    out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    unsafe { create_clipper(out) }
}

unsafe extern "system" fn dd_create_palette(
    _this: *mut DirectDrawObject,
    flags: Dword,
    entries: *mut c_void,
    out: *mut *mut c_void,
    _outer: *mut c_void,
) -> HResult {
    unsafe { create_palette_object(flags, entries.cast(), out) }
}

unsafe extern "system" fn s_query_interface(
    this: *mut SurfaceObject,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    if this.is_null() || riid.is_null() || out.is_null() {
        return E_POINTER;
    }

    let riid = unsafe { *riid };
    if matches!(riid, IID_IUNKNOWN | IID_IDIRECTDRAWSURFACE7) {
        unsafe {
            s_add_ref(this);
            *out = this.cast();
        }
        S_OK
    } else {
        unsafe {
            *out = ptr::null_mut();
        }
        E_NOINTERFACE
    }
}

unsafe extern "system" fn s_add_ref(this: *mut SurfaceObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn s_release(this: *mut SurfaceObject) -> u32 {
    if this.is_null() {
        return 0;
    }

    let remaining = unsafe { (*this).refs.fetch_sub(1, Ordering::Release) - 1 };
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this));
        }
    }
    remaining
}

unsafe extern "system" fn s_lock(
    this: *mut SurfaceObject,
    _rect: *mut Rect,
    desc: *mut SurfaceDesc2,
    _flags: Dword,
    _event: isize,
) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    let mut state = unsafe { (*this).state.lock().expect("surface state poisoned") };
    fill_surface_desc(desc, &mut state)
}

unsafe extern "system" fn s_get_surface_desc(
    this: *mut SurfaceObject,
    desc: *mut SurfaceDesc2,
) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    let mut state = unsafe { (*this).state.lock().expect("surface state poisoned") };
    fill_surface_desc(desc, &mut state)
}

unsafe extern "system" fn s_get_pixel_format(
    this: *mut SurfaceObject,
    format: *mut PixelFormat,
) -> HResult {
    if this.is_null() || format.is_null() {
        return E_POINTER;
    }

    let state = unsafe { (*this).state.lock().expect("surface state poisoned") };
    unsafe {
        *format = pixel_format_for_bpp(state.mode().bpp);
    }
    S_OK
}

unsafe extern "system" fn s_get_caps(_this: *mut SurfaceObject, caps: *mut Caps2) -> HResult {
    if caps.is_null() {
        return E_POINTER;
    }
    unsafe {
        *caps = Caps2::default();
    }
    S_OK
}

unsafe extern "system" fn s_get_palette(
    this: *mut SurfaceObject,
    out: *mut *mut c_void,
) -> HResult {
    if this.is_null() || out.is_null() {
        return E_POINTER;
    }

    let palette = unsafe { *(*this).palette.lock().expect("surface palette poisoned") };
    unsafe {
        *out = palette
            .map(|value| value as *mut c_void)
            .unwrap_or(ptr::null_mut());
    }

    if let Some(palette) = palette {
        unsafe {
            p_add_ref(palette as *mut PaletteObject);
        }
    }

    S_OK
}

unsafe extern "system" fn s_set_palette(this: *mut SurfaceObject, palette: *mut c_void) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    unsafe {
        let mut current = (*this).palette.lock().expect("surface palette poisoned");
        if let Some(old) = *current {
            p_release(old as *mut PaletteObject);
        }
        if !palette.is_null() {
            p_add_ref(palette.cast());
            *current = Some(palette as usize);
        } else {
            *current = None;
        }
    }

    S_OK
}

unsafe extern "system" fn s_blt(
    this: *mut SurfaceObject,
    _dst: *mut Rect,
    src: *mut SurfaceObject,
    _src_rect: *mut Rect,
    _flags: Dword,
    _fx: *mut c_void,
) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    if !src.is_null() {
        unsafe {
            let mut dst = (*this).state.lock().expect("surface state poisoned");
            let src = (*src).state.lock().expect("source surface state poisoned");
            let len = dst.primary_mut().len().min(src.primary().len());
            dst.primary_mut()[..len].copy_from_slice(&src.primary()[..len]);
        }
    }

    S_OK
}

unsafe extern "system" fn p_query_interface(
    this: *mut PaletteObject,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    if this.is_null() || riid.is_null() || out.is_null() {
        return E_POINTER;
    }

    let riid = unsafe { *riid };
    if matches!(riid, IID_IUNKNOWN | IID_IDIRECTDRAWPALETTE) {
        unsafe {
            p_add_ref(this);
            *out = this.cast();
        }
        S_OK
    } else {
        unsafe {
            *out = ptr::null_mut();
        }
        E_NOINTERFACE
    }
}

unsafe extern "system" fn p_add_ref(this: *mut PaletteObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn p_release(this: *mut PaletteObject) -> u32 {
    if this.is_null() {
        return 0;
    }

    let remaining = unsafe { (*this).refs.fetch_sub(1, Ordering::Release) - 1 };
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this));
        }
    }
    remaining
}

unsafe extern "system" fn p_get_caps(this: *mut PaletteObject, caps: *mut Dword) -> HResult {
    if this.is_null() || caps.is_null() {
        return E_POINTER;
    }

    unsafe {
        *caps = (*this).caps;
    }
    S_OK
}

unsafe extern "system" fn p_get_entries(
    this: *mut PaletteObject,
    _flags: Dword,
    base: Dword,
    count: Dword,
    entries: *mut PaletteEntry,
) -> HResult {
    if this.is_null() || entries.is_null() || base as usize + count as usize > 256 {
        return DDERR_INVALIDPARAMS;
    }

    let palette = unsafe { (*this).entries.lock().expect("palette entries poisoned") };
    unsafe {
        ptr::copy_nonoverlapping(palette.as_ptr().add(base as usize), entries, count as usize);
    }

    S_OK
}

unsafe extern "system" fn p_initialize(
    this: *mut PaletteObject,
    _ddraw: *mut c_void,
    caps: Dword,
    entries: *mut PaletteEntry,
) -> HResult {
    if this.is_null() {
        return E_POINTER;
    }

    unsafe {
        (*this).caps = caps;
        if !entries.is_null() {
            let mut palette = (*this).entries.lock().expect("palette entries poisoned");
            ptr::copy_nonoverlapping(entries, palette.as_mut_ptr(), 256);
        }
    }

    S_OK
}

unsafe extern "system" fn p_set_entries(
    this: *mut PaletteObject,
    _flags: Dword,
    base: Dword,
    count: Dword,
    entries: *const PaletteEntry,
) -> HResult {
    if this.is_null() || entries.is_null() || base as usize + count as usize > 256 {
        return DDERR_INVALIDPARAMS;
    }

    let mut palette = unsafe { (*this).entries.lock().expect("palette entries poisoned") };
    unsafe {
        ptr::copy_nonoverlapping(
            entries,
            palette.as_mut_ptr().add(base as usize),
            count as usize,
        );
    }

    S_OK
}

unsafe extern "system" fn c_query_interface(
    this: *mut ClipperObject,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    if this.is_null() || riid.is_null() || out.is_null() {
        return E_POINTER;
    }

    let riid = unsafe { *riid };
    if matches!(riid, IID_IUNKNOWN | IID_IDIRECTDRAWCLIPPER) {
        unsafe {
            c_add_ref(this);
            *out = this.cast();
        }
        S_OK
    } else {
        unsafe {
            *out = ptr::null_mut();
        }
        E_NOINTERFACE
    }
}

unsafe extern "system" fn c_add_ref(this: *mut ClipperObject) -> u32 {
    if this.is_null() {
        return 0;
    }
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn c_release(this: *mut ClipperObject) -> u32 {
    if this.is_null() {
        return 0;
    }

    let remaining = unsafe { (*this).refs.fetch_sub(1, Ordering::Release) - 1 };
    if remaining == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(this));
        }
    }
    remaining
}

unsafe extern "system" fn ok_dd(_this: *mut DirectDrawObject) -> HResult {
    S_OK
}

unsafe extern "system" fn ok_surface(_this: *mut SurfaceObject) -> HResult {
    S_OK
}

macro_rules! dd_stub {
    ($name:ident ( $($arg:ident : $ty:ty),* )) => {
        unsafe extern "system" fn $name(_this: *mut DirectDrawObject, $($arg: $ty),*) -> HResult {
            let _ = ($($arg),*);
            S_OK
        }
    };
}

macro_rules! surface_stub {
    ($name:ident ( $($arg:ident : $ty:ty),* )) => {
        unsafe extern "system" fn $name(_this: *mut SurfaceObject, $($arg: $ty),*) -> HResult {
            let _ = ($($arg),*);
            S_OK
        }
    };
}

macro_rules! clipper_stub {
    ($name:ident ( $($arg:ident : $ty:ty),* )) => {
        unsafe extern "system" fn $name(_this: *mut ClipperObject, $($arg: $ty),*) -> HResult {
            let _ = ($($arg),*);
            S_OK
        }
    };
}

dd_stub!(dd_duplicate_surface(surface: *mut c_void, out: *mut *mut c_void));
dd_stub!(dd_enum_display_modes(flags: Dword, desc: *mut SurfaceDesc2, context: *mut c_void, callback: *mut c_void));
dd_stub!(dd_enum_surfaces(flags: Dword, desc: *mut SurfaceDesc2, context: *mut c_void, callback: *mut c_void));
dd_stub!(dd_get_caps(driver: *mut c_void, emu: *mut c_void));
dd_stub!(dd_get_four_cc_codes(count: *mut Dword, codes: *mut Dword));
dd_stub!(dd_get_gdi_surface(out: *mut *mut c_void));
dd_stub!(dd_get_scan_line(scanline: *mut Dword));
dd_stub!(dd_initialize(guid: *mut Guid));
dd_stub!(dd_wait_for_vertical_blank(flags: Dword, event: isize));
dd_stub!(dd_get_surface_from_dc(hdc: isize, out: *mut *mut c_void));
dd_stub!(dd_get_device_identifier(identifier: *mut c_void, flags: Dword));
dd_stub!(dd_start_mode_test(sizes: *mut c_void, count: Dword, flags: Dword));
dd_stub!(dd_evaluate_mode(flags: Dword, timeout: *mut Dword));

surface_stub!(s_add_attached_surface(surface: *mut c_void));
surface_stub!(s_add_overlay_dirty_rect(rect: *mut Rect));
surface_stub!(s_blt_batch(batch: *mut c_void, count: Dword, flags: Dword));
surface_stub!(s_blt_fast(x: Dword, y: Dword, src: *mut SurfaceObject, rect: *mut Rect, flags: Dword));
surface_stub!(s_delete_attached_surface(flags: Dword, surface: *mut c_void));
surface_stub!(s_enum_attached_surfaces(context: *mut c_void, callback: *mut c_void));
surface_stub!(s_enum_overlay_z_orders(flags: Dword, context: *mut c_void, callback: *mut c_void));
surface_stub!(s_flip(target: *mut SurfaceObject, flags: Dword));
surface_stub!(s_get_attached_surface(caps: *mut Caps2, out: *mut *mut c_void));
surface_stub!(s_get_blt_status(flags: Dword));
surface_stub!(s_get_clipper(out: *mut *mut c_void));
surface_stub!(s_get_color_key(flags: Dword, key: *mut ColorKey));
surface_stub!(s_get_dc(dc: *mut isize));
surface_stub!(s_get_flip_status(flags: Dword));
surface_stub!(s_get_overlay_position(x: *mut i32, y: *mut i32));
surface_stub!(s_initialize(ddraw: *mut c_void, desc: *mut SurfaceDesc2));
surface_stub!(s_release_dc(dc: isize));
surface_stub!(s_set_clipper(clipper: *mut c_void));
surface_stub!(s_set_color_key(flags: Dword, key: *mut ColorKey));
surface_stub!(s_set_overlay_position(x: i32, y: i32));
surface_stub!(s_unlock(rect: *mut c_void));
surface_stub!(s_update_overlay(src: *mut Rect, dest_surface: *mut c_void, dest: *mut Rect, flags: Dword, fx: *mut c_void));
surface_stub!(s_update_overlay_display(flags: Dword));
surface_stub!(s_update_overlay_z_order(flags: Dword, surface: *mut c_void));
surface_stub!(s_get_dd_interface(out: *mut *mut c_void));
surface_stub!(s_page_lock(flags: Dword));
surface_stub!(s_page_unlock(flags: Dword));
surface_stub!(s_set_surface_desc(desc: *mut SurfaceDesc2, flags: Dword));
surface_stub!(s_set_private_data(guid: *const Guid, data: *mut c_void, size: Dword, flags: Dword));
surface_stub!(s_get_private_data(guid: *const Guid, data: *mut c_void, size: *mut Dword));
surface_stub!(s_free_private_data(guid: *const Guid));
surface_stub!(s_get_uniqueness_value(value: *mut Dword));
surface_stub!(s_change_uniqueness_value());
surface_stub!(s_set_priority(priority: Dword));
surface_stub!(s_get_priority(priority: *mut Dword));
surface_stub!(s_set_lod(lod: Dword));
surface_stub!(s_get_lod(lod: *mut Dword));

clipper_stub!(c_get_clip_list(rect: *mut Rect, data: *mut c_void, size: *mut Dword));
clipper_stub!(c_get_hwnd(hwnd: *mut isize));
clipper_stub!(c_initialize(ddraw: *mut c_void, flags: Dword));
clipper_stub!(c_is_clip_list_changed(changed: *mut Bool));
clipper_stub!(c_set_clip_list(data: *mut c_void, flags: Dword));
clipper_stub!(c_set_hwnd(flags: Dword, hwnd: isize));

static DDRAW_VTBL_V1: DirectDrawVtblV1 = DirectDrawVtblV1 {
    query_interface: dd_query_interface,
    add_ref: dd_add_ref,
    release: dd_release,
    compact: ok_dd,
    create_clipper: dd_create_clipper,
    create_palette: dd_create_palette,
    create_surface: dd_create_surface,
    duplicate_surface: dd_duplicate_surface,
    enum_display_modes: dd_enum_display_modes,
    enum_surfaces: dd_enum_surfaces,
    flip_to_gdi_surface: ok_dd,
    get_caps: dd_get_caps,
    get_display_mode: dd_get_display_mode,
    get_four_cc_codes: dd_get_four_cc_codes,
    get_gdi_surface: dd_get_gdi_surface,
    get_monitor_frequency: dd_get_monitor_frequency,
    get_scan_line: dd_get_scan_line,
    get_vertical_blank_status: dd_get_vertical_blank_status,
    initialize: dd_initialize,
    restore_display_mode: ok_dd,
    set_cooperative_level: dd_set_cooperative_level,
    set_display_mode: dd_set_display_mode_v1,
    wait_for_vertical_blank: dd_wait_for_vertical_blank,
};

static DDRAW_VTBL_X: DirectDrawVtblX = DirectDrawVtblX {
    query_interface: dd_query_interface,
    add_ref: dd_add_ref,
    release: dd_release,
    compact: ok_dd,
    create_clipper: dd_create_clipper,
    create_palette: dd_create_palette,
    create_surface: dd_create_surface,
    duplicate_surface: dd_duplicate_surface,
    enum_display_modes: dd_enum_display_modes,
    enum_surfaces: dd_enum_surfaces,
    flip_to_gdi_surface: ok_dd,
    get_caps: dd_get_caps,
    get_display_mode: dd_get_display_mode,
    get_four_cc_codes: dd_get_four_cc_codes,
    get_gdi_surface: dd_get_gdi_surface,
    get_monitor_frequency: dd_get_monitor_frequency,
    get_scan_line: dd_get_scan_line,
    get_vertical_blank_status: dd_get_vertical_blank_status,
    initialize: dd_initialize,
    restore_display_mode: ok_dd,
    set_cooperative_level: dd_set_cooperative_level,
    set_display_mode: dd_set_display_mode_x,
    wait_for_vertical_blank: dd_wait_for_vertical_blank,
    get_available_vid_mem: dd_get_available_vid_mem,
    get_surface_from_dc: dd_get_surface_from_dc,
    restore_all_surfaces: ok_dd,
    test_cooperative_level: ok_dd,
    get_device_identifier: dd_get_device_identifier,
    start_mode_test: dd_start_mode_test,
    evaluate_mode: dd_evaluate_mode,
};

static SURFACE_VTBL: SurfaceVtbl = SurfaceVtbl {
    query_interface: s_query_interface,
    add_ref: s_add_ref,
    release: s_release,
    add_attached_surface: s_add_attached_surface,
    add_overlay_dirty_rect: s_add_overlay_dirty_rect,
    blt: s_blt,
    blt_batch: s_blt_batch,
    blt_fast: s_blt_fast,
    delete_attached_surface: s_delete_attached_surface,
    enum_attached_surfaces: s_enum_attached_surfaces,
    enum_overlay_z_orders: s_enum_overlay_z_orders,
    flip: s_flip,
    get_attached_surface: s_get_attached_surface,
    get_blt_status: s_get_blt_status,
    get_caps: s_get_caps,
    get_clipper: s_get_clipper,
    get_color_key: s_get_color_key,
    get_dc: s_get_dc,
    get_flip_status: s_get_flip_status,
    get_overlay_position: s_get_overlay_position,
    get_palette: s_get_palette,
    get_pixel_format: s_get_pixel_format,
    get_surface_desc: s_get_surface_desc,
    initialize: s_initialize,
    is_lost: ok_surface,
    lock: s_lock,
    release_dc: s_release_dc,
    restore: ok_surface,
    set_clipper: s_set_clipper,
    set_color_key: s_set_color_key,
    set_overlay_position: s_set_overlay_position,
    set_palette: s_set_palette,
    unlock: s_unlock,
    update_overlay: s_update_overlay,
    update_overlay_display: s_update_overlay_display,
    update_overlay_z_order: s_update_overlay_z_order,
    get_dd_interface: s_get_dd_interface,
    page_lock: s_page_lock,
    page_unlock: s_page_unlock,
    set_surface_desc: s_set_surface_desc,
    set_private_data: s_set_private_data,
    get_private_data: s_get_private_data,
    free_private_data: s_free_private_data,
    get_uniqueness_value: s_get_uniqueness_value,
    change_uniqueness_value: s_change_uniqueness_value,
    set_priority: s_set_priority,
    get_priority: s_get_priority,
    set_lod: s_set_lod,
    get_lod: s_get_lod,
};

static CLIPPER_VTBL: ClipperVtbl = ClipperVtbl {
    query_interface: c_query_interface,
    add_ref: c_add_ref,
    release: c_release,
    get_clip_list: c_get_clip_list,
    get_hwnd: c_get_hwnd,
    initialize: c_initialize,
    is_clip_list_changed: c_is_clip_list_changed,
    set_clip_list: c_set_clip_list,
    set_hwnd: c_set_hwnd,
};

static PALETTE_VTBL: PaletteVtbl = PaletteVtbl {
    query_interface: p_query_interface,
    add_ref: p_add_ref,
    release: p_release,
    get_caps: p_get_caps,
    get_entries: p_get_entries,
    initialize: p_initialize,
    set_entries: p_set_entries,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_directdraw_object() {
        let mut out = ptr::null_mut();
        let hr = unsafe { create_direct_draw(DirectDrawVersion::V7, &mut out) };

        assert_eq!(hr, S_OK);
        assert!(!out.is_null());
        assert_eq!(unsafe { dd_release(out.cast()) }, 0);
    }

    #[test]
    fn surface_desc_exposes_cpu_buffer() {
        let mut out = ptr::null_mut();
        let hr = unsafe { create_direct_draw(DirectDrawVersion::V7, &mut out) };
        assert_eq!(hr, S_OK);

        let mut surface = ptr::null_mut();
        let mut desc = SurfaceDesc2 {
            width: 320,
            height: 200,
            pixel_format: PixelFormat {
                rgb_bit_count: 8,
                ..PixelFormat::default()
            },
            ..SurfaceDesc2::default()
        };
        let hr = unsafe { dd_create_surface(out.cast(), &mut desc, &mut surface, ptr::null_mut()) };
        assert_eq!(hr, S_OK);

        let mut locked = SurfaceDesc2::default();
        let hr = unsafe { s_lock(surface.cast(), ptr::null_mut(), &mut locked, 0, 0) };
        assert_eq!(hr, S_OK);
        assert_eq!(locked.width, 320);
        assert_eq!(locked.height, 200);
        assert!(!locked.surface.is_null());

        assert_eq!(unsafe { s_release(surface.cast()) }, 0);
        assert_eq!(unsafe { dd_release(out.cast()) }, 0);
    }

    #[test]
    fn palette_entries_round_trip() {
        let mut palette = ptr::null_mut();
        let mut entries = [PaletteEntry::default(); 256];
        entries[7] = PaletteEntry {
            red: 10,
            green: 20,
            blue: 30,
            flags: 0,
        };

        let hr = unsafe { create_palette_object(0, entries.as_ptr(), &mut palette) };
        assert_eq!(hr, S_OK);

        let mut out = [PaletteEntry::default(); 1];
        let hr = unsafe { p_get_entries(palette.cast(), 0, 7, 1, out.as_mut_ptr()) };
        assert_eq!(hr, S_OK);
        assert_eq!(out[0].red, 10);
        assert_eq!(out[0].green, 20);
        assert_eq!(out[0].blue, 30);

        assert_eq!(unsafe { p_release(palette.cast()) }, 0);
    }
}
