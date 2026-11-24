//! Pure wgpu presentation backend. All Win32 overlay lifecycle and Z-order management lives
//! in `crate::window::OverlayWindow`.
use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use raw_window_handle::{Win32WindowHandle, WindowsDisplayHandle};
use windows::Win32::Foundation::HWND;

use crate::compat;
use crate::config::{self, RendererBackend};
use crate::window::{compute_viewport, OverlayWindow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    Presented,
    Hidden,
    Timeout,
}

#[derive(Debug)]
pub enum PresentError {
    Recoverable(String),
    DeviceLost(String),
    Fatal(String),
}

impl std::fmt::Display for PresentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recoverable(m) | Self::DeviceLost(m) | Self::Fatal(m) => f.write_str(m),
        }
    }
}

struct UploadedFrame {
    texture: wgpu::Texture,
    bindings: wgpu::BindGroup,
}

impl UploadedFrame {
    fn new(
        device: &wgpu::Device,
        pipeline: &wgpu::RenderPipeline,
        target_format: wgpu::TextureFormat,
        size: wgpu::Extent3d,
    ) -> Self {
        // Match the transfer function of the target to preserve the game's byte colors.
        let format = if target_format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw primary pixels"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("DirectDraw frame bindings"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        UploadedFrame { texture, bindings }
    }
}

fn instance_descriptor() -> wgpu::InstanceDescriptor {
    // Allow WGPU_BACKEND env var to take precedence for testing/debugging.
    if std::env::var_os("WGPU_BACKEND").is_some() {
        return wgpu::InstanceDescriptor::from_env_or_default();
    }
    let backends = match config::get().renderer {
        RendererBackend::Dx12 => wgpu::Backends::DX12,
        RendererBackend::Vulkan => wgpu::Backends::VULKAN,
        RendererBackend::Auto => wgpu::Backends::PRIMARY,
    };
    wgpu::InstanceDescriptor {
        backends,
        ..Default::default()
    }
}

fn pick_present_mode(caps: &wgpu::SurfaceCapabilities, vsync: bool) -> wgpu::PresentMode {
    if vsync {
        return wgpu::PresentMode::Fifo;
    }
    if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
        wgpu::PresentMode::Immediate
    } else if caps.present_modes.contains(&wgpu::PresentMode::FifoRelaxed) {
        wgpu::PresentMode::FifoRelaxed
    } else {
        wgpu::PresentMode::Fifo
    }
}

pub struct Renderer {
    // Keep wgpu surface/device ahead of `overlay` so the swapchain drops before DestroyWindow.
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    device_lost: Arc<AtomicBool>,
    config: wgpu::SurfaceConfiguration,
    configured: bool,
    pipeline: wgpu::RenderPipeline,
    frame: Option<UploadedFrame>,
    overlay: OverlayWindow,
}

