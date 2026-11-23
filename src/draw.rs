use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::rc::{Rc, Weak};

use windows::core::{
    implement, s, w, Error, IUnknown, Interface, OutRef, Ref, Result, BOOL, GUID, HRESULT, PCWSTR,
};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, CLASS_E_NOAGGREGATION, ERROR_SUCCESS, E_FAIL, E_INVALIDARG,
    E_NOINTERFACE, E_NOTIMPL, E_POINTER, HANDLE, HWND, S_OK,
};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS,
    HMONITOR, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, PALETTEENTRY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, GetWindowPlacement, IsWindow, SetWindowLongW, SetWindowPlacement, SetWindowPos,
    GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    WINDOWPLACEMENT, WINDOW_LONG_PTR_INDEX, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_DLGMODALFRAME,
    WS_EX_STATICEDGE, WS_EX_WINDOWEDGE, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

use crate::{palette, renderer::Renderer, surface};

const DEFAULT_MODE: (u32, u32, u32) = (640, 480, 32);
const ALREADY_INITIALIZED: HRESULT = HRESULT(0x88760005_u32 as i32);
const SURFACE_BUSY: HRESULT = HRESULT(0x887601ae_u32 as i32);

pub struct Display {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    renderer: Option<Renderer>,
    rgba: Vec<u8>,
    pub primary: Weak<RefCell<surface::SurfaceData>>,
}

impl Display {
    pub fn refresh(&mut self) -> Result<()> {
        let (Some(renderer), Some(primary)) = (&mut self.renderer, self.primary.upgrade()) else {
            return Ok(());
        };
        let primary = primary.try_borrow().map_err(|_| SURFACE_BUSY)?;
        if primary.locked() {
            return Ok(());
        }
        surface::frame_rgba(&primary, &mut self.rgba)?;
        let (width, height) = (primary.width, primary.height);
        drop(primary);
        renderer
            .present(width, height, &self.rgba)
            .map_err(|message| Error::new(E_FAIL, message))
    }
}

#[derive(Clone, Copy)]
struct SavedWindow {
    hwnd: HWND,
    placement: WINDOWPLACEMENT,
    style: i32,
    ex_style: i32,
}

fn set_style(hwnd: HWND, index: WINDOW_LONG_PTR_INDEX, style: i32) -> Result<()> {
    unsafe {
        SetLastError(ERROR_SUCCESS);
        if SetWindowLongW(hwnd, index, style) == 0 {
            let error = GetLastError();
            if error != ERROR_SUCCESS {
                return Err(error.to_hresult().into());
            }
        }
    }
    Ok(())
}

impl SavedWindow {
    fn enter(hwnd: HWND) -> Result<Self> {
        let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
        if style as u32 & WS_CHILD.0 != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        unsafe { GetWindowPlacement(hwnd, &mut placement)? };
        let saved = Self {
            hwnd,
            placement,
            style,
            ex_style: unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) },
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).ok()?
        };
        let rect = info.rcMonitor;
        let edge_styles =
            WS_EX_CLIENTEDGE.0 | WS_EX_WINDOWEDGE.0 | WS_EX_DLGMODALFRAME.0 | WS_EX_STATICEDGE.0;
        let apply = || -> Result<()> {
            set_style(
                hwnd,
                GWL_STYLE,
                ((style as u32 & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0) as i32,
            )?;
            set_style(
                hwnd,
                GWL_EXSTYLE,
                (saved.ex_style as u32 & !edge_styles) as i32,
            )?;
            unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            }
        };
        if let Err(error) = apply() {
            let _ = saved.restore();
            return Err(error);
        }
        Ok(saved)
    }

    fn restore(self) -> Result<()> {
        if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            return Ok(());
        }
        set_style(self.hwnd, GWL_STYLE, self.style)?;
        set_style(self.hwnd, GWL_EXSTYLE, self.ex_style)?;
        unsafe {
            SetWindowPlacement(self.hwnd, &self.placement)?;
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            )
        }
    }
}

#[implement(IDirectDraw, Agile = false)]
struct Draw {
    display: Rc<RefCell<Display>>,
    hwnd: Cell<HWND>,
    fullscreen: Cell<Option<SavedWindow>>,
    changing_window: RefCell<()>,
}

impl Draw {
    fn new() -> Self {
        let (width, height, bpp) = DEFAULT_MODE;
        Self {
            display: Rc::new(RefCell::new(Display {
                width,
                height,
                bpp,
                renderer: None,
                rgba: Vec::new(),
                primary: Weak::new(),
            })),
            hwnd: Cell::new(HWND::default()),
            fullscreen: Cell::new(None),
            changing_window: RefCell::new(()),
        }
    }
}

