use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use windows::core::{
    implement, s, w, Error, IUnknown, IUnknownImpl, Interface, OutRef, Ref, Result, BOOL, GUID,
    HRESULT, PCWSTR,
};
use windows::Win32::Foundation::{
    CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, HANDLE, HWND,
    SIZE, S_OK,
};
use windows::Win32::Graphics::DirectDraw::*;
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS, HDC,
    HMONITOR, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, PALETTEENTRY,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::{
    clipper, config, palette,
    renderer::Renderer,
    surface,
    window::{self, HostWindow, PresentationMode},
};

const DEFAULT_MODE: (u32, u32, u32) = (640, 480, 32);
const VIRTUAL_VIDMEM_BYTES: u32 = 64 * 1024 * 1024;
const ALREADY_INITIALIZED: HRESULT = HRESULT(0x8876_0005_u32 as i32);
const DDERR_CURRENTLYNOTAVAIL: HRESULT = HRESULT(0x8876_0028_u32 as i32);
const DDERR_INVALIDOBJECT: HRESULT = HRESULT(0x8876_0082_u32 as i32);
const DDERR_NOTFOUND: HRESULT = HRESULT(0x8876_00ff_u32 as i32);
const SURFACE_BUSY: HRESULT = HRESULT(0x8876_01ae_u32 as i32);
const DDERR_VERTICALBLANKINPROGRESS: HRESULT = HRESULT(0x8876_0219_u32 as i32);

static FULLSCREEN_COUNT: AtomicUsize = AtomicUsize::new(0);
const FLUSH_MAX_DEFER: Duration = Duration::from_millis(16);

thread_local! {
    static ACTIVE_DISPLAY: RefCell<Weak<RefCell<Display>>> = const { RefCell::new(Weak::new()) };
}

pub fn is_fullscreen_active() -> bool {
    FULLSCREEN_COUNT.load(Ordering::Relaxed) > 0
}

pub fn flush_pending_present(from_timer: bool) {
    let display = ACTIVE_DISPLAY.with(|slot| slot.borrow().upgrade());
    let Some(display) = display else {
        return;
    };
    let min_ms = config::get().min_frame_interval_ms();
    let Ok(mut d) = display.try_borrow_mut() else {
        window::arm_flush_timer(min_ms as u32);
        return;
    };
    d.flush_scheduled = false;
    if !d.dirty {
        return;
    }
    if !from_timer
        && d.last_present
            .is_some_and(|t| t.elapsed() < Duration::from_millis(min_ms))
    {
        window::arm_flush_timer(min_ms as u32);
        return;
    }
    let _ = d.present_now();
}

