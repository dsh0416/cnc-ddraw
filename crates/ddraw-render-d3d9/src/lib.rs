use ddraw_core::DisplayMode;
use ddraw_render::{
    repack_frame_to_bgra, Frame, RenderError, Renderer, RendererConfig, RendererKind, Viewport,
};

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows::core::BOOL;

#[cfg(windows)]
use windows::Win32::Foundation::HWND;

#[cfg(windows)]
use windows::Win32::Graphics::Direct3D9::{
    Direct3DCreate9, IDirect3D9, IDirect3DDevice9, IDirect3DTexture9, D3DADAPTER_DEFAULT,
    D3DCLEAR_TARGET, D3DCREATE_FPU_PRESERVE, D3DCREATE_MULTITHREADED,
    D3DCREATE_SOFTWARE_VERTEXPROCESSING, D3DDEVTYPE_HAL, D3DFMT_A8R8G8B8, D3DFMT_X8R8G8B8,
    D3DFVF_TEX1, D3DFVF_XYZRHW, D3DLOCKED_RECT, D3DPOOL_MANAGED, D3DPRESENT_INTERVAL_IMMEDIATE,
    D3DPRESENT_INTERVAL_ONE, D3DPRESENT_PARAMETERS, D3DPT_TRIANGLESTRIP, D3DSAMP_MAGFILTER,
    D3DSAMP_MINFILTER, D3DSWAPEFFECT_DISCARD, D3DTA_TEXTURE, D3DTEXF_POINT, D3DTOP_SELECTARG1,
    D3DTSS_ALPHAARG1, D3DTSS_ALPHAOP, D3DTSS_COLORARG1, D3DTSS_COLOROP, D3D_SDK_VERSION,
};

#[derive(Debug)]
pub struct D3d9Renderer {
    hwnd: Option<isize>,
    mode: DisplayMode,
    viewport: Viewport,
    vsync: bool,
    bgra_pixels: Vec<u8>,
    bgra_stride: usize,
    #[cfg(windows)]
    d3d: Option<IDirect3D9>,
    #[cfg(windows)]
    device: Option<IDirect3DDevice9>,
    #[cfg(windows)]
    texture: Option<IDirect3DTexture9>,
}

// SAFETY: The D3D9 device is created with D3DCREATE_MULTITHREADED, and all
// renderer state is accessed through &mut self. The renderer may move to a
// render thread, but it is not shared concurrently.
unsafe impl Send for D3d9Renderer {}

impl Default for D3d9Renderer {
    fn default() -> Self {
        let mode = DisplayMode::default();
        Self {
            hwnd: None,
            mode,
            viewport: Viewport::from_mode(mode),
            vsync: false,
            bgra_pixels: Vec::new(),
            bgra_stride: 0,
            #[cfg(windows)]
            d3d: None,
            #[cfg(windows)]
            device: None,
            #[cfg(windows)]
            texture: None,
        }
    }
}

impl Renderer for D3d9Renderer {
    fn kind(&self) -> RendererKind {
        RendererKind::D3d9
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
        self.create_texture()
    }

    fn upload_palette(&mut self, _palette: &[[u8; 4]; 256]) -> Result<(), RenderError> {
        Ok(())
    }

    fn upload_frame(&mut self, frame: Frame<'_>) -> Result<(), RenderError> {
        if frame.mode != self.mode || self.bgra_pixels.is_empty() {
            self.resize(frame.mode, Viewport::from_mode(frame.mode))?;
        }

        self.bgra_stride = repack_frame_to_bgra(frame, &mut self.bgra_pixels)?;
        self.upload_texture()
    }

    fn present(&mut self) -> Result<(), RenderError> {
        self.present_impl()
    }

    fn set_vsync(&mut self, enabled: bool) -> Result<(), RenderError> {
        self.vsync = enabled;
        Ok(())
    }

    fn shutdown(&mut self) {
        #[cfg(windows)]
        {
            self.texture = None;
            self.device = None;
            self.d3d = None;
        }
    }
}

