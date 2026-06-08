use ddraw_core::DisplayMode;
use ddraw_render::{
    repack_frame_to_bgra, Frame, RenderError, Renderer, RendererConfig, RendererKind, Viewport,
};

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows::Win32::Foundation::HWND;

#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC, HDC};

#[cfg(windows)]
use windows::Win32::Graphics::OpenGL::{
    glClear, glDisable, glDrawPixels, glLoadIdentity, glMatrixMode, glOrtho, glPixelStorei,
    glPixelZoom, glRasterPos2i, glViewport, wglCreateContext, wglDeleteContext, wglMakeCurrent,
    ChoosePixelFormat, SetPixelFormat, GL_BGRA_EXT, GL_COLOR_BUFFER_BIT, GL_DEPTH_TEST,
    GL_MODELVIEW, GL_PROJECTION, GL_TEXTURE_2D, GL_UNPACK_ALIGNMENT, GL_UNSIGNED_BYTE, HGLRC,
    PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_MAIN_PLANE, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
    PIXELFORMATDESCRIPTOR,
};

#[derive(Debug)]
pub struct OpenGlRenderer {
    hwnd: Option<isize>,
    mode: DisplayMode,
    viewport: Viewport,
    bgra_pixels: Vec<u8>,
    bgra_stride: usize,
    vsync: bool,
    #[cfg(windows)]
    hdc: Option<HDC>,
    #[cfg(windows)]
    context: Option<HGLRC>,
}

// SAFETY: The OpenGL context is made current only during &mut self methods. The
// renderer may move to a render thread but is not accessed concurrently.
unsafe impl Send for OpenGlRenderer {}

impl Default for OpenGlRenderer {
    fn default() -> Self {
        let mode = DisplayMode::default();
        Self {
            hwnd: None,
            mode,
            viewport: Viewport::from_mode(mode),
            bgra_pixels: Vec::new(),
            bgra_stride: 0,
            vsync: false,
            #[cfg(windows)]
            hdc: None,
            #[cfg(windows)]
            context: None,
        }
    }
}

impl Renderer for OpenGlRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::OpenGl
    }

    fn init(&mut self, config: RendererConfig) -> Result<(), RenderError> {
        self.hwnd = config.window_handle;
        self.viewport = config.viewport;
        self.vsync = config.vsync;
        self.init_impl()
    }

    fn resize(&mut self, mode: DisplayMode, viewport: Viewport) -> Result<(), RenderError> {
        self.mode = mode;
        self.viewport = viewport;
        self.bgra_stride = mode.width as usize * 4;
        self.bgra_pixels
            .resize(self.bgra_stride * mode.height as usize, 0);
        Ok(())
    }

    fn upload_palette(&mut self, _palette: &[[u8; 4]; 256]) -> Result<(), RenderError> {
        Ok(())
    }

    fn upload_frame(&mut self, frame: Frame<'_>) -> Result<(), RenderError> {
        if frame.mode != self.mode || self.bgra_pixels.is_empty() {
            self.resize(frame.mode, Viewport::from_mode(frame.mode))?;
        }

        self.bgra_stride = repack_frame_to_bgra(frame, &mut self.bgra_pixels)?;
        Ok(())
    }

    fn present(&mut self) -> Result<(), RenderError> {
        self.present_impl()
    }

    fn set_vsync(&mut self, enabled: bool) -> Result<(), RenderError> {
        self.vsync = enabled;
        Ok(())
    }

    fn shutdown(&mut self) {
        self.shutdown_impl();
    }
}