impl Drop for Draw {
    fn drop(&mut self) {
        if let Some(saved) = self.fullscreen.take() {
            let _ = saved.restore();
        }
    }
}

fn mode_description(width: u32, height: u32, bpp: u32) -> Result<DDSURFACEDESC> {
    Ok(DDSURFACEDESC {
        dwSize: size_of::<DDSURFACEDESC>() as u32,
        dwFlags: (DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_PIXELFORMAT) as u32,
        dwWidth: width,
        dwHeight: height,
        Anonymous1: DDSURFACEDESC_0 {
            lPitch: ((width * (bpp / 8) + 3) & !3) as i32,
        },
        ddpfPixelFormat: surface::pixel_format(bpp)?,
        ..Default::default()
    })
}

fn check_desc(desc: *const DDSURFACEDESC) -> Result<()> {
    if desc.is_null() || unsafe { (*desc).dwSize } != size_of::<DDSURFACEDESC>() as u32 {
        return Err(E_INVALIDARG.into());
    }
    Ok(())
}

fn put_interface<T>(out: OutRef<T>, make: impl FnOnce() -> Result<T>) -> Result<()>
where
    T: Interface + windows_core::Type<T, Default = Option<T>>,
{
    if out.is_null() {
        return Err(E_POINTER.into());
    }
    match make() {
        Ok(value) => out.write(Some(value)),
        Err(error) => {
            out.write(None)?;
            Err(error)
        }
    }
}

fn no_aggregation(outer: Ref<IUnknown>) -> Result<()> {
    if outer.is_null() {
        Ok(())
    } else {
        Err(CLASS_E_NOAGGREGATION.into())
    }
}

#[allow(non_snake_case)]
impl IDirectDraw_Impl for Draw_Impl {
    fn Compact(&self) -> Result<()> {
        Ok(())
    }

