//! HWND geometry queries with Rust-owned output storage.
use windows::core::Result;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{ClientToScreen, MapWindowPoints, ScreenToClient};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsWindow};

pub(crate) fn is_window(hwnd: HWND) -> bool {
    // SAFETY: IsWindow accepts stale or invalid handles.
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

pub(crate) fn client_rect(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect)? };
    Ok(rect)
}

pub(super) fn window_rect(hwnd: HWND) -> Result<RECT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect)? };
    Ok(rect)
}

pub(super) fn client_to_screen(hwnd: HWND, point: &mut POINT) -> bool {
    unsafe { ClientToScreen(hwnd, point).as_bool() }
}

pub(super) fn screen_to_client(hwnd: HWND, point: &mut POINT) -> bool {
    unsafe { ScreenToClient(hwnd, point).as_bool() }
}

pub(super) fn map_rect(from: Option<HWND>, to: Option<HWND>, rect: RECT) -> RECT {
    // Passing exactly two corners preserves MapWindowPoints' mirrored-window RECT handling.
    let mut corners = [
        POINT {
            x: rect.left,
            y: rect.top,
        },
        POINT {
            x: rect.right,
            y: rect.bottom,
        },
    ];
    unsafe { MapWindowPoints(from, to, &mut corners) };
    RECT {
        left: corners[0].x,
        top: corners[0].y,
        right: corners[1].x,
        bottom: corners[1].y,
    }
}