impl OpenGlRenderer {
    #[cfg(windows)]
    fn init_impl(&mut self) -> Result<(), RenderError> {
        let hwnd = self.hwnd.ok_or(RenderError::MissingWindowHandle)?;
        let hwnd = HWND(hwnd as *mut c_void);

        // SAFETY: hwnd belongs to the windowing layer and stays valid while the
        // renderer owns the context.
        let hdc = unsafe { GetDC(Some(hwnd)) };
        if hdc.0.is_null() {
            return Err(RenderError::DeviceLost);
        }

        let pfd = PIXELFORMATDESCRIPTOR {
            nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
            nVersion: 1,
            dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
            iPixelType: PFD_TYPE_RGBA,
            cColorBits: 32,
            cAlphaBits: 8,
            iLayerType: PFD_MAIN_PLANE.0 as u8,
            ..PIXELFORMATDESCRIPTOR::default()
        };

        // SAFETY: hdc is a valid window DC. Pixel format descriptor is fully
        // initialized and lives across both calls.
        unsafe {
            let format = ChoosePixelFormat(hdc, &pfd);
            if format == 0 {
                ReleaseDC(Some(hwnd), hdc);
                return Err(RenderError::UnsupportedBackend("ChoosePixelFormat failed"));
            }
            SetPixelFormat(hdc, format, &pfd).map_err(|_| {
                ReleaseDC(Some(hwnd), hdc);
                RenderError::UnsupportedBackend("SetPixelFormat failed")
            })?;

            let context = wglCreateContext(hdc).map_err(|_| {
                ReleaseDC(Some(hwnd), hdc);
                RenderError::UnsupportedBackend("wglCreateContext failed")
            })?;
            wglMakeCurrent(hdc, context).map_err(|_| {
                let _ = wglDeleteContext(context);
                ReleaseDC(Some(hwnd), hdc);
                RenderError::UnsupportedBackend("wglMakeCurrent failed")
            })?;

            glDisable(GL_DEPTH_TEST);
            glDisable(GL_TEXTURE_2D);
            glPixelStorei(GL_UNPACK_ALIGNMENT, 4);
            wglMakeCurrent(HDC(std::ptr::null_mut()), HGLRC(std::ptr::null_mut()))
                .map_err(|_| RenderError::DeviceLost)?;

            self.hdc = Some(hdc);
            self.context = Some(context);
        }

        Ok(())
    }

    #[cfg(not(windows))]
    fn init_impl(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn present_impl(&mut self) -> Result<(), RenderError> {
        let (Some(hdc), Some(context)) = (self.hdc, self.context) else {
            return Err(RenderError::DeviceLost);
        };
        if self.bgra_pixels.is_empty() {
            return Ok(());
        }

        // SAFETY: The WGL context was created for this DC. Pixel data is owned by
        // self and remains valid for glDrawPixels.
        unsafe {
            wglMakeCurrent(hdc, context).map_err(|_| RenderError::DeviceLost)?;
            glViewport(
                self.viewport.x,
                self.viewport.y,
                self.viewport.width as i32,
                self.viewport.height as i32,
            );
            glMatrixMode(GL_PROJECTION);
            glLoadIdentity();
            glOrtho(
                0.0,
                self.viewport.width as f64,
                0.0,
                self.viewport.height as f64,
                -1.0,
                1.0,
            );
            glMatrixMode(GL_MODELVIEW);
            glLoadIdentity();
            glClear(GL_COLOR_BUFFER_BIT);
            glPixelZoom(
                self.viewport.width as f32 / self.mode.width as f32,
                -(self.viewport.height as f32 / self.mode.height as f32),
            );
            glRasterPos2i(0, self.viewport.height as i32);
            glDrawPixels(
                self.mode.width as i32,
                self.mode.height as i32,
                GL_BGRA_EXT,
                GL_UNSIGNED_BYTE,
                self.bgra_pixels.as_ptr().cast(),
            );
            windows::Win32::Graphics::OpenGL::SwapBuffers(hdc)
                .map_err(|_| RenderError::DeviceLost)?;
            wglMakeCurrent(HDC(std::ptr::null_mut()), HGLRC(std::ptr::null_mut()))
                .map_err(|_| RenderError::DeviceLost)?;
        }

        Ok(())
    }

    #[cfg(not(windows))]
    fn present_impl(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn shutdown_impl(&mut self) {
        // SAFETY: The context/DC were acquired by init_impl and are released once
        // here when the renderer shuts down.
        unsafe {
            let _ = wglMakeCurrent(HDC(std::ptr::null_mut()), HGLRC(std::ptr::null_mut()));
            if let Some(context) = self.context.take() {
                let _ = wglDeleteContext(context);
            }
            if let (Some(hwnd), Some(hdc)) = (self.hwnd, self.hdc.take()) {
                let _ = ReleaseDC(Some(HWND(hwnd as *mut c_void)), hdc);
            }
        }
    }

    #[cfg(not(windows))]
    fn shutdown_impl(&mut self) {}
}