pub fn toggle_fullscreen_hotkey() {
    let display = ACTIVE_DISPLAY.with(|slot| slot.borrow().upgrade());
    let Some(display) = display else {
        return;
    };
    let Ok(mut d) = display.try_borrow_mut() else {
        return;
    };
    let (lw, lh) = (d.width, d.height);
    let cfg = config::get();
    if let Some(host) = d.host.as_mut() {
        let was_fs = host.is_fullscreen();
        if let Ok(new_mode) = host.toggle_mode(lw, lh, cfg) {
            let now_fs = new_mode == PresentationMode::BorderlessFullscreen;
            if !was_fs && now_fs {
                FULLSCREEN_COUNT.fetch_add(1, Ordering::Relaxed);
            } else if was_fs && !now_fs {
                FULLSCREEN_COUNT.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(renderer) = d.renderer.as_mut() {
                renderer.sync_overlay();
            }
            let _ = d.present_now();
        }
    }
}

pub fn on_host_window_changed() {
    let display = ACTIVE_DISPLAY.with(|slot| slot.borrow().upgrade());
    let Some(display) = display else {
        return;
    };
    let Ok(mut d) = display.try_borrow_mut() else {
        return;
    };
    if let Some(renderer) = d.renderer.as_mut() {
        renderer.sync_overlay();
    }
    if d.primary.upgrade().is_some() {
        let _ = d.refresh();
    }
}

pub struct Display {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    renderer: Option<Renderer>,
    host: Option<HostWindow>,
    rgba: Vec<u8>,
    pub primary: Weak<RefCell<surface::SurfaceData>>,
    surfaces: Vec<(*mut c_void, Weak<RefCell<surface::SurfaceData>>)>,
    dirty: bool,
    flush_scheduled: bool,
    last_present: Option<Instant>,
}

impl Display {
    pub fn register_surface(
        &mut self,
        raw: *mut c_void,
        data: Weak<RefCell<surface::SurfaceData>>,
    ) {
        self.surfaces.retain(|(_, w)| w.upgrade().is_some());
        self.surfaces.push((raw, data));
    }

    pub fn unregister_surface(&mut self, raw: *mut c_void) {
        self.surfaces.retain(|(p, _)| *p != raw);
    }

    pub fn allocated_bytes(&self) -> usize {
        self.surfaces
            .iter()
            .filter_map(|(_, w)| w.upgrade())
            .filter_map(|d| d.try_borrow().ok().map(|s| s.byte_len()))
            .sum()
    }

    pub fn live_surfaces(&self) -> Vec<IDirectDrawSurface> {
        self.surfaces
            .iter()
            .filter_map(|(raw, weak)| {
                weak.upgrade()?;
                unsafe { IDirectDrawSurface::from_raw_borrowed(raw) }.cloned()
            })
            .collect()
    }

    pub fn primary_surface(&self) -> Option<IDirectDrawSurface> {
        for (raw, weak) in &self.surfaces {
            let Some(data) = weak.upgrade() else {
                continue;
            };
            if data.try_borrow().is_ok_and(|s| s.is_primary()) {
                return unsafe { IDirectDrawSurface::from_raw_borrowed(raw) }.cloned();
            }
        }
        None
    }

    pub fn find_surface_by_dc(&self, hdc: HDC) -> Option<IDirectDrawSurface> {
        if hdc.is_invalid() {
            return None;
        }
        for (raw, weak) in &self.surfaces {
            let Some(data) = weak.upgrade() else {
                continue;
            };
            let matches = data.try_borrow().is_ok_and(|s| s.has_dc(hdc));
            if matches {
                return unsafe { IDirectDrawSurface::from_raw_borrowed(raw) }.cloned();
            }
        }
        None
    }

    fn set_host(&mut self, next: Option<HostWindow>) -> Option<HostWindow> {
        let prev = std::mem::replace(&mut self.host, next);
        let was_fs = prev.as_ref().is_some_and(HostWindow::is_fullscreen);
        let now_fs = self.host.as_ref().is_some_and(HostWindow::is_fullscreen);
        if !was_fs && now_fs {
            FULLSCREEN_COUNT.fetch_add(1, Ordering::Relaxed);
        } else if was_fs && !now_fs {
            FULLSCREEN_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
        prev
    }

    pub fn present_now(&mut self) -> Result<()> {
        let (Some(renderer), Some(primary)) = (&mut self.renderer, self.primary.upgrade()) else {
            self.dirty = false;
            self.flush_scheduled = false;
            return Ok(());
        };
        let primary = primary.try_borrow().map_err(|_| SURFACE_BUSY)?;
        if primary.locked() {
            return Ok(());
        }
        surface::frame_rgba(&primary, &mut self.rgba)?;
        let (width, height) = (primary.width, primary.height);
        drop(primary);
        self.dirty = false;
        self.flush_scheduled = false;
        self.last_present = Some(Instant::now());
        window::disarm_flush_timer();
        window::sync_presented_size(width, height);
        renderer
            .present(width, height, &self.rgba)
            .map_err(|message| Error::new(E_FAIL, message))
    }

    pub fn refresh(&mut self) -> Result<()> {
        if self.renderer.is_none() || self.primary.upgrade().is_none() {
            return Ok(());
        }
        self.dirty = true;
        let force_now = self.last_present.is_none()
            || self
                .last_present
                .is_some_and(|t| t.elapsed() >= FLUSH_MAX_DEFER)
            || !window::can_schedule_flush();
        if force_now {
            return self.present_now();
        }
        if !self.flush_scheduled {
            if window::schedule_flush() {
                self.flush_scheduled = true;
            } else {
                return self.present_now();
            }
        }
        Ok(())
    }
}

#[implement(IDirectDraw, IDirectDraw2, IDirectDraw4, IDirectDraw7, Agile = false)]
struct Draw {
    display: Rc<RefCell<Display>>,
    hwnd: Cell<HWND>,
    bind_id: Cell<usize>,
    exclusive: Cell<bool>,
    manage_window: Cell<bool>,
    changing_window: RefCell<()>,
    vblank_epoch: Instant,
    initialized: Cell<bool>,
}

impl Draw {
    fn new_with_init(initialized: bool) -> Self {
        let (width, height, bpp) = DEFAULT_MODE;
        Self {
            display: Rc::new(RefCell::new(Display {
                width,
                height,
                bpp,
                renderer: None,
                host: None,
                rgba: Vec::new(),
                primary: Weak::new(),
                surfaces: Vec::new(),
                dirty: false,
                flush_scheduled: false,
                last_present: None,
            })),
            hwnd: Cell::new(HWND::default()),
            bind_id: Cell::new(0),
            exclusive: Cell::new(false),
            manage_window: Cell::new(false),
            changing_window: RefCell::new(()),
            vblank_epoch: Instant::now(),
            initialized: Cell::new(initialized),
        }
    }

    fn new() -> Self {
        Self::new_with_init(true)
    }
}

pub fn create_uninitialized_draw() -> IDirectDraw {
    Draw::new_with_init(false).into()
}

impl Drop for Draw {
    fn drop(&mut self) {
        let id = self.bind_id.get();
        let owns_hooks = window::is_active_owner(id);
        if let Ok(mut display) = self.display.try_borrow_mut() {
            if let Some(saved) = display.set_host(None) {
                if owns_hooks {
                    let _ = saved.restore();
                }
            }
        }
        if id != 0 {
            window::unbind_window(id);
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

fn check_desc2(desc: *const DDSURFACEDESC2) -> Result<()> {
    if desc.is_null() || unsafe { (*desc).dwSize } != size_of::<DDSURFACEDESC2>() as u32 {
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

fn write_cstr(dst: &mut [i8], text: &str) {
    dst.fill(0);
    let max = dst.len().saturating_sub(1);
    for (slot, byte) in dst.iter_mut().zip(text.bytes()).take(max) {
        *slot = byte as i8;
    }
}

fn fill_device_identifier() -> DDDEVICEIDENTIFIER {
    let mut id = DDDEVICEIDENTIFIER::default();
    write_cstr(&mut id.szDriver, "ddraw.dll");
    write_cstr(&mut id.szDescription, "cnc-ddraw wgpu");
    id.liDriverVersion = 0x0001_0000_0000_0000;
    id.guidDeviceIdentifier = GUID::from_u128(0x636e6364_6472_6177_7767_707530303031);
    id
}

impl Draw_Impl {
    fn parent_interface(&self) -> IDirectDraw {
        self.as_interface::<IDirectDraw>().to_owned()
    }

    fn monitor_hz(&self) -> u32 {
        let mut hz = 0u32;
        if IDirectDraw_Impl::GetMonitorFrequency(self, &mut hz).is_ok() && (24..=500).contains(&hz)
        {
            hz
        } else {
            60
        }
    }

    /// Returns `(phase_us, active_end_us, frame_us, display_height)`.
    fn frame_timing(&self) -> (u64, u64, u64, u32) {
        let hz = self.monitor_hz() as u64;
        let frame_us = (1_000_000 / hz).max(1_000);
        let active_end_us = frame_us * 92 / 100;
        let elapsed_us = self.vblank_epoch.elapsed().as_micros() as u64;
        let phase_us = elapsed_us % frame_us;
        let height = self
            .display
            .try_borrow()
            .map(|d| d.height.max(1))
            .unwrap_or(480);
        (phase_us, active_end_us, frame_us, height)
    }

    fn create_surface_v1(
        &self,
        desc: *mut DDSURFACEDESC,
        out: OutRef<IDirectDrawSurface>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        put_interface(out, || {
            no_aggregation(outer)?;
            check_desc(desc)?;
            surface::create_surface(
                Some(self.parent_interface()),
                self.display.clone(),
                unsafe { &*desc },
            )
        })
    }

    fn create_surface_v2<S>(
        &self,
        desc: *mut DDSURFACEDESC2,
        out: OutRef<S>,
        outer: Ref<IUnknown>,
    ) -> Result<()>
    where
        S: Interface + windows_core::Type<S, Default = Option<S>>,
    {
        put_interface(out, || {
            no_aggregation(outer)?;
            check_desc2(desc)?;
            let d1 = surface::desc2_to_desc(unsafe { &*desc })?;
            let surf =
                surface::create_surface(Some(self.parent_interface()), self.display.clone(), &d1)?;
            surf.cast::<S>()
        })
    }

    fn duplicate_surface_impl<S: Interface>(&self, src: Ref<S>) -> Result<S> {
        let src_v1 = src.ok()?.cast::<IDirectDrawSurface>()?;
        let dup = surface::duplicate_surface(
            Some(self.parent_interface()),
            self.display.clone(),
            &src_v1,
        )?;
        dup.cast::<S>()
    }

    fn enum_surfaces_impl<S: Interface>(
        &self,
        flags: u32,
        filter: Option<DDSURFACEDESC>,
        mut emit: impl FnMut(S, &mut DDSURFACEDESC) -> i32,
    ) -> Result<()> {
        let allowed = (DDENUMSURFACES_ALL
            | DDENUMSURFACES_MATCH
            | DDENUMSURFACES_NOMATCH
            | DDENUMSURFACES_CANBECREATED
            | DDENUMSURFACES_DOESEXIST) as u32;
        if flags & !allowed != 0 {
            return Err(E_INVALIDARG.into());
        }
        let all = flags & DDENUMSURFACES_ALL as u32 != 0;
        let matched = flags & DDENUMSURFACES_MATCH as u32 != 0;
        let nomatch = flags & DDENUMSURFACES_NOMATCH as u32 != 0;
        let can_create = flags & DDENUMSURFACES_CANBECREATED as u32 != 0;
        let exists = flags & DDENUMSURFACES_DOESEXIST as u32 != 0;
        if (all as u8 + matched as u8 + nomatch as u8) != 1
            || (can_create as u8 + exists as u8) != 1
        {
            return Err(E_INVALIDARG.into());
        }
        if (matched || nomatch) && filter.is_none() {
            return Err(E_INVALIDARG.into());
        }
        if can_create {
            return Ok(());
        }
        let snapshot = self
            .display
            .try_borrow()
            .map_err(|_| SURFACE_BUSY)?
            .live_surfaces();
        for surf in snapshot {
            let keep = if all {
                true
            } else {
                let is_match = surface::matches_desc(&surf, filter.as_ref().unwrap())?;
                if matched {
                    is_match
                } else {
                    !is_match
                }
            };
            if !keep {
                continue;
            }
            let mut desc = surface::surface_desc(&surf)?;
            let typed = surf.cast::<S>()?;
            if emit(typed, &mut desc) == DDENUMRET_CANCEL as i32 {
                break;
            }
        }
        Ok(())
    }

    fn enum_display_modes_impl(
        &self,
        flags: u32,
        filter: DDSURFACEDESC,
        mut emit: impl FnMut(&mut DDSURFACEDESC) -> i32,
    ) -> Result<()> {
        if flags & !((DDEDM_REFRESHRATES | DDEDM_STANDARDVGAMODES) as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        if filter.dwFlags
            & !((DDSD_WIDTH | DDSD_HEIGHT | DDSD_PIXELFORMAT | DDSD_REFRESHRATE) as u32)
            != 0
        {
            return Err(E_INVALIDARG.into());
        }
        let current = self.display.try_borrow().map_err(|_| SURFACE_BUSY)?;
        let current_size = (current.width, current.height);
        drop(current);
        let refresh = self.monitor_hz();
        let include_refresh = flags & DDEDM_REFRESHRATES as u32 != 0;
        let sizes = [
            (320, 200),
            (320, 240),
            (640, 400),
            (640, 480),
            (800, 600),
            (1024, 768),
            (1280, 720),
            (1280, 960),
            (1280, 1024),
            (1600, 1200),
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
                if include_refresh {
                    mode.dwFlags |= DDSD_REFRESHRATE as u32;
                    mode.Anonymous2.dwRefreshRate = refresh;
                }
                if filter.dwFlags & DDSD_REFRESHRATE as u32 != 0 {
                    let wanted = unsafe { filter.Anonymous2.dwRefreshRate };
                    if wanted != 0 && wanted != refresh {
                        continue;
                    }
                }
                if filter.dwFlags & DDSD_PIXELFORMAT as u32 != 0 {
                    let requested = &filter.ddpfPixelFormat;
                    let format = &mode.ddpfPixelFormat;
                    let masks_requested = unsafe {
                        requested.Anonymous2.dwRBitMask
                            | requested.Anonymous3.dwGBitMask
                            | requested.Anonymous4.dwBBitMask
                    } != 0;
                    if (requested.dwFlags != 0 && requested.dwFlags != format.dwFlags)
                        || unsafe {
                            (requested.Anonymous1.dwRGBBitCount != 0
                                && requested.Anonymous1.dwRGBBitCount != bpp)
                                || (masks_requested
                                    && (
                                        requested.Anonymous2.dwRBitMask,
                                        requested.Anonymous3.dwGBitMask,
                                        requested.Anonymous4.dwBBitMask,
                                    ) != (
                                        format.Anonymous2.dwRBitMask,
                                        format.Anonymous3.dwGBitMask,
                                        format.Anonymous4.dwBBitMask,
                                    ))
                        }
                    {
                        continue;
                    }
                }
                if emit(&mut mode) == DDENUMRET_CANCEL as i32 {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn get_available_vid_mem_impl(&self, total: *mut u32, free: *mut u32) -> Result<()> {
        if total.is_null() && free.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let used = self
            .display
            .try_borrow()
            .map_err(|_| SURFACE_BUSY)?
            .allocated_bytes()
            .min(VIRTUAL_VIDMEM_BYTES as usize) as u32;
        if let Some(total) = unsafe { total.as_mut() } {
            *total = VIRTUAL_VIDMEM_BYTES;
        }
        if let Some(free) = unsafe { free.as_mut() } {
            *free = VIRTUAL_VIDMEM_BYTES.saturating_sub(used);
        }
        Ok(())
    }

    fn get_surface_from_dc_impl<S: Interface>(&self, hdc: HDC) -> Result<S> {
        let surface = self
            .display
            .try_borrow()
            .map_err(|_| SURFACE_BUSY)?
            .find_surface_by_dc(hdc)
            .ok_or(DDERR_NOTFOUND)?;
        surface.cast::<S>()
    }
}

#[allow(non_snake_case)]
impl IDirectDraw_Impl for Draw_Impl {
    fn Compact(&self) -> Result<()> {
        Ok(())
    }

    fn CreateClipper(
        &self,
        flags: u32,
        out: OutRef<IDirectDrawClipper>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        put_interface(out, || {
            no_aggregation(outer)?;
            clipper::create_clipper(flags, Some(self.display.clone()))
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
        self.create_surface_v1(desc, out, outer)
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface>) -> Result<IDirectDrawSurface> {
        self.duplicate_surface_impl(src)
    }

    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if filter.is_null() {
            DDSURFACEDESC::default()
        } else {
            check_desc(filter)?;
            unsafe { *filter }
        };
        self.enum_display_modes_impl(flags, filter, |mode| unsafe { callback(mode, context).0 })
    }

    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if desc.is_null() {
            None
        } else {
            check_desc(desc)?;
            Some(unsafe { *desc })
        };
        self.enum_surfaces_impl::<IDirectDrawSurface>(flags, filter, |surf, d| {
            let owned = ManuallyDrop::new(surf);
            unsafe { callback((&*owned).into(), d, context).0 }
        })
    }
    fn FlipToGDISurface(&self) -> Result<()> {
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        display.present_now()
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
        for output in [driver, software] {
            if output.is_null() {
                continue;
            }
            let size = unsafe { (*output).dwSize };
            let mut caps = DDCAPS_DX7 {
                dwSize: size,
                dwCaps: (DDCAPS_BLT
                    | DDCAPS_BLTSTRETCH
                    | DDCAPS_BLTCOLORFILL
                    | DDCAPS_COLORKEY
                    | DDCAPS_CANBLTSYSMEM
                    | DDCAPS_CANCLIP
                    | DDCAPS_CANCLIPSTRETCHED
                    | DDCAPS_OVERLAY
                    | DDCAPS_PALETTE) as u32,
                dwCaps2: (DDCAPS2_PRIMARYGAMMA | DDCAPS2_NOPAGELOCKREQUIRED | DDCAPS2_WIDESURFACES)
                    as u32,
                dwCKeyCaps: (DDCKEYCAPS_SRCBLT | DDCKEYCAPS_SRCBLTCLRSPACE | DDCKEYCAPS_DESTBLT)
                    as u32,
                dwFXCaps: (DDFXCAPS_BLTMIRRORLEFTRIGHT
                    | DDFXCAPS_BLTMIRRORUPDOWN
                    | DDFXCAPS_BLTSTRETCHX
                    | DDFXCAPS_BLTSTRETCHY) as u32,
                dwPalCaps: (DDPCAPS_8BIT | DDPCAPS_ALLOW256 | DDPCAPS_PRIMARYSURFACE) as u32,
                dwVidMemTotal: VIRTUAL_VIDMEM_BYTES,
                dwVidMemFree: VIRTUAL_VIDMEM_BYTES,
                ..Default::default()
            };
            caps.ddsOldCaps.dwCaps = (DDSCAPS_PRIMARYSURFACE
                | DDSCAPS_OFFSCREENPLAIN
                | DDSCAPS_SYSTEMMEMORY
                | DDSCAPS_VIDEOMEMORY
                | DDSCAPS_LOCALVIDMEM
                | DDSCAPS_NONLOCALVIDMEM
                | DDSCAPS_OWNDC
                | DDSCAPS_FLIP
                | DDSCAPS_FRONTBUFFER
                | DDSCAPS_BACKBUFFER
                | DDSCAPS_OVERLAY
                | DDSCAPS_COMPLEX) as u32;
            caps.ddsCaps.dwCaps = caps.ddsOldCaps.dwCaps;
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
        self.display
            .try_borrow()
            .map_err(|_| SURFACE_BUSY)?
            .primary_surface()
            .ok_or_else(|| Error::from(DDERR_NOTFOUND))
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
    fn GetScanLine(&self, line: *mut u32) -> Result<()> {
        let out = unsafe { line.as_mut() }.ok_or(E_INVALIDARG)?;
        let (phase_us, active_end_us, _, height) = self.frame_timing();
        if phase_us >= active_end_us {
            *out = 0;
            return Err(DDERR_VERTICALBLANKINPROGRESS.into());
        }
        *out = ((phase_us * height as u64) / active_end_us.max(1)) as u32;
        Ok(())
    }
    fn GetVerticalBlankStatus(&self, status: *mut BOOL) -> Result<()> {
        let out = unsafe { status.as_mut() }.ok_or(E_INVALIDARG)?;
        let (phase_us, active_end_us, _, _) = self.frame_timing();
        *out = BOOL::from(phase_us >= active_end_us);
        Ok(())
    }
    fn Initialize(&self, device: *mut GUID) -> Result<()> {
        if self.initialized.get() {
            return Err(ALREADY_INITIALIZED.into());
        }
        if !valid_device_guid(device) {
            return Err(E_INVALIDARG.into());
        }
        self.initialized.set(true);
        Ok(())
    }
    fn RestoreDisplayMode(&self) -> Result<()> {
        let _transition = self
            .changing_window
            .try_borrow_mut()
            .map_err(|_| SURFACE_BUSY)?;
        window::set_scaling_active(self.bind_id.get(), false);
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        if let Some(saved) = display.set_host(None) {
            saved.restore()?;
        }
        if display.primary.upgrade().is_none() {
            (display.width, display.height, display.bpp) = DEFAULT_MODE;
        }
        display.present_now()
    }

    fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
        let known_flags = (DDSCL_NORMAL
            | DDSCL_FULLSCREEN
            | DDSCL_EXCLUSIVE
            | DDSCL_ALLOWREBOOT
            | DDSCL_ALLOWMODEX
            | DDSCL_NOWINDOWCHANGES
            | DDSCL_FPUSETUP
            | DDSCL_FPUPRESERVE
            | DDSCL_MULTITHREADED
            | DDSCL_CREATEDEVICEWINDOW
            | DDSCL_SETDEVICEWINDOW
            | DDSCL_SETFOCUSWINDOW) as u32;
        if flags & !known_flags != 0 {
            return Err(E_INVALIDARG.into());
        }
        let normal = flags & DDSCL_NORMAL as u32 != 0;
        let exclusive = flags & DDSCL_EXCLUSIVE as u32 != 0;
        let fullscreen = flags & DDSCL_FULLSCREEN as u32 != 0;
        let no_window_changes = flags & DDSCL_NOWINDOWCHANGES as u32 != 0;
        let window_only = flags
            & (DDSCL_SETFOCUSWINDOW | DDSCL_SETDEVICEWINDOW | DDSCL_CREATEDEVICEWINDOW) as u32
            != 0;
        if (normal && exclusive)
            || (exclusive && !fullscreen)
            || (!normal && !exclusive && !window_only)
        {
            return Err(E_INVALIDARG.into());
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
        let manage_window = exclusive && !no_window_changes;
        let change_hwnd = self.hwnd.get() != hwnd;
        {
            let display = self.display.try_borrow().map_err(|_| SURFACE_BUSY)?;
            if !change_hwnd
                && display.host.is_some() == manage_window
                && self.exclusive.get() == exclusive
            {
                return Ok(());
            }
        }
        let replacement = if change_hwnd && !hwnd.is_invalid() {
            Some(Renderer::new(hwnd.0 as isize).map_err(|message| Error::new(E_FAIL, message))?)
        } else {
            None
        };
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        if let Some(saved) = display.set_host(None) {
            saved.restore()?;
        }
        if manage_window {
            let cfg = config::get();
            let initial_mode = if cfg.windowed {
                PresentationMode::Windowed
            } else {
                PresentationMode::BorderlessFullscreen
            };
            let host = HostWindow::enter(hwnd, initial_mode, display.width, display.height, cfg)?;
            display.set_host(Some(host));
        }
        self.exclusive.set(exclusive);
        self.manage_window.set(manage_window);
        if change_hwnd {
            display.renderer = replacement;
            self.hwnd.set(hwnd);
            let prev_id = self.bind_id.get();
            if hwnd.is_invalid() {
                window::unbind_window(prev_id);
                self.bind_id.set(0);
            } else {
                let id = window::bind_window(hwnd, exclusive);
                self.bind_id.set(id);
                ACTIVE_DISPLAY.with(|slot| *slot.borrow_mut() = Rc::downgrade(&self.display));
            }
        } else {
            window::set_scaling_active(self.bind_id.get(), exclusive);
        }
        display.present_now()
    }
    fn SetDisplayMode(&self, width: u32, height: u32, bpp: u32) -> Result<()> {
        if !(1..=8192).contains(&width)
            || !(1..=8192).contains(&height)
            || ![8, 16, 32].contains(&bpp)
        {
            return Err(E_INVALIDARG.into());
        }
        {
            let display = self.display.try_borrow().map_err(|_| SURFACE_BUSY)?;
            if (display.width, display.height, display.bpp) != (width, height, bpp)
                && display.primary.upgrade().is_some()
            {
                return Err(SURFACE_BUSY.into());
            }
        }
        let hwnd = self.hwnd.get();
        let cfg = config::get();
        let _transition = self
            .changing_window
            .try_borrow_mut()
            .map_err(|_| SURFACE_BUSY)?;
        let mut display = self.display.try_borrow_mut().map_err(|_| SURFACE_BUSY)?;
        if (display.width, display.height, display.bpp) != (width, height, bpp)
            && display.primary.upgrade().is_some()
        {
            return Err(SURFACE_BUSY.into());
        }
        if self.exclusive.get() && !hwnd.is_invalid() {
            if self.manage_window.get() && display.host.is_none() {
                let mode = if cfg.windowed {
                    PresentationMode::Windowed
                } else {
                    PresentationMode::BorderlessFullscreen
                };
                let host = HostWindow::enter(hwnd, mode, width, height, cfg)?;
                display.set_host(Some(host));
            } else if let Some(host) = display.host.as_mut() {
                host.on_logical_mode_changed(width, height, cfg);
            }
            window::set_scaling_active(self.bind_id.get(), true);
        }
        (display.width, display.height, display.bpp) = (width, height, bpp);
        window::set_mode(width, height, bpp, true);
        if let Some(renderer) = display.renderer.as_mut() {
            renderer.sync_overlay();
        }
        Ok(())
    }
    fn WaitForVerticalBlank(&self, flags: u32, event: HANDLE) -> Result<()> {
        if !event.is_invalid() {
            return Err(E_NOTIMPL.into());
        }
        if let Ok(mut display) = self.display.try_borrow_mut() {
            if display.dirty {
                let _ = display.present_now();
            }
        }
        let (phase_us, active_end_us, frame_us, _) = self.frame_timing();
        let wait_us = match flags as i32 {
            DDWAITVB_BLOCKBEGIN => {
                if phase_us < active_end_us {
                    active_end_us - phase_us
                } else {
                    (frame_us - phase_us) + active_end_us
                }
            }
            DDWAITVB_BLOCKEND => frame_us - phase_us,
            DDWAITVB_BLOCKBEGINEVENT => return Err(E_NOTIMPL.into()),
            _ => return Err(E_INVALIDARG.into()),
        };
        if wait_us > 0 {
            std::thread::sleep(Duration::from_micros(wait_us.min(frame_us)));
        }
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IDirectDraw2_Impl for Draw_Impl {
    fn Compact(&self) -> Result<()> {
        IDirectDraw_Impl::Compact(self)
    }
    fn CreateClipper(
        &self,
        flags: u32,
        out: OutRef<IDirectDrawClipper>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreateClipper(self, flags, out, outer)
    }
    fn CreatePalette(
        &self,
        flags: u32,
        entries: *mut PALETTEENTRY,
        out: OutRef<IDirectDrawPalette>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreatePalette(self, flags, entries, out, outer)
    }
    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC,
        out: OutRef<IDirectDrawSurface>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        self.create_surface_v1(desc, out, outer)
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface>) -> Result<IDirectDrawSurface> {
        self.duplicate_surface_impl(src)
    }
    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK,
    ) -> Result<()> {
        IDirectDraw_Impl::EnumDisplayModes(self, flags, filter, context, callback)
    }
    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK,
    ) -> Result<()> {
        IDirectDraw_Impl::EnumSurfaces(self, flags, desc, context, callback)
    }
    fn FlipToGDISurface(&self) -> Result<()> {
        IDirectDraw_Impl::FlipToGDISurface(self)
    }
    fn GetCaps(&self, driver: *mut DDCAPS_DX7, software: *mut DDCAPS_DX7) -> Result<()> {
        IDirectDraw_Impl::GetCaps(self, driver, software)
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC) -> Result<()> {
        IDirectDraw_Impl::GetDisplayMode(self, desc)
    }
    fn GetFourCCCodes(&self, count: *mut u32, codes: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetFourCCCodes(self, count, codes)
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface> {
        IDirectDraw_Impl::GetGDISurface(self)
    }
    fn GetMonitorFrequency(&self, output: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetMonitorFrequency(self, output)
    }
    fn GetScanLine(&self, line: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetScanLine(self, line)
    }
    fn GetVerticalBlankStatus(&self, status: *mut BOOL) -> Result<()> {
        IDirectDraw_Impl::GetVerticalBlankStatus(self, status)
    }
    fn Initialize(&self, guid: *mut GUID) -> Result<()> {
        IDirectDraw_Impl::Initialize(self, guid)
    }
    fn RestoreDisplayMode(&self) -> Result<()> {
        IDirectDraw_Impl::RestoreDisplayMode(self)
    }
    fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
        IDirectDraw_Impl::SetCooperativeLevel(self, hwnd, flags)
    }
    fn SetDisplayMode(
        &self,
        width: u32,
        height: u32,
        bpp: u32,
        _refresh_rate: u32,
        _flags: u32,
    ) -> Result<()> {
        IDirectDraw_Impl::SetDisplayMode(self, width, height, bpp)
    }
    fn WaitForVerticalBlank(&self, flags: u32, event: HANDLE) -> Result<()> {
        IDirectDraw_Impl::WaitForVerticalBlank(self, flags, event)
    }
    fn GetAvailableVidMem(
        &self,
        caps: *mut DDSCAPS,
        total: *mut u32,
        free: *mut u32,
    ) -> Result<()> {
        if caps.is_null() {
            return Err(E_INVALIDARG.into());
        }
        self.get_available_vid_mem_impl(total, free)
    }
}

#[allow(non_snake_case)]
impl IDirectDraw4_Impl for Draw_Impl {
    fn Compact(&self) -> Result<()> {
        IDirectDraw_Impl::Compact(self)
    }
    fn CreateClipper(
        &self,
        flags: u32,
        out: OutRef<IDirectDrawClipper>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreateClipper(self, flags, out, outer)
    }
    fn CreatePalette(
        &self,
        flags: u32,
        entries: *mut PALETTEENTRY,
        out: OutRef<IDirectDrawPalette>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreatePalette(self, flags, entries, out, outer)
    }
    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC2,
        out: OutRef<IDirectDrawSurface4>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        self.create_surface_v2(desc, out, outer)
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface4>) -> Result<IDirectDrawSurface4> {
        self.duplicate_surface_impl(src)
    }
    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if filter.is_null() {
            DDSURFACEDESC::default()
        } else {
            check_desc2(filter)?;
            surface::desc2_to_desc(unsafe { &*filter })?
        };
        self.enum_display_modes_impl(flags, filter, |mode| {
            let mut mode2 = surface::desc_to_desc2(mode);
            unsafe { callback(&mut mode2, context).0 }
        })
    }
    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK2,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if desc.is_null() {
            None
        } else {
            check_desc2(desc)?;
            Some(surface::desc2_to_desc(unsafe { &*desc })?)
        };
        self.enum_surfaces_impl::<IDirectDrawSurface4>(flags, filter, |surf, d| {
            let mut d2 = surface::desc_to_desc2(d);
            let owned = ManuallyDrop::new(surf);
            unsafe { callback((&*owned).into(), &mut d2, context).0 }
        })
    }
    fn FlipToGDISurface(&self) -> Result<()> {
        IDirectDraw_Impl::FlipToGDISurface(self)
    }
    fn GetCaps(&self, driver: *mut DDCAPS_DX7, software: *mut DDCAPS_DX7) -> Result<()> {
        IDirectDraw_Impl::GetCaps(self, driver, software)
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC2) -> Result<()> {
        check_desc2(desc)?;
        let d = self.display.try_borrow().map_err(|_| SURFACE_BUSY)?;
        let mode = mode_description(d.width, d.height, d.bpp)?;
        unsafe { desc.write(surface::desc_to_desc2(&mode)) };
        Ok(())
    }
    fn GetFourCCCodes(&self, count: *mut u32, codes: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetFourCCCodes(self, count, codes)
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface4> {
        IDirectDraw_Impl::GetGDISurface(self)?.cast()
    }
    fn GetMonitorFrequency(&self, output: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetMonitorFrequency(self, output)
    }
    fn GetScanLine(&self, line: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetScanLine(self, line)
    }
    fn GetVerticalBlankStatus(&self, status: *mut BOOL) -> Result<()> {
        IDirectDraw_Impl::GetVerticalBlankStatus(self, status)
    }
    fn Initialize(&self, guid: *mut GUID) -> Result<()> {
        IDirectDraw_Impl::Initialize(self, guid)
    }
    fn RestoreDisplayMode(&self) -> Result<()> {
        IDirectDraw_Impl::RestoreDisplayMode(self)
    }
    fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
        IDirectDraw_Impl::SetCooperativeLevel(self, hwnd, flags)
    }
    fn SetDisplayMode(
        &self,
        width: u32,
        height: u32,
        bpp: u32,
        _refresh_rate: u32,
        _flags: u32,
    ) -> Result<()> {
        IDirectDraw_Impl::SetDisplayMode(self, width, height, bpp)
    }
    fn WaitForVerticalBlank(&self, flags: u32, event: HANDLE) -> Result<()> {
        IDirectDraw_Impl::WaitForVerticalBlank(self, flags, event)
    }
    fn GetAvailableVidMem(
        &self,
        caps: *mut DDSCAPS2,
        total: *mut u32,
        free: *mut u32,
    ) -> Result<()> {
        if caps.is_null() {
            return Err(E_INVALIDARG.into());
        }
        self.get_available_vid_mem_impl(total, free)
    }
    fn GetSurfaceFromDC(&self, hdc: HDC) -> Result<IDirectDrawSurface4> {
        self.get_surface_from_dc_impl(hdc)
    }
    fn RestoreAllSurfaces(&self) -> Result<()> {
        Ok(())
    }
    fn TestCooperativeLevel(&self) -> Result<()> {
        Ok(())
    }
    fn GetDeviceIdentifier(&self, out: *mut DDDEVICEIDENTIFIER, flags: u32) -> Result<()> {
        if flags & !(DDGDI_GETHOSTIDENTIFIER as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        *out = fill_device_identifier();
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IDirectDraw7_Impl for Draw_Impl {
    fn Compact(&self) -> Result<()> {
        IDirectDraw_Impl::Compact(self)
    }
    fn CreateClipper(
        &self,
        flags: u32,
        out: OutRef<IDirectDrawClipper>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreateClipper(self, flags, out, outer)
    }
    fn CreatePalette(
        &self,
        flags: u32,
        entries: *mut PALETTEENTRY,
        out: OutRef<IDirectDrawPalette>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        IDirectDraw_Impl::CreatePalette(self, flags, entries, out, outer)
    }
    fn CreateSurface(
        &self,
        desc: *mut DDSURFACEDESC2,
        out: OutRef<IDirectDrawSurface7>,
        outer: Ref<IUnknown>,
    ) -> Result<()> {
        self.create_surface_v2(desc, out, outer)
    }
    fn DuplicateSurface(&self, src: Ref<IDirectDrawSurface7>) -> Result<IDirectDrawSurface7> {
        self.duplicate_surface_impl(src)
    }
    fn EnumDisplayModes(
        &self,
        flags: u32,
        filter: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMMODESCALLBACK2,
    ) -> Result<()> {
        IDirectDraw4_Impl::EnumDisplayModes(self, flags, filter, context, callback)
    }
    fn EnumSurfaces(
        &self,
        flags: u32,
        desc: *mut DDSURFACEDESC2,
        context: *mut c_void,
        callback: LPDDENUMSURFACESCALLBACK7,
    ) -> Result<()> {
        let callback = callback.ok_or(E_INVALIDARG)?;
        let filter = if desc.is_null() {
            None
        } else {
            check_desc2(desc)?;
            Some(surface::desc2_to_desc(unsafe { &*desc })?)
        };
        self.enum_surfaces_impl::<IDirectDrawSurface7>(flags, filter, |surf, d| {
            let mut d2 = surface::desc_to_desc2(d);
            let owned = ManuallyDrop::new(surf);
            unsafe { callback((&*owned).into(), &mut d2, context).0 }
        })
    }
    fn FlipToGDISurface(&self) -> Result<()> {
        IDirectDraw_Impl::FlipToGDISurface(self)
    }
    fn GetCaps(&self, driver: *mut DDCAPS_DX7, software: *mut DDCAPS_DX7) -> Result<()> {
        IDirectDraw_Impl::GetCaps(self, driver, software)
    }
    fn GetDisplayMode(&self, desc: *mut DDSURFACEDESC2) -> Result<()> {
        IDirectDraw4_Impl::GetDisplayMode(self, desc)
    }
    fn GetFourCCCodes(&self, count: *mut u32, codes: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetFourCCCodes(self, count, codes)
    }
    fn GetGDISurface(&self) -> Result<IDirectDrawSurface7> {
        IDirectDraw_Impl::GetGDISurface(self)?.cast()
    }
    fn GetMonitorFrequency(&self, output: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetMonitorFrequency(self, output)
    }
    fn GetScanLine(&self, line: *mut u32) -> Result<()> {
        IDirectDraw_Impl::GetScanLine(self, line)
    }
    fn GetVerticalBlankStatus(&self, status: *mut BOOL) -> Result<()> {
        IDirectDraw_Impl::GetVerticalBlankStatus(self, status)
    }
    fn Initialize(&self, guid: *mut GUID) -> Result<()> {
        IDirectDraw_Impl::Initialize(self, guid)
    }
    fn RestoreDisplayMode(&self) -> Result<()> {
        IDirectDraw_Impl::RestoreDisplayMode(self)
    }
    fn SetCooperativeLevel(&self, hwnd: HWND, flags: u32) -> Result<()> {
        IDirectDraw_Impl::SetCooperativeLevel(self, hwnd, flags)
    }
    fn SetDisplayMode(
        &self,
        width: u32,
        height: u32,
        bpp: u32,
        refresh_rate: u32,
        flags: u32,
    ) -> Result<()> {
        IDirectDraw4_Impl::SetDisplayMode(self, width, height, bpp, refresh_rate, flags)
    }
    fn WaitForVerticalBlank(&self, flags: u32, event: HANDLE) -> Result<()> {
        IDirectDraw_Impl::WaitForVerticalBlank(self, flags, event)
    }
    fn GetAvailableVidMem(
        &self,
        caps: *mut DDSCAPS2,
        total: *mut u32,
        free: *mut u32,
    ) -> Result<()> {
        IDirectDraw4_Impl::GetAvailableVidMem(self, caps, total, free)
    }
    fn GetSurfaceFromDC(&self, hdc: HDC) -> Result<IDirectDrawSurface7> {
        self.get_surface_from_dc_impl(hdc)
    }
    fn RestoreAllSurfaces(&self) -> Result<()> {
        Ok(())
    }
    fn TestCooperativeLevel(&self) -> Result<()> {
        Ok(())
    }
    fn GetDeviceIdentifier(&self, out: *mut DDDEVICEIDENTIFIER2, flags: u32) -> Result<()> {
        if flags & !(DDGDI_GETHOSTIDENTIFIER as u32) != 0 {
            return Err(E_INVALIDARG.into());
        }
        let out = unsafe { out.as_mut() }.ok_or(E_INVALIDARG)?;
        let base = fill_device_identifier();
        *out = DDDEVICEIDENTIFIER2 {
            szDriver: base.szDriver,
            szDescription: base.szDescription,
            liDriverVersion: base.liDriverVersion,
            dwVendorId: base.dwVendorId,
            dwDeviceId: base.dwDeviceId,
            dwSubSysId: base.dwSubSysId,
            dwRevision: base.dwRevision,
            guidDeviceIdentifier: base.guidDeviceIdentifier,
            dwWHQLLevel: 0,
        };
        Ok(())
    }
    fn StartModeTest(&self, _: *mut SIZE, _: u32, _: u32) -> Result<()> {
        Err(DDERR_CURRENTLYNOTAVAIL.into())
    }
    fn EvaluateMode(&self, _: u32, _: *mut u32) -> Result<()> {
        Err(DDERR_INVALIDOBJECT.into())
    }
}

fn valid_device_guid(device: *const GUID) -> bool {
    device.is_null()
        || device as usize == DDCREATE_HARDWAREONLY as usize
        || device as usize == DDCREATE_EMULATIONONLY as usize
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
    if !valid_device_guid(device) {
        return E_INVALIDARG;
    }
    let device: IDirectDraw = Draw::new().into();
    unsafe { out.write(device.into_raw()) };
    S_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawCreateEx(
    device: *const GUID,
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
    if !valid_device_guid(device) {
        return E_INVALIDARG;
    }
    let requested = unsafe { *iid };
    if requested != IDirectDraw7::IID {
        return E_NOINTERFACE;
    }
    let dd: IDirectDraw7 = Draw::new().into();
    unsafe { out.write(dd.into_raw()) };
    S_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectDrawCreateClipper(
    flags: u32,
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
    match clipper::create_clipper(flags, None) {
        Ok(clipper) => {
            unsafe { out.write(clipper.into_raw()) };
            S_OK
        }
        Err(error) => error.code(),
    }
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
