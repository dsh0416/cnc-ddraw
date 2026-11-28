//! Owned Win32 overlay window used as the wgpu swapchain surface so underlying Win32 child
//! and popup game dialogs keep receiving WM_PAINT / WM_DRAWITEM unmodified.
#![forbid(unsafe_code)]

use std::num::NonZeroIsize;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_NOTOPMOST, HWND_TOP, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
    SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WM_TIMER, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::win32::current_module_handle;
use crate::win32::geometry::{client_rect, client_to_screen, is_window};
use crate::win32::hwnd::{
    children, class_name, create_window_ex_w, destroy_window, ex_style, foreground_window,
    is_iconic, is_visible, kill_timer, owner, prev_siblings, register_overlay_class, set_timer,
    set_window_pos, SendHwnd,
};

use super::input::{
    is_native_modal_dialog, is_related_dialog_window, is_same_process_window, present_wndproc,
};
use super::{bound_hwnd, clear_present_hwnd, host_callbacks, present_hwnd, set_present_hwnd};

const EMBEDDED_VIDEO_TIMER_ID: usize = 1;

pub struct OverlayWindow {
    owner: SendHwnd,
    hwnd: SendHwnd,
    hinstance: NonZeroIsize,
    last_rect: Option<(i32, i32, u32, u32)>,
    visible: bool,
    watch_video: bool,
}