impl Renderer {
    /// The DirectDraw client owns `window` and must release the display before destroying it.
    pub fn new(window: isize) -> Result<Self, String> {
        let _ = NonZeroIsize::new(window).ok_or("a game window is required")?;
        let game_hwnd = HWND(window as *mut _);
        let overlay = OverlayWindow::new(game_hwnd)?;
        let mut raw = Win32WindowHandle::new(overlay.raw_handle()?);
        raw.hinstance = Some(overlay.hinstance());

        let instance = wgpu::Instance::new(&instance_descriptor());
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_window_handle: raw.into(),
                raw_display_handle: WindowsDisplayHandle::new().into(),
            })
        }
        .map_err(|err| format!("creating window surface: {err}"))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|err| format!("selecting adapter: {err}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("DirectDraw device"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|err| format!("creating GPU device: {err}"))?;

        // wgpu's default error handler panics. Log non-fatal errors and only mark device_lost
        // when the GPU device is actually lost so transient glitches never permanently freeze.
        let device_lost = Arc::new(AtomicBool::new(false));
        let lost_flag = Arc::clone(&device_lost);
        device.on_uncaptured_error(Arc::new(move |err: wgpu::Error| {
            compat::log_msg(&format!("wgpu uncaptured error: {err}"));
            if matches!(err, wgpu::Error::OutOfMemory { .. }) {
                lost_flag.store(true, Ordering::Relaxed);
            }
        }));
        let lost_flag = Arc::clone(&device_lost);
        device.set_device_lost_callback(move |reason, message| {
            compat::log_msg(&format!("wgpu device lost ({reason:?}): {message}"));
            lost_flag.store(true, Ordering::Relaxed);
        });

        // Delay the first configure until the HWND has a drawable client area.
        let mut surface_cfg = surface
            .get_default_config(&adapter, 1, 1)
            .ok_or("no presentation format for the game window")?;
        let caps = surface.get_capabilities(&adapter);
        surface_cfg.present_mode = pick_present_mode(&caps, config::get().vsync);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DirectDraw nearest-neighbor blit"),
            source: wgpu::ShaderSource::Wgsl(include_str!("present.wgsl").into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("DirectDraw presentation"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(surface_cfg.format.into())],
            }),
            multiview: None,
            cache: None,
        });
        if device_lost.load(Ordering::Relaxed) {
            return Err("GPU device lost during initialization".into());
        }
        Ok(Self {
            surface,
            device,
            queue,
            device_lost,
            config: surface_cfg,
            configured: false,
            pipeline,
            frame: None,
            overlay,
        })
    }

    pub fn sync_overlay(&mut self) {
        let _ = self.overlay.sync_to_owner();
    }

    pub fn present(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<PresentOutcome, PresentError> {
        if self.device_lost.load(Ordering::Relaxed) {
            return Err(PresentError::DeviceLost("GPU device lost".into()));
        }
        let limit = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > limit || height > limit {
            return Err(PresentError::Fatal(format!(
                "unsupported frame dimensions {width}x{height}"
            )));
        }
        let pitch = width
            .checked_mul(4)
            .ok_or_else(|| PresentError::Fatal("frame row size overflow".into()))?;
        if (pitch as usize).checked_mul(height as usize) != Some(rgba.len()) {
            return Err(PresentError::Fatal("incorrect RGBA frame length".into()));
        }
        let Some((target_w, target_h)) = self
            .overlay
            .sync_to_owner()
            .map_err(PresentError::Recoverable)?
        else {
            return Ok(PresentOutcome::Hidden);
        };
        if target_w > limit || target_h > limit {
            return Err(PresentError::Fatal(format!(
                "window exceeds the adapter's texture limit of {limit}"
            )));
        }
        if !self.configured || (target_w, target_h) != (self.config.width, self.config.height) {
            self.config.width = target_w;
            self.config.height = target_h;
            self.surface.configure(&self.device, &self.config);
            if self.device_lost.load(Ordering::Relaxed) {
                return Err(PresentError::DeviceLost(
                    "GPU device lost on configure".into(),
                ));
            }
            self.configured = true;
        }

        let mut acquired = self.surface.get_current_texture();
        if matches!(
            acquired,
            Err(wgpu::SurfaceError::Lost
                | wgpu::SurfaceError::Outdated
                | wgpu::SurfaceError::Other)
        ) {
            self.configured = false;
            self.surface.configure(&self.device, &self.config);
            if self.device_lost.load(Ordering::Relaxed) {
                return Err(PresentError::DeviceLost(
                    "GPU device lost during reconfigure".into(),
                ));
            }
            self.configured = true;
            acquired = self.surface.get_current_texture();
        }
        let output = match acquired {
            Ok(output) => output,
            Err(wgpu::SurfaceError::Timeout) => {
                compat::log_msg("wgpu get_current_texture timeout");
                return Ok(PresentOutcome::Timeout);
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                self.configured = false;
                return Err(PresentError::DeviceLost("surface out of memory".into()));
            }
            Err(err) => {
                self.configured = false;
                compat::log_msg(&format!(
                    "wgpu get_current_texture recoverable error: {err}"
                ));
                return Err(PresentError::Recoverable(format!(
                    "acquiring presentation texture: {err}"
                )));
            }
        };
        self.configured = !output.suboptimal;
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        if self
            .frame
            .as_ref()
            .is_some_and(|frame| frame.texture.size() != size)
        {
            self.frame = None;
        }
        let frame = self.frame.get_or_insert_with(|| {
            UploadedFrame::new(&self.device, &self.pipeline, self.config.format, size)
        });
        self.queue.write_texture(
            frame.texture.as_image_copy(),
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: None,
            },
            size,
        );
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("DirectDraw present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            let (vx, vy, vw, vh) =
                compute_viewport(width, height, target_w, target_h, config::get().maintas);
            pass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &frame.bindings, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        output.present();
        if self.device_lost.load(Ordering::Relaxed) {
            return Err(PresentError::DeviceLost(
                "GPU device lost after present".into(),
            ));
        }
        Ok(PresentOutcome::Presented)
    }
}