impl D3d9Renderer {
    #[cfg(windows)]
    fn init_impl(&mut self) -> Result<(), RenderError> {
        let hwnd = self.hwnd.ok_or(RenderError::MissingWindowHandle)?;
        let hwnd = HWND(hwnd as *mut c_void);

        // SAFETY: Direct3DCreate9 has no pointer arguments and returns None when
        // d3d9.dll is unavailable.
        let d3d = unsafe { Direct3DCreate9(D3D_SDK_VERSION) }
            .ok_or(RenderError::UnsupportedBackend("Direct3DCreate9 failed"))?;

        let mut params = D3DPRESENT_PARAMETERS {
            BackBufferWidth: 0,
            BackBufferHeight: 0,
            BackBufferFormat: D3DFMT_X8R8G8B8,
            BackBufferCount: 1,
            SwapEffect: D3DSWAPEFFECT_DISCARD,
            hDeviceWindow: hwnd,
            Windowed: BOOL(1),
            PresentationInterval: if self.vsync {
                D3DPRESENT_INTERVAL_ONE as u32
            } else {
                D3DPRESENT_INTERVAL_IMMEDIATE as u32
            },
            ..D3DPRESENT_PARAMETERS::default()
        };

        let behavior = (D3DCREATE_MULTITHREADED
            | D3DCREATE_SOFTWARE_VERTEXPROCESSING
            | D3DCREATE_FPU_PRESERVE) as u32;
        let mut device = None;

        // SAFETY: hwnd belongs to the target window; params is initialized and
        // the output Option is written by D3D9.
        unsafe {
            d3d.CreateDevice(
                D3DADAPTER_DEFAULT,
                D3DDEVTYPE_HAL,
                hwnd,
                behavior,
                &mut params,
                &mut device,
            )
            .map_err(|_| RenderError::UnsupportedBackend("IDirect3D9::CreateDevice failed"))?;
        }

        self.d3d = Some(d3d);
        self.device = device;
        self.apply_fixed_function_state()?;
        self.create_texture()
    }

