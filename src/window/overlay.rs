//! Owned Win32 overlay window used as the wgpu swapchain surface so underlying Win32 child
//! and popup game dialogs keep receiving WM_PAINT / WM_DRAWITEM unmodified.
use std::ffi::c_void;
use std::num::NonZeroIsize;
use std::sync::OnceLock;

use super::geometry::{client_rect, client_to_screen, is_window};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, POINT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetForegroundWindow, GetWindow, GetWindowLongW, IsIconic,
    IsWindowVisible, RegisterClassW, SetWindowPos, GWL_EXSTYLE, GW_HWNDPREV, HWND_NOTOPMOST,
    HWND_TOP, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::input::{is_related_dialog_window, is_same_process_window, present_wndproc};
use super::{clear_present_hwnd, present_hwnd_raw, set_present_hwnd};

pub struct OverlayWindow {
    owner: HWND,
    hwnd: HWND,
    hinstance: NonZeroIsize,
    last_rect: Option<(i32, i32, u32, u32)>,
    visible: bool,
}

impl OverlayWindow {
    pub fn new(owner: HWND) -> Result<Self, String> {
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|err| format!("getting module handle: {err}"))?;
        let hinstance = HINSTANCE(module.0);
        let class_name = w!("CncDdrawWgpuPresent");
        static REGISTERED: OnceLock<()> = OnceLock::new();
        REGISTERED.get_or_init(|| {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(present_wndproc),
                hInstance: hinstance,
                lpszClassName: class_name,
                ..Default::default()
            };
            unsafe {
                let _ = RegisterClassW(&wc);
            }
        });

        let (x, y, w, h) = client_screen_rect(owner).unwrap_or((0, 0, 1, 1));
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class_name,
                w!(""),
                WS_POPUP,
                x,
                y,
                w.max(1) as i32,
                h.max(1) as i32,
                Some(owner),
                None,
                Some(hinstance),
                None,
            )
        }
        .map_err(|err| format!("creating presentation overlay window: {err}"))?;
        set_present_hwnd(hwnd);
        let hinst_nz = NonZeroIsize::new(module.0 as isize)
            .ok_or_else(|| "invalid module handle".to_string())?;
        Ok(Self {
            owner,
            hwnd,
            hinstance: hinst_nz,
            last_rect: None,
            visible: false,
        })
    }

    pub fn raw_handle(&self) -> Result<NonZeroIsize, String> {
        NonZeroIsize::new(self.hwnd.0 as isize)
            .ok_or_else(|| "invalid presentation window handle".to_string())
    }

    pub fn hinstance(&self) -> NonZeroIsize {
        self.hinstance
    }

    /// Synchronizes overlay bounds with the owner's client area.
    /// Returns `Some((width, height))` if visible and drawable, or `None` if hidden/minimized.
    pub fn sync_to_owner(&mut self) -> Result<Option<(u32, u32)>, String> {
        let (x, y, target_w, target_h) = client_screen_rect(self.owner)?;
        if unsafe { IsIconic(self.owner) }.as_bool()
            || !unsafe { IsWindowVisible(self.owner) }.as_bool()
            || target_w == 0
            || target_h == 0
        {
            self.hide();
            return Ok(None);
        }
        let rect = (x, y, target_w, target_h);
        if self.last_rect != Some(rect) || !self.visible {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    rect.0,
                    rect.1,
                    rect.2 as i32,
                    rect.3 as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                );
            }
            self.last_rect = Some(rect);
            self.visible = true;
        }
        keep_present_on_top(self.owner, self.hwnd);
        Ok(Some((target_w, target_h)))
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
                );
            }
            self.visible = false;
        }
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        clear_present_hwnd(self.hwnd);
        if is_window(self.hwnd) {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

pub fn client_screen_rect(hwnd: HWND) -> Result<(i32, i32, u32, u32), String> {
    let client = client_rect(hwnd).map_err(|err| format!("getting client area: {err}"))?;
    let mut origin = POINT { x: 0, y: 0 };
    client_to_screen(hwnd, &mut origin);
    Ok((
        origin.x,
        origin.y,
        client.right.max(0) as u32,
        client.bottom.max(0) as u32,
    ))
}

/// Checks if any visible game window/dialog/dropdown/scrollbar is currently above `present`
/// in Z-order while the game owns foreground focus, strips `WS_EX_TOPMOST` if needed, and raises
/// `present` above it.
pub fn keep_present_on_top(owner: HWND, present: HWND) {
    if present.is_invalid() || owner.is_invalid() {
        return;
    }
    let fg = unsafe { GetForegroundWindow() };
    if fg != owner && !is_related_dialog_window(owner, fg) && !is_same_process_window(fg) {
        return;
    }
    let mut prev = unsafe { GetWindow(present, GW_HWNDPREV) }.unwrap_or_default();
    let mut steps = 0;
    let mut need_raise = false;
    while !prev.is_invalid() && steps < 512 {
        steps += 1;
        let next = unsafe { GetWindow(prev, GW_HWNDPREV) }.unwrap_or_default();
        if prev != present
            && unsafe { IsWindowVisible(prev) }.as_bool()
            && (prev == owner
                || is_related_dialog_window(owner, prev)
                || is_same_process_window(prev))
        {
            let ex = unsafe { GetWindowLongW(prev, GWL_EXSTYLE) } as u32;
            if ex & WS_EX_TOPMOST.0 != 0 {
                unsafe {
                    let _ = SetWindowPos(
                        prev,
                        Some(HWND_NOTOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                    );
                }
            }
            need_raise = true;
        }
        prev = next;
    }
    if need_raise {
        unsafe {
            let _ = SetWindowPos(
                present,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
        }
    }
}

pub(crate) fn ensure_present_on_top_if_active(bound: HWND) {
    let raw = present_hwnd_raw();
    if raw != 0 {
        keep_present_on_top(bound, HWND(raw as *mut c_void));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassW, SetWindowPos, HWND_TOP, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        WM_PAINT, WNDCLASSW, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
        WS_POPUP, WS_VISIBLE,
    };

    static PAINT_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn count_paint_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_PAINT {
            let mut ps = PAINTSTRUCT::default();
            let _ = unsafe { BeginPaint(hwnd, &mut ps) };
            let _ = unsafe { EndPaint(hwnd, &ps) };
            PAINT_COUNT.fetch_add(1, Ordering::Relaxed);
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    #[test]
    fn owned_overlay_above_dialogs_delivers_wm_paint_to_both_child_and_popup() {
        unsafe {
            let class_name = w!("CncDdrawTestPaintClass");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(count_paint_wndproc),
                lpszClassName: class_name,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);

            let parent = CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!("parent"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                0,
                0,
                320,
                240,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let child = CreateWindowExW(
                Default::default(),
                class_name,
                w!("child"),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                160,
                120,
                Some(parent),
                None,
                None,
                None,
            )
            .unwrap();
            let popup = CreateWindowExW(
                Default::default(),
                class_name,
                w!("popup"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                160,
                120,
                Some(parent),
                None,
                None,
                None,
            )
            .unwrap();
            let overlay = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("overlay"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                320,
                240,
                Some(parent),
                None,
                None,
                None,
            )
            .unwrap();
            let _ = SetWindowPos(
                overlay,
                Some(HWND_TOP),
                0,
                0,
                320,
                240,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );

            PAINT_COUNT.store(0, Ordering::Relaxed);
            let _ = InvalidateRect(Some(child), None, false);
            let _ = InvalidateRect(Some(popup), None, false);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
            assert!(
                PAINT_COUNT.load(Ordering::Relaxed) >= 2,
                "expected WM_PAINT on both child and popup behind overlay"
            );

            let _ = DestroyWindow(overlay);
            let _ = DestroyWindow(popup);
            let _ = DestroyWindow(parent);
        }
    }
}
