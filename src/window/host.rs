//! Game host HWND lifecycle: borderless fullscreen, windowed mode, live Alt+Enter toggle,
//! and lossless restoration of original window style and placement.
use std::mem::size_of;

use windows::core::Result;
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_SUCCESS, E_INVALIDARG, HWND, RECT,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, GetWindowLongW, GetWindowPlacement, GetWindowRect, IsWindow,
    SetWindowLongW, SetWindowPlacement, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WINDOWPLACEMENT, WINDOW_EX_STYLE,
    WINDOW_LONG_PTR_INDEX, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
    WS_OVERLAPPEDWINDOW, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

use crate::config::Config;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationMode {
    BorderlessFullscreen,
    Windowed,
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

fn monitor_info(hwnd: HWND) -> Result<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).ok()?;
    }
    Ok(info)
}

#[derive(Clone, Copy)]
pub struct HostWindow {
    hwnd: HWND,
    placement: WINDOWPLACEMENT,
    orig_style: i32,
    orig_ex_style: i32,
    mode: PresentationMode,
    saved_windowed_rect: Option<RECT>,
    has_custom_window_rect: bool,
}

impl HostWindow {
    pub fn enter(
        hwnd: HWND,
        mode: PresentationMode,
        logical_w: u32,
        logical_h: u32,
        cfg: &Config,
    ) -> Result<Self> {
        let orig_style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
        if orig_style as u32 & WS_CHILD.0 != 0 {
            return Err(E_INVALIDARG.into());
        }
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        unsafe { GetWindowPlacement(hwnd, &mut placement)? };
        let mut host = Self {
            hwnd,
            placement,
            orig_style,
            orig_ex_style: unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) },
            mode,
            saved_windowed_rect: None,
            has_custom_window_rect: false,
        };
        let result = match mode {
            PresentationMode::BorderlessFullscreen => host.apply_borderless_fullscreen(),
            PresentationMode::Windowed => host.apply_windowed(logical_w, logical_h, cfg),
        };
        if let Err(err) = result {
            let _ = host.restore();
            return Err(err);
        }
        Ok(host)
    }

    pub fn is_fullscreen(&self) -> bool {
        self.mode == PresentationMode::BorderlessFullscreen
    }

    pub fn toggle_mode(
        &mut self,
        logical_w: u32,
        logical_h: u32,
        cfg: &Config,
    ) -> Result<PresentationMode> {
        match self.mode {
            PresentationMode::BorderlessFullscreen => {
                self.apply_windowed(logical_w, logical_h, cfg)?;
                self.mode = PresentationMode::Windowed;
            }
            PresentationMode::Windowed => {
                let mut rc = RECT::default();
                if unsafe { GetWindowRect(self.hwnd, &mut rc) }.is_ok()
                    && rc.right > rc.left
                    && rc.bottom > rc.top
                {
                    self.saved_windowed_rect = Some(rc);
                    self.has_custom_window_rect = true;
                }
                self.apply_borderless_fullscreen()?;
                self.mode = PresentationMode::BorderlessFullscreen;
            }
        }
        Ok(self.mode)
    }

    /// Updates windowed size when the game switches logical resolution and the user hasn't
    /// configured a fixed size or manually resized/moved the window.
    pub fn on_logical_mode_changed(&mut self, logical_w: u32, logical_h: u32, cfg: &Config) {
        if self.mode == PresentationMode::Windowed
            && !self.has_custom_window_rect
            && cfg.width == 0
            && cfg.height == 0
        {
            let _ = self.apply_windowed(logical_w, logical_h, cfg);
        }
    }

    fn apply_borderless_fullscreen(&self) -> Result<()> {
        let info = monitor_info(self.hwnd)?;
        let rect = info.rcMonitor;
        let edge_styles =
            WS_EX_CLIENTEDGE.0 | WS_EX_WINDOWEDGE.0 | WS_EX_DLGMODALFRAME.0 | WS_EX_STATICEDGE.0;
        set_style(
            self.hwnd,
            GWL_STYLE,
            ((self.orig_style as u32 & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0) as i32,
        )?;
        set_style(
            self.hwnd,
            GWL_EXSTYLE,
            (self.orig_ex_style as u32 & !edge_styles) as i32,
        )?;
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
    }

    fn apply_windowed(&mut self, logical_w: u32, logical_h: u32, cfg: &Config) -> Result<()> {
        let info = monitor_info(self.hwnd)?;
        let work = info.rcWork;
        let mut style = (self.orig_style as u32) & !(WS_POPUP.0 | WS_OVERLAPPEDWINDOW.0);
        if cfg.border {
            style |= WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0;
            if cfg.resizable {
                style |= WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0;
            }
        } else {
            style |= WS_POPUP.0;
        }
        let ex_style = self.orig_ex_style as u32;
        set_style(self.hwnd, GWL_STYLE, style as i32)?;
        set_style(self.hwnd, GWL_EXSTYLE, ex_style as i32)?;

        let (x, y, win_w, win_h) = if let Some(rc) = self.saved_windowed_rect {
            (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top)
        } else {
            let client_w = if cfg.width > 0 {
                cfg.width
            } else {
                logical_w.max(320)
            } as i32;
            let client_h = if cfg.height > 0 {
                cfg.height
            } else {
                logical_h.max(200)
            } as i32;
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: client_w,
                bottom: client_h,
            };
            unsafe {
                let _ = AdjustWindowRectEx(
                    &mut rc,
                    WINDOW_STYLE(style),
                    false,
                    WINDOW_EX_STYLE(ex_style),
                );
            }
            let max_w = (work.right - work.left).max(320);
            let max_h = (work.bottom - work.top).max(200);
            let w = (rc.right - rc.left).min(max_w);
            let h = (rc.bottom - rc.top).min(max_h);
            let x = cfg.pos_x.unwrap_or_else(|| work.left + (max_w - w) / 2);
            let y = cfg.pos_y.unwrap_or_else(|| work.top + (max_h - h) / 2);
            (x, y, w, h)
        };

        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                win_w,
                win_h,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        }
    }

    pub fn restore(self) -> Result<()> {
        if !unsafe { IsWindow(Some(self.hwnd)) }.as_bool() {
            return Ok(());
        }
        set_style(self.hwnd, GWL_STYLE, self.orig_style)?;
        set_style(self.hwnd, GWL_EXSTYLE, self.orig_ex_style)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_VISIBLE};

    #[test]
    fn host_window_enters_toggles_and_restores_original_style_and_rect() {
        unsafe {
            let hwnd = CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("cnc-ddraw-host-test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                640,
                480,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let orig_style = GetWindowLongW(hwnd, GWL_STYLE);
            let mut orig_rect = RECT::default();
            GetWindowRect(hwnd, &mut orig_rect).unwrap();

            let cfg = Config {
                width: 800,
                height: 600,
                border: true,
                resizable: true,
                ..Config::default()
            };
            let mut host =
                HostWindow::enter(hwnd, PresentationMode::BorderlessFullscreen, 800, 600, &cfg)
                    .unwrap();
            assert!(host.is_fullscreen());

            let next = host.toggle_mode(800, 600, &cfg).unwrap();
            assert_eq!(next, PresentationMode::Windowed);
            assert!(!host.is_fullscreen());
            let win_style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            assert!(win_style & WS_CAPTION.0 != 0);
            assert!(win_style & WS_THICKFRAME.0 != 0);

            let back = host.toggle_mode(800, 600, &cfg).unwrap();
            assert_eq!(back, PresentationMode::BorderlessFullscreen);

            host.restore().unwrap();
            assert_eq!(GetWindowLongW(hwnd, GWL_STYLE), orig_style);
            let mut restored = RECT::default();
            GetWindowRect(hwnd, &mut restored).unwrap();
            assert_eq!(restored, orig_rect);

            let _ = DestroyWindow(hwnd);
        }
    }
}