    #[cfg(not(windows))]
    fn init_impl(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn apply_fixed_function_state(&self) -> Result<(), RenderError> {
        let Some(device) = &self.device else {
            return Ok(());
        };

        // SAFETY: These are value-only fixed-function state updates on a live
        // D3D9 device.
        unsafe {
            device
                .SetFVF(D3DFVF_XYZRHW | D3DFVF_TEX1)
                .map_err(|_| RenderError::UploadFailed("SetFVF failed"))?;
            device
                .SetTextureStageState(0, D3DTSS_COLOROP, D3DTOP_SELECTARG1.0 as u32)
                .map_err(|_| RenderError::UploadFailed("SetTextureStageState color op failed"))?;
            device
                .SetTextureStageState(0, D3DTSS_COLORARG1, D3DTA_TEXTURE)
                .map_err(|_| RenderError::UploadFailed("SetTextureStageState color arg failed"))?;
            device
                .SetTextureStageState(0, D3DTSS_ALPHAOP, D3DTOP_SELECTARG1.0 as u32)
                .map_err(|_| RenderError::UploadFailed("SetTextureStageState alpha op failed"))?;
            device
                .SetTextureStageState(0, D3DTSS_ALPHAARG1, D3DTA_TEXTURE)
                .map_err(|_| RenderError::UploadFailed("SetTextureStageState alpha arg failed"))?;
            device
                .SetSamplerState(0, D3DSAMP_MINFILTER, D3DTEXF_POINT.0 as u32)
                .map_err(|_| RenderError::UploadFailed("SetSamplerState min failed"))?;
            device
                .SetSamplerState(0, D3DSAMP_MAGFILTER, D3DTEXF_POINT.0 as u32)
                .map_err(|_| RenderError::UploadFailed("SetSamplerState mag failed"))?;
        }

        Ok(())
    }

    #[cfg(not(windows))]
    fn create_texture(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn create_texture(&mut self) -> Result<(), RenderError> {
        let Some(device) = &self.device else {
            return Ok(());
        };
        if self.mode.width == 0 || self.mode.height == 0 {
            return Ok(());
        }

        let mut texture = None;
        // SAFETY: The output Option is written by D3D9. Texture dimensions come
        // from the renderer mode and are non-zero here.
        unsafe {
            device
                .CreateTexture(
                    self.mode.width,
                    self.mode.height,
                    1,
                    0,
                    D3DFMT_A8R8G8B8,
                    D3DPOOL_MANAGED,
                    &mut texture,
                    std::ptr::null_mut(),
                )
                .map_err(|_| RenderError::UploadFailed("CreateTexture failed"))?;
        }

        self.texture = texture;
        Ok(())
    }

    #[cfg(not(windows))]
    fn upload_texture(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn upload_texture(&mut self) -> Result<(), RenderError> {
        let Some(texture) = &self.texture else {
            return Ok(());
        };
        if self.bgra_pixels.is_empty() {
            return Ok(());
        }

        let mut locked = D3DLOCKED_RECT::default();
        // SAFETY: The texture is a live managed A8R8G8B8 texture. Copy respects
        // the pitch returned by D3D9 and unlocks before returning.
        unsafe {
            texture
                .LockRect(0, &mut locked, std::ptr::null(), 0)
                .map_err(|_| RenderError::UploadFailed("texture LockRect failed"))?;

            let dst_pitch = locked.Pitch as usize;
            let dst = locked.pBits as *mut u8;
            for y in 0..self.mode.height as usize {
                let src = y * self.bgra_stride;
                let row_dst = dst.add(y * dst_pitch);
                std::ptr::copy_nonoverlapping(
                    self.bgra_pixels[src..src + self.bgra_stride].as_ptr(),
                    row_dst,
                    self.bgra_stride,
                );
            }

            texture
                .UnlockRect(0)
                .map_err(|_| RenderError::UploadFailed("texture UnlockRect failed"))?;
        }

        Ok(())
    }

    #[cfg(not(windows))]
    fn present_impl(&mut self) -> Result<(), RenderError> {
        Ok(())
    }

    #[cfg(windows)]
    fn present_impl(&mut self) -> Result<(), RenderError> {
        let Some(device) = &self.device else {
            return Err(RenderError::DeviceLost);
        };
        let Some(texture) = &self.texture else {
            return Ok(());
        };

        let vertices = textured_quad(self.viewport);

        // SAFETY: Device and texture are live COM objects owned by this renderer.
        // Vertex data is stack-allocated and valid for DrawPrimitiveUP.
        unsafe {
            device
                .Clear(0, std::ptr::null(), D3DCLEAR_TARGET as u32, 0, 1.0, 0)
                .map_err(|_| RenderError::DeviceLost)?;
            device.BeginScene().map_err(|_| RenderError::DeviceLost)?;
            device
                .SetTexture(0, texture)
                .map_err(|_| RenderError::DeviceLost)?;
            device
                .DrawPrimitiveUP(
                    D3DPT_TRIANGLESTRIP,
                    2,
                    vertices.as_ptr().cast(),
                    std::mem::size_of::<TexturedVertex>() as u32,
                )
                .map_err(|_| RenderError::DeviceLost)?;
            device.EndScene().map_err(|_| RenderError::DeviceLost)?;
            device
                .Present(
                    std::ptr::null(),
                    std::ptr::null(),
                    HWND(std::ptr::null_mut()),
                    std::ptr::null(),
                )
                .map_err(|_| RenderError::DeviceLost)?;
        }

        Ok(())
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct TexturedVertex {
    x: f32,
    y: f32,
    z: f32,
    rhw: f32,
    u: f32,
    v: f32,
}

#[cfg(windows)]
fn textured_quad(viewport: Viewport) -> [TexturedVertex; 4] {
    let left = viewport.x as f32 - 0.5;
    let top = viewport.y as f32 - 0.5;
    let right = (viewport.x + viewport.width as i32) as f32 - 0.5;
    let bottom = (viewport.y + viewport.height as i32) as f32 - 0.5;

    [
        TexturedVertex {
            x: left,
            y: top,
            z: 0.0,
            rhw: 1.0,
            u: 0.0,
            v: 0.0,
        },
        TexturedVertex {
            x: right,
            y: top,
            z: 0.0,
            rhw: 1.0,
            u: 1.0,
            v: 0.0,
        },
        TexturedVertex {
            x: left,
            y: bottom,
            z: 0.0,
            rhw: 1.0,
            u: 0.0,
            v: 1.0,
        },
        TexturedVertex {
            x: right,
            y: bottom,
            z: 0.0,
            rhw: 1.0,
            u: 1.0,
            v: 1.0,
        },
    ]
}