    fn CreateClipper(
        &self,
        _: u32,
        out: OutRef<IDirectDrawClipper>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        put_interface(out, || {
            no_aggregation(outer)?;
            Err(E_NOTIMPL.into())
        })
    }
    fn CreatePalette(
        &self,
        flags: u32,
        entries: *mut PALETTEENTRY,
        out: OutRef<IDirectDrawPalette>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        put_interface(out, || {
            no_aggregation(outer)?;
            palette::create_palette(flags, entries, self.display.clone())
        })
    }
    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC,
        out: OutRef<IDirectDrawSurface>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        put_interface(out, || {
            no_aggregation(outer)?;
            check_desc(desc)?;
            surface::create_surface(self.display.clone(), unsafe { &*desc })
        })
    }
    fn DuplicateSurface(&self, _: Ref<IDirectDrawSurface>) -> Result<IDirectDrawSurface> {
        Err(E_NOTIMPL.into())
    }

    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK,
    ) -> Result<()> {
        if flags != 0 {
            return Err(E_NOTIMPL.into());
        }
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if filter.is_null() {
            DDSURFACEDESC::default()
        } else {
            check_desc(filter)?;
            unsafe { *filter }
        };
        if filter.dwFlags & !((DDSD_WIDTH | DDSD_HEIGHT | DDSD_PIXELFORMAT) as u32) != 0 {
            return Err(E_NOTIMPL.into());
        }
        let current = self.display.try_borrow().map_err(|_| SURFACE_BUSY)?;
        let current_size = (current.width, current.height);
        drop(current);
        let sizes = [
            (320, 200),
            (320, 240),
            (640, 400),
            (640, 480),
            (800, 600),
            (1024, 768),
            (1280, 720),
            (1920, 1080),
        ];
        let additional = (!sizes.contains(&current_size)).then_some(current_size);
        for (width, height) in sizes.into_iter().chain(additional) {
            if filter.dwFlags & DDSD_WIDTH as u32 != 0 && filter.dwWidth != width
                || filter.dwFlags & DDSD_HEIGHT as u32 != 0 && filter.dwHeight != height
            {
                continue;
            }
            for bpp in [8, 16, 32] {
                let mut mode = mode_description(width, height, bpp)?;
                if filter.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
                    let requested = &filter.ddpfPixelFormat;
                    let format = &mode.ddpfPixelFormat;
                    if requested.dwFlags != format.dwFlags
                        || unsafe {
                            requested.Anonymous1.dwRGBBitCount != bpp
                                || (
                                    requested.Anonymous2.dwRBitMask,
                                    requested.Anonymous3.dwGBitMask,
                                    requested.Anonymous4.dwBBitMask,
                                ) != (
                                    format.Anonymous2.dwRBitMask,
                                    format.Anonymous3.dwGBitMask,
                                    format.Anonymous4.dwBBitMask,
                                )
                        }
                    {
                        continue;
                    }
                }
                if unsafe { callback(&mut mode, context) }.0 == DDENUMRET_CANCEL as i32 {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn EnumSurfaces(
        &self,
        _: u32,
        _: *mut DDSURFACEDESC,
        _: *mut c_void,
        _: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn FlipToGDISurface(&self) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetCaps(&self, driver: *mut DDCAPS_DX7, software: *mut DDCAPS_DX7) -> Result<()> {
        if driver.is_null() && software.is_null() {
            return Err(E_INVALIDARG.into());
        }
        for output in [driver, software].into_iter().filter(|p| !p.is_null()) {
            let size = unsafe { (*output).dwSize } as usize;
            if ![
                size_of::<DDCAPS_DX1>(),
                size_of::<DDCAPS_DX3>(),
                size_of::<DDCAPS_DX5>(),
                size_of::<DDCAPS_DX6>(),
                size_of::<DDCAPS_DX7>(),
            ]
            .contains(&size)
            {
                return Err(E_INVALIDARG.into());
            }
        }
        for (output, emulation) in [(driver, false), (software, true)] {
            if output.is_null() {
                continue;
            }
            let size = unsafe { (*output).dwSize };
            let mut caps = DDCAPS_DX7 {
                dwSize: size,
                dwCaps: DDCAPS_NOHARDWARE as u32,
                ..Default::default()
            };
            if emulation {
                caps.dwCaps = (DDCAPS_BLT
                    | DDCAPS_BLTSTRETCH
                    | DDCAPS_BLTCOLORFILL
                    | DDCAPS_COLORKEY
                    | DDCAPS_CANBLTSYSMEM
                    | DDCAPS_PALETTE) as u32;
                caps.dwCKeyCaps = DDCKEYCAPS_SRCBLT as u32;
                caps.dwPalCaps = (DDPCAPS_8BIT | DDPCAPS_ALLOW256) as u32;
                caps.ddsOldCaps.dwCaps = (DDSCAPS_PRIMARYSURFACE
                    | DDSCAPS_OFFSCREENPLAIN
                    | DDSCAPS_SYSTEMMEMORY
                    | DDSCAPS_FLIP
                    | DDSCAPS_BACKBUFFER
                    | DDSCAPS_COMPLEX) as u32;
                caps.ddsCaps.dwCaps = caps.ddsOldCaps.dwCaps;
            }
            // Legacy DDCAPS layouts are prefixes of DX7 and callers allocate only dwSize.
            unsafe {
                ptr::copy_nonoverlapping(
                    (&raw const caps).cast::<u8>(),
                    output.cast::<u8>(),
                    size as usize,
                )
            };
        }
        Ok(())
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC) -> Result<()> {
        check_desc(desc)?;
        let d = self.display.try_borrow().map_err(|_| SURFACE_BUSY)?;
        unsafe { desc.write(mode_description(d.width, d.height, d.bpp)?) };
        Ok(())
    }
    fn GetFourCCCodes(&self, count: *mut u32, _: *mut u32) -> Result<()> {
        if count.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { count.write(0) };
        Ok(())
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface> {
        Err(E_NOTIMPL.into())
    }
    fn GetMonitorFrequency(&self, output: *mut u32) -> Result<()> {
        if output.is_null() {
            return Err(E_POINTER.into());
        }
        let mut mode = DEVMODEW {
            dmSize: size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let hwnd = self.hwnd.get();
        let mut monitor = MONITORINFOEXW::default();
        let device_name = if hwnd.is_invalid() {
            PCWSTR::null()
        } else {
            monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            unsafe {
                GetMonitorInfoW(
                    MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                    &mut monitor.monitorInfo,
                )
                .ok()?
            };
            PCWSTR(monitor.szDevice.as_ptr())
        };
        if !unsafe { EnumDisplaySettingsW(device_name, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool()
            || mode.dmDisplayFrequency <= 1
        {
            return Err(E_FAIL.into());
        }
        unsafe { output.write(mode.dmDisplayFrequency) };
        Ok(())
    }
    fn GetScanLine(&self, _: *mut u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetVerticalBlankStatus(&self, _: *mut BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Initialize(&self, _: *mut GUID) -> Result<()> {
        Err(ALREADY_INITIALIZED.into())
    }
    fn RestoreDisplayMode(&self) -> Result<()> {
        let _transition = self
            .changing_window
            .try_borrow_mut()
            .map_err(|_| SURFACE_BUSY)?;
        if let Some(saved) = self.fullscreen.get() {
            saved.restore()?;
            self.fullscreen.set(None);
        }
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        if display.primary.upgrade().is_none() {
            (display.width, display.height, display.bpp) = DEFAULT_MODE;
        }
        display.refresh()
    }

    fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
        let exclusive = flags == (DDSCL_FULLSCREEN | DDSCL_EXCLUSIVE) as u32;
        if !exclusive && flags != DDSCL_NORMAL as u32 {
            return Err(E_NOTIMPL.into());
        }
        if (hwnd.is_invalid() && exclusive)
            || (!hwnd.is_invalid() && !unsafe { IsWindow(Some(hwnd)) }.as_bool())
        {
            return Err(E_INVALIDARG.into());
        }
        // SetWindowPos synchronously calls the game's WndProc, which may re-enter us.
        let _transition = self
            .changing_window
            .try_borrow_mut()
            .map_err(|_| SURFACE_BUSY)?;
        let change_hwnd = self.hwnd.get() != hwnd;
        if !change_hwnd && self.fullscreen.get().is_some() == exclusive {
            return Ok(());
        }
        let replacement = if change_hwnd && !hwnd.is_invalid() {
            Some(Renderer::new(hwnd.0 as isize).map_err(|message| Error::new(E_FAIL, message))?)
        } else {
            None
        };
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        if let Some(saved) = self.fullscreen.get() {
            saved.restore()?;
            self.fullscreen.set(None);
        }
        if exclusive {
            self.fullscreen.set(Some(SavedWindow::enter(hwnd)?));
        }
        if change_hwnd {
            display.renderer = replacement;
            self.hwnd.set(hwnd);
        }
        display.refresh()
    }
    fn SetDisplayMode(&self, width: u32, height: u32, bpp: u32) -> Result<()> {
        if !(1..=8192).contains(&width)
            || !(1..=8192).contains(&height)
            || ![8, 16, 32].contains(&bpp)
        {
            return Err(E_INVALIDARG.into());
        }
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        if (display.width, display.height, display.bpp) != (width, height, bpp)
            && display.primary.upgrade().is_some()
        {
            return Err(SURFACE_BUSY.into());
        }
        (display.width, display.height, display.bpp) = (width, height, bpp);
        Ok(())
    }
    fn WaitForVerticalBlank(&self, _: u32, _: HANDLE) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawCreate(
    device: *const GUID,
    out: *mut *mut c_void,
    outer: *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe { out.write(ptr::null_mut()) };
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    if !device.is_null() {
        return E_INVALIDARG;
    }
    let device: IDirectDraw = Draw::new().into();
    unsafe { out.write(device.into_raw()) };
    S_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawCreateEx(
    _: *const GUID,
    out: *mut *mut c_void,
    iid: *const GUID,
    outer: *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe { out.write(ptr::null_mut()) };
    if iid.is_null() {
        return E_POINTER;
    }
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    // This entry point requires IDirectDraw7.
    E_NOINTERFACE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawCreateClipper(
    _: u32,
    out: *mut *mut c_void,
    outer: *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe { out.write(ptr::null_mut()) };
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    E_NOTIMPL
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateA(
    callback: LPDDENUMCALLBACKA,
    context: *mut c_void,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            s!("cnc-ddraw wgpu"),
            s!("display"),
            context,
        )
    };
    S_OK
}
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateW(
    callback: LPDDENUMCALLBACKW,
    context: *mut c_void,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            w!("cnc-ddraw wgpu"),
            w!("display"),
            context,
        )
    };
    S_OK
}
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateExA(
    callback: LPDDENUMCALLBACKEXA,
    context: *mut c_void,
    flags: u32,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    if flags
        & !((DDENUM_ATTACHEDSECONDARYDEVICES
            | DDENUM_DETACHEDSECONDARYDEVICES
            | DDENUM_NONDISPLAYDEVICES) as u32)
        != 0
    {
        return E_INVALIDARG;
    }
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            s!("cnc-ddraw wgpu"),
            s!("display"),
            context,
            HMONITOR::default(),
        )
    };
    S_OK
}
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawEnumerateExW(
    callback: LPDDENUMCALLBACKEXW,
    context: *mut c_void,
    flags: u32,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    if flags
        & !((DDENUM_ATTACHEDSECONDARYDEVICES
            | DDENUM_DETACHEDSECONDARYDEVICES
            | DDENUM_NONDISPLAYDEVICES) as u32)
        != 0
    {
        return E_INVALIDARG;
    }
    let _ = unsafe {
        callback(
            ptr::null_mut(),
            w!("cnc-ddraw wgpu"),
            w!("display"),
            context,
            HMONITOR::default(),
        )
    };
    S_OK
}
