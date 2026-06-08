//! Renderer abstraction shared by legacy and future backends.

use ddraw_core::DisplayMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Viewport {
    pub fn from_mode(mode: DisplayMode) -> Self {
        Self {
            x: 0,
            y: 0,
            width: mode.width,
            height: mode.height,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererKind {
    Auto,
    Gdi,
    OpenGl,
    D3d9,
    D3d11,
    D3d12,
    Vulkan,
}

impl RendererKind {
    pub fn from_legacy_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Self::Auto,
            "gdi" => Self::Gdi,
            "opengl" | "openglcore" | "openglshader" => Self::OpenGl,
            "direct3d9" | "direct3d9on12" | "d3d9" => Self::D3d9,
            "direct3d11" | "d3d11" => Self::D3d11,
            "direct3d12" | "d3d12" => Self::D3d12,
            "vulkan" => Self::Vulkan,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererConfig {
    pub kind: RendererKind,
    pub vsync: bool,
    pub viewport: Viewport,
    pub window_handle: Option<isize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub mode: DisplayMode,
    pub pitch: usize,
    pub pixels: &'a [u8],
    pub palette: Option<&'a [[u8; 4]; 256]>,
}

pub trait Renderer {
    fn kind(&self) -> RendererKind;
    fn init(&mut self, config: RendererConfig) -> Result<(), RenderError>;
    fn resize(&mut self, mode: DisplayMode, viewport: Viewport) -> Result<(), RenderError>;
    fn upload_palette(&mut self, palette: &[[u8; 4]; 256]) -> Result<(), RenderError>;
    fn upload_frame(&mut self, frame: Frame<'_>) -> Result<(), RenderError>;
    fn present(&mut self) -> Result<(), RenderError>;
    fn set_vsync(&mut self, enabled: bool) -> Result<(), RenderError>;
    fn shutdown(&mut self);
}

pub fn repack_frame_to_bgra(frame: Frame<'_>, out: &mut Vec<u8>) -> Result<usize, RenderError> {
    let bytes_per_pixel = match frame.mode.bpp {
        8 => 1,
        16 => 2,
        24 => 3,
        32 => 4,
        _ => {
            return Err(RenderError::UnsupportedBackend(
                "unsupported source bit depth",
            ))
        }
    };
    let row_bytes = frame.mode.width as usize * bytes_per_pixel;
    if frame.pitch < row_bytes {
        return Err(RenderError::UploadFailed(
            "source pitch is smaller than width",
        ));
    }

    let height = frame.mode.height as usize;
    let required_src = frame.pitch.saturating_mul(height.saturating_sub(1)) + row_bytes;
    if frame.pixels.len() < required_src {
        return Err(RenderError::UploadFailed(
            "source frame is shorter than pitch",
        ));
    }

    let width = frame.mode.width as usize;
    out.resize(width * height * 4, 0);

    for y in 0..height {
        let src_row = y * frame.pitch;
        let dst_row = y * width * 4;
        match frame.mode.bpp {
            8 => {
                let palette = frame
                    .palette
                    .ok_or(RenderError::UploadFailed("8bpp frame is missing palette"))?;
                for x in 0..width {
                    let entry = palette[frame.pixels[src_row + x] as usize];
                    let dst = dst_row + x * 4;
                    out[dst] = entry[2];
                    out[dst + 1] = entry[1];
                    out[dst + 2] = entry[0];
                    out[dst + 3] = 0xFF;
                }
            }
            16 => {
                for x in 0..width {
                    let src = src_row + x * 2;
                    let pixel = u16::from_le_bytes([frame.pixels[src], frame.pixels[src + 1]]);
                    let r = ((pixel >> 11) & 0x1F) as u8;
                    let g = ((pixel >> 5) & 0x3F) as u8;
                    let b = (pixel & 0x1F) as u8;
                    let dst = dst_row + x * 4;
                    out[dst] = (b << 3) | (b >> 2);
                    out[dst + 1] = (g << 2) | (g >> 4);
                    out[dst + 2] = (r << 3) | (r >> 2);
                    out[dst + 3] = 0xFF;
                }
            }
            24 => {
                for x in 0..width {
                    let src = src_row + x * 3;
                    let dst = dst_row + x * 4;
                    out[dst] = frame.pixels[src];
                    out[dst + 1] = frame.pixels[src + 1];
                    out[dst + 2] = frame.pixels[src + 2];
                    out[dst + 3] = 0xFF;
                }
            }
            32 => {
                for x in 0..width {
                    let src = src_row + x * 4;
                    let dst = dst_row + x * 4;
                    out[dst..dst + 4].copy_from_slice(&frame.pixels[src..src + 4]);
                    if out[dst + 3] == 0 {
                        out[dst + 3] = 0xFF;
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    Ok(width * 4)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    UnsupportedBackend(&'static str),
    MissingWindowHandle,
    DeviceLost,
    UploadFailed(&'static str),
}

#[derive(Debug, Clone)]
pub struct NullRenderer {
    kind: RendererKind,
    last_mode: DisplayMode,
    vsync: bool,
    uploaded_bytes: usize,
    palette_uploaded: bool,
}

impl NullRenderer {
    pub fn new(kind: RendererKind) -> Self {
        Self {
            kind,
            last_mode: DisplayMode::default(),
            vsync: false,
            uploaded_bytes: 0,
            palette_uploaded: false,
        }
    }

    pub fn uploaded_bytes(&self) -> usize {
        self.uploaded_bytes
    }

    pub fn palette_uploaded(&self) -> bool {
        self.palette_uploaded
    }
}

impl Renderer for NullRenderer {
    fn kind(&self) -> RendererKind {
        self.kind
    }

    fn init(&mut self, config: RendererConfig) -> Result<(), RenderError> {
        self.vsync = config.vsync;
        Ok(())
    }

    fn resize(&mut self, mode: DisplayMode, _viewport: Viewport) -> Result<(), RenderError> {
        self.last_mode = mode;
        Ok(())
    }

    fn upload_palette(&mut self, _palette: &[[u8; 4]; 256]) -> Result<(), RenderError> {
        self.palette_uploaded = true;
        Ok(())
    }

    fn upload_frame(&mut self, frame: Frame<'_>) -> Result<(), RenderError> {
        self.last_mode = frame.mode;
        self.uploaded_bytes = frame.pixels.len();
        Ok(())
    }

    fn present(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    fn set_vsync(&mut self, enabled: bool) -> Result<(), RenderError> {
        self.vsync = enabled;
        Ok(())
    }

    fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_legacy_renderer_names() {
        assert_eq!(RendererKind::from_legacy_name("auto"), RendererKind::Auto);
        assert_eq!(RendererKind::from_legacy_name("gdi"), RendererKind::Gdi);
        assert_eq!(
            RendererKind::from_legacy_name("openglcore"),
            RendererKind::OpenGl
        );
        assert_eq!(
            RendererKind::from_legacy_name("direct3d9on12"),
            RendererKind::D3d9
        );
        assert_eq!(RendererKind::from_legacy_name("d3d11"), RendererKind::D3d11);
        assert_eq!(RendererKind::from_legacy_name("d3d12"), RendererKind::D3d12);
        assert_eq!(
            RendererKind::from_legacy_name("vulkan"),
            RendererKind::Vulkan
        );
    }

    #[test]
    fn repacks_565_frame_to_bgra() {
        let mode = DisplayMode {
            width: 2,
            height: 1,
            bpp: 16,
            refresh_rate: 0,
        };
        let pixels = [0x00, 0xF8, 0xE0, 0x07];
        let mut out = Vec::new();

        let stride = repack_frame_to_bgra(
            Frame {
                mode,
                pitch: 4,
                pixels: &pixels,
                palette: None,
            },
            &mut out,
        )
        .unwrap();

        assert_eq!(stride, 8);
        assert_eq!(&out[..4], &[0, 0, 255, 255]);
        assert_eq!(&out[4..8], &[0, 255, 0, 255]);
    }
}