impl OverlayWindow {
    pub fn new(owner: HWND) -> Result<Self, String> {
        let module =
            current_module_handle().map_err(|err| format!("getting module handle: {err}"))?;
        let hinstance = HINSTANCE(module.0);
        let class_name = w!("CncDdrawWgpuPresent");
        register_overlay_class(class_name, hinstance, overlay_wndproc);

        let (x, y, w, h) = client_screen_rect(owner).unwrap_or((0, 0, 1, 1));
        let hwnd = create_window_ex_w(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!(""),
            WS_POPUP,
            x,
            y,
            w.max(1) as i32,
            h.max(1) as i32,
            Some(owner),
            Some(hinstance),
        )
        .map_err(|err| format!("creating presentation overlay window: {err}"))?;
        set_present_hwnd(hwnd);
        // Ensure neither the owner nor the owned overlay sits in the Topmost band so normal
        // windows can cleanly cover the game after Alt+Tab.
        strip_topmost(owner);
        strip_topmost(hwnd);
        let hinst_nz = NonZeroIsize::new(module.0 as isize)
            .ok_or_else(|| "invalid module handle".to_string())?;
        let watch_video = std::env::current_exe().is_ok_and(|exe| {
            exe.file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("Poq.exe"))
        });
        if watch_video {
            // VLC can start or finish while the game is not submitting DirectDraw frames.
            set_timer(hwnd, EMBEDDED_VIDEO_TIMER_ID, 50);
        }
        Ok(Self {
            owner: SendHwnd(owner),
            hwnd: SendHwnd(hwnd),
            hinstance: hinst_nz,
            last_rect: None,
            visible: false,
            watch_video,
        })
    }

    pub fn raw_handle(&self) -> Result<NonZeroIsize, String> {
        NonZeroIsize::new(self.hwnd.get().0 as isize)
            .ok_or_else(|| "invalid presentation window handle".to_string())
    }

    pub fn hinstance(&self) -> NonZeroIsize {
        self.hinstance
    }

    /// Synchronizes overlay bounds with the owner's client area.
    /// Returns the drawable size when the overlay should be visible.
    pub fn sync_to_owner(&mut self) -> Result<Option<(u32, u32)>, String> {
        let owner = self.owner.get();
        let hwnd = self.hwnd.get();
        let (x, y, target_w, target_h) = client_screen_rect(owner)?;
        if is_iconic(owner)
            || !is_visible(owner)
            || target_w == 0
            || target_h == 0
            || (self.watch_video && embedded_video_visible(owner))
        {
            self.hide();
            return Ok(None);
        }
        let rect = (x, y, target_w, target_h);
        if self.last_rect != Some(rect) || !self.visible {
            let _ = set_window_pos(
                hwnd,
                None,
                rect.0,
                rect.1,
                rect.2 as i32,
                rect.3 as i32,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            );
            self.last_rect = Some(rect);
            self.visible = true;
        }
        keep_present_on_top(owner, hwnd);
        Ok(Some((target_w, target_h)))
    }

    pub fn hide(&mut self) {
        if self.visible {
            let _ = set_window_pos(
                self.hwnd.get(),
                None,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
            );
            self.visible = false;
        }
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        let hwnd = self.hwnd.get();
        clear_present_hwnd(hwnd);
        if is_window(hwnd) {
            if self.watch_video {
                kill_timer(hwnd, EMBEDDED_VIDEO_TIMER_ID);
            }
            destroy_window(hwnd);
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

fn embedded_video_visible(hwnd: HWND) -> bool {
    children(hwnd).any(|child| {
        class_name(child).is_some_and(|name| name.starts_with("VLC video main "))
            && children(child).any(|output| {
                is_visible(output)
                    && class_name(output).is_some_and(|name| name.starts_with("VLC video output "))
            })
    })
}

fn overlay_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg != WM_TIMER || wparam.0 != EMBEDDED_VIDEO_TIMER_ID {
        return present_wndproc(hwnd, msg, wparam, lparam);
    }
    if present_hwnd() == Some(hwnd) {
        if let Some(owner) = owner(hwnd)
            .filter(|&owner| bound_hwnd() == Some(owner) && is_visible(owner) && !is_iconic(owner))
        {
            let show_overlay = !embedded_video_visible(owner);
            if show_overlay != is_visible(hwnd) {
                if let Some(callbacks) = host_callbacks() {
                    callbacks.on_host_window_changed();
                }
            }
        }
    }
    LRESULT(0)
}

fn strip_topmost(hwnd: HWND) {
    if !hwnd.is_invalid() && ex_style(hwnd) as u32 & WS_EX_TOPMOST.0 != 0 {
        let _ = set_window_pos(
            hwnd,
            Some(HWND_NOTOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

/// Keeps the presenter above the game while letting a foreground native modal dialog stay visible.
pub fn keep_present_on_top(owner: HWND, present: HWND) {
    if owner.is_invalid() || !is_visible(present) {
        return;
    }
    strip_topmost(owner);
    strip_topmost(present);
    keep_present_in_front(owner, present, foreground_window());
}

fn keep_present_in_front(owner: HWND, present: HWND, fg: HWND) {
    if is_native_modal_dialog(owner, fg) {
        if !prev_siblings(present).take(512).any(|hwnd| hwnd == fg) {
            let _ = set_window_pos(
                present,
                Some(fg),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
            );
        }
        return;
    }
    if fg != owner && !is_related_dialog_window(owner, fg) && !is_same_process_window(fg) {
        return;
    }
    let mut need_raise = false;
    for prev in prev_siblings(present).take(512) {
        if prev != present
            && is_visible(prev)
            && (prev == owner
                || is_related_dialog_window(owner, prev)
                || is_same_process_window(prev))
        {
            strip_topmost(prev);
            need_raise = true;
        }
    }
    if need_raise {
        let _ = set_window_pos(
            present,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
    }
}

pub(crate) fn ensure_present_on_top_if_active(bound: HWND) {
    if let Some(present) = present_hwnd() {
        keep_present_on_top(bound, present);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, WM_PAINT, WS_CHILD,
        WS_DISABLED, WS_EX_DLGMODALFRAME, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
        WS_POPUP, WS_VISIBLE,
    };

    use crate::win32::hwnd::test_support::{
        consume_paint, invalidate_window, pump_thread_messages, register_test_class,
    };
    use crate::win32::hwnd::{
        create_window_ex_w, def_window_proc_w, destroy_window, prev_siblings, set_window_pos,
    };

    use super::{embedded_video_visible, keep_present_in_front};

    static PAINT_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn count_paint_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_PAINT {
            consume_paint(hwnd);
            PAINT_COUNT.fetch_add(1, Ordering::Relaxed);
            return LRESULT(0);
        }
        def_window_proc_w(hwnd, msg, wparam, lparam)
    }

    #[test]
    fn owned_overlay_above_dialogs_delivers_wm_paint_to_both_child_and_popup() {
        let class_name = w!("CncDdrawTestPaintClass");
        register_test_class(class_name, count_paint_wndproc);

        let parent = create_window_ex_w(
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
        )
        .unwrap();
        let child = create_window_ex_w(
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
        )
        .unwrap();
        let popup = create_window_ex_w(
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
        )
        .unwrap();
        let overlay = create_window_ex_w(
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
        )
        .unwrap();
        let _ = set_window_pos(
            overlay,
            Some(HWND_TOP),
            0,
            0,
            320,
            240,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        PAINT_COUNT.store(0, Ordering::Relaxed);
        invalidate_window(child);
        invalidate_window(popup);
        pump_thread_messages();
        assert!(
            PAINT_COUNT.load(Ordering::Relaxed) >= 2,
            "expected WM_PAINT on both child and popup behind overlay"
        );

        destroy_window(overlay);
        destroy_window(popup);
        destroy_window(parent);
    }

    #[test]
    fn native_modal_stays_above_the_presentation_window() {
        let owner = create_window_ex_w(
            Default::default(),
            w!("STATIC"),
            w!("disabled owner"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_DISABLED,
            0,
            0,
            320,
            240,
            None,
            None,
        )
        .unwrap();
        let create_popup = |ex_style, class| {
            create_window_ex_w(
                ex_style,
                class,
                w!("popup"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                160,
                120,
                Some(owner),
                None,
            )
            .unwrap()
        };
        let present = create_popup(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"));
        let modal = create_popup(WS_EX_DLGMODALFRAME, w!("#32770"));
        let raise = |hwnd| {
            set_window_pos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            )
            .unwrap();
        };
        let is_above = |above, below| prev_siblings(below).any(|hwnd| hwnd == above);

        raise(modal);
        assert!(is_above(modal, present));
        keep_present_in_front(owner, present, modal);
        assert!(is_above(modal, present));

        raise(present);
        assert!(is_above(present, modal));
        keep_present_in_front(owner, present, modal);
        assert!(is_above(modal, present));
        assert!(is_above(present, owner));
        destroy_window(modal);

        let popup = create_popup(Default::default(), w!("STATIC"));
        raise(popup);
        assert!(is_above(popup, present));
        keep_present_in_front(owner, present, popup);
        assert!(is_above(present, popup));
        destroy_window(owner);
    }

    #[test]
    fn embedded_video_requires_a_visible_vlc_output_child() {
        let main_class = w!("VLC video main test");
        let output_class = w!("VLC video output test");
        register_test_class(main_class, count_paint_wndproc);
        register_test_class(output_class, count_paint_wndproc);

        let create = |class_name, style, parent| {
            create_window_ex_w(
                Default::default(),
                class_name,
                w!(""),
                style,
                0,
                0,
                320,
                240,
                parent,
                None,
            )
            .unwrap()
        };
        let owner = create(w!("STATIC"), WS_OVERLAPPEDWINDOW | WS_VISIBLE, None);
        let main = create(main_class, WS_CHILD | WS_VISIBLE, Some(owner));
        create(w!("STATIC"), WS_CHILD | WS_VISIBLE, Some(main));
        assert!(!embedded_video_visible(owner));

        let output = create(output_class, WS_CHILD, Some(main));
        assert!(!embedded_video_visible(owner));
        set_window_pos(
            output,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        )
        .unwrap();
        assert!(embedded_video_visible(owner));

        destroy_window(output);
        assert!(!embedded_video_visible(owner));
        destroy_window(owner);
    }
}
