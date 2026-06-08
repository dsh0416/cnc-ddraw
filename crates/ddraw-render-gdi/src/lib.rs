use ddraw_core::DisplayMode;
use ddraw_render::Frame;
use ddraw_render::{RenderError, Renderer, RendererConfig, RendererKind, Viewport};

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows::Win32::Foundation::HWND;

#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{
    GetDC, ReleaseDC, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, RGBQUAD, SRCCOPY,
};

#[derive(Debug, Clone)]
pub struct GdiRenderer {
    hwnd: Option<isize>,
    mode: DisplayMode,
    viewport: Viewport,
    dib_pixels: Vec<u8>,
    dib_stride: usize,
    palette: [[u8; 4]; 256],
    vsync: bool,
}

impl Default for GdiRenderer {
    fn default() -> Self {
        let mode = DisplayMode::default();
        Self {
            hwnd: None,
            mode,
            viewport: Viewport::from_mode(mode),
            dib_pixels: Vec::new(),
            dib_stride: 0,
            palette: [[0; 4]; 256],
            vsync: false,
        }
    }
}

impl Renderer for GdiRenderer {
    fn kind(&self) -> RendererKind {
        RendererKind::Gdi
    }

    fn init(&mut self, config: RendererConfig) -> Result<(), RenderError> {
        self.hwnd = config.window_handle;
        self.viewport = config.viewport;
        self.vsync = config.vsync;

        #[cfg(windows)]
        if self.hwnd.is_none() {
            return Err(RenderError::MissingWindowHandle);
        }

        Ok(())
    }

    fn resize(&mut self, mode: DisplayMode, viewport: Viewport) -> Result<(), RenderError> {
        self.mode = mode;
        self.viewport = viewport;
        self.dib_stride = dib_stride(mode)?;
        self.dib_pixels
            .resize(self.dib_stride * mode.height as usize, 0);
        Ok(())
    }

    fn upload_palette(&mut self, palette: &[[u8; 4]; 256]) -> Result<(), RenderError> {
        self.palette = *palette;
        Ok(())
    }

    fn upload_frame(&mut self, frame: Frame<'_>) -> Result<(), RenderError> {
        if frame.mode.width == 0 || frame.mode.height == 0 {
            return Err(RenderError::UploadFailed("empty frame"));
        }

        if frame.palette.is_some() {
            if let Some(palette) = frame.palette {
                self.upload_palette(palette)?;
            }
        }

        if frame.mode != self.mode || self.dib_pixels.is_empty() {
            self.resize(frame.mode, Viewport::from_mode(frame.mode))?;
        }

        let bytes_per_pixel = bytes_per_pixel(frame.mode.bpp)?;
        let min_src_pitch = frame.mode.width as usize * bytes_per_pixel;
        if frame.pitch < min_src_pitch {
            return Err(RenderError::UploadFailed(
                "source pitch is smaller than width",
            ));
        }

        let height = frame.mode.height as usize;
        let required_src = frame.pitch.saturating_mul(height.saturating_sub(1)) + min_src_pitch;
        if frame.pixels.len() < required_src {
            return Err(RenderError::UploadFailed(
                "source frame is shorter than pitch",
            ));
        }

        for y in 0..height {
            let src = y * frame.pitch;
            let dst = y * self.dib_stride;
            self.dib_pixels[dst..dst + min_src_pitch]
                .copy_from_slice(&frame.pixels[src..src + min_src_pitch]);
        }

        Ok(())
    }

    fn present(&mut self) -> Result<(), RenderError> {
        self.present_impl()
    }

    fn set_vsync(&mut self, enabled: bool) -> Result<(), RenderError> {
        self.vsync = enabled;
        Ok(())
    }

    fn shutdown(&mut self) {}
}

impl GdiRenderer {
    #[cfg(windows)]
    fn present_impl(&mut self) -> Result<(), RenderError> {
        let hwnd = self.hwnd.ok_or(RenderError::MissingWindowHandle)?;
        if self.dib_pixels.is_empty() {
            return Ok(());
        }

        let info = self.bitmap_info()?;

        // SAFETY: hwnd is supplied by the owning Win32 windowing layer. The DIB
        // pointers reference self-owned buffers and remain valid for the call.
        unsafe {
            let hwnd = HWND(hwnd as *mut c_void);
            let hdc = GetDC(Some(hwnd));
            if hdc.0.is_null() {
                return Err(RenderError::DeviceLost);
            }

            let copied = StretchDIBits(
                hdc,
                self.viewport.x,
                self.viewport.y,
                self.viewport.width as i32,
                self.viewport.height as i32,
                0,
                0,
                self.mode.width as i32,
                self.mode.height as i32,
                Some(self.dib_pixels.as_ptr().cast::<c_void>()),
                (&info as *const BitmapInfo256).cast::<BITMAPINFO>(),
                DIB_RGB_COLORS,
                SRCCOPY,
            );

            ReleaseDC(Some(hwnd), hdc);

            if copied == 0 {
                return Err(RenderError::UploadFailed("StretchDIBits copied no lines"));
            }
        }

        Ok(())
    }

    #[cfg(not(windows))]
    fn present_impl(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn bitmap_info(&self) -> Result<BitmapInfo256, RenderError> {
        let mut colors = [RGBQUAD::default(); 256];
        if self.mode.bpp == 8 {
            for (dst, src) in colors.iter_mut().zip(self.palette) {
                dst.rgbRed = src[0];
                dst.rgbGreen = src[1];
                dst.rgbBlue = src[2];
                dst.rgbReserved = src[3];
            }
        }

        Ok(BitmapInfo256 {
            header: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.mode.width as i32,
                biHeight: -(self.mode.height as i32),
                biPlanes: 1,
                biBitCount: self.mode.bpp as u16,
                biCompression: 0,
                biSizeImage: self.dib_pixels.len() as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: if self.mode.bpp == 8 { 256 } else { 0 },
                biClrImportant: 0,
            },
            colors,
        })
    }
}

#[cfg(windows)]
#[repr(C)]
struct BitmapInfo256 {
    header: BITMAPINFOHEADER,
    colors: [RGBQUAD; 256],
}

fn bytes_per_pixel(bpp: u32) -> Result<usize, RenderError> {
    match bpp {
        8 => Ok(1),
        16 => Ok(2),
        24 => Ok(3),
        32 => Ok(4),
        _ => Err(RenderError::UnsupportedBackend("unsupported GDI bit depth")),
    }
}

fn dib_stride(mode: DisplayMode) -> Result<usize, RenderError> {
    let bits_per_line = mode.width as usize * mode.bpp as usize;
    let stride = bits_per_line.div_ceil(32) * 4;
    if stride == 0 {
        return Err(RenderError::UploadFailed("empty DIB stride"));
    }
    Ok(stride)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_frame_repacks_source_pitch_to_dib_stride() {
        let mut renderer = GdiRenderer::default();
        let mode = DisplayMode {
            width: 3,
            height: 2,
            bpp: 8,
            refresh_rate: 0,
        };
        let pixels = [1, 2, 3, 99, 4, 5, 6, 99];

        renderer
            .upload_frame(Frame {
                mode,
                pitch: 4,
                pixels: &pixels,
                palette: None,
            })
            .unwrap();

        assert_eq!(renderer.dib_stride, 4);
        assert_eq!(&renderer.dib_pixels, &[1, 2, 3, 0, 4, 5, 6, 0]);
    }
}
