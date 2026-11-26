//! Pure wgpu presentation backend with GPU-side pixel unpacking (Indexed / RGB555 / RGB565 / XRGB)
//! and palette/gamma LUT evaluation. All Win32 overlay management lives in `crate::window::OverlayWindow`.
use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use raw_window_handle::{Win32WindowHandle, WindowsDisplayHandle};
use windows::Win32::Foundation::HWND;

use crate::compat;
use crate::config::{self, RendererBackend};
use crate::shader::FilterChain;
use crate::surface::{PixelFormat, SurfaceSlot};
use crate::window::{self, compute_viewport, OverlayWindow};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentOutcome {
    Presented,
    Hidden,
    Timeout,
    Busy,
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

fn raw_texture_format(format: PixelFormat) -> wgpu::TextureFormat {
    match format {
        PixelFormat::Indexed => wgpu::TextureFormat::R8Uint,
        PixelFormat::Rgb555 | PixelFormat::Rgb565 => wgpu::TextureFormat::R16Uint,
        PixelFormat::Xrgb => wgpu::TextureFormat::R32Uint,
    }
}

fn params_bytes(format: PixelFormat) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&(format as u32).to_le_bytes());
    out
}

struct UploadedFrame {
    raw_texture: wgpu::Texture,
    raw_format: wgpu::TextureFormat,
    lut_texture: wgpu::Texture,
    params_buffer: wgpu::Buffer,
    bindings: wgpu::BindGroup,
    last_format: Option<PixelFormat>,
    last_lut: Option<[[u8; 4]; 256]>,
}

impl UploadedFrame {
    fn new(
        device: &wgpu::Device,
        pipeline: &wgpu::RenderPipeline,
        target_format: wgpu::TextureFormat,
        size: wgpu::Extent3d,
        raw_format: wgpu::TextureFormat,
    ) -> Self {
        let raw_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw raw primary pixels"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: raw_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        // Match the transfer function of the target to preserve exact game byte colors.
        let lut_format = if target_format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let lut_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw palette/gamma LUT"),
            size: wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: lut_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("DirectDraw present params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let raw_view = raw_texture.create_view(&Default::default());
        let lut_view = lut_texture.create_view(&Default::default());
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("DirectDraw frame bindings"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&raw_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buffer.as_entire_binding(),
                },
            ],
        });
        UploadedFrame {
            raw_texture,
            raw_format,
            lut_texture,
            params_buffer,
            bindings,
            last_format: None,
            last_lut: None,
        }
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
    filter_chain: Option<FilterChain>,
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
        // SAFETY: `overlay` owns the Win32 presentation HWND and is declared after `surface`
        // in `Renderer`, so `surface` is always dropped before `overlay` destroys the window.
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
        let mut required_features = wgpu::Features::empty();
        if adapter
            .features()
            .contains(wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER)
        {
            required_features |= wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER;
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("DirectDraw device"),
            required_features,
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

        let shader_path = config::get().resolved_shader_path();
        if shader_path.is_some() {
            // RetroArch GLSL shaders output gamma-encoded colors; prefer a non-sRGB swapchain
            // format when available so the final pass can write straight to the swapchain.
            if let Some(&non_srgb) = caps.formats.iter().find(|f| !f.is_srgb()) {
                surface_cfg.format = non_srgb;
            }
        }

        let filter_chain = match shader_path {
            Some(path) => match FilterChain::load(&device, &queue, &path, surface_cfg.format) {
                Ok(chain) => {
                    compat::log_msg(&format!("loaded RetroArch shader: {}", path.display()));
                    Some(chain)
                }
                Err(err) => {
                    compat::log_msg(&format!(
                        "failed to load shader {} (falling back to default present): {err}",
                        path.display()
                    ));
                    None
                }
            },
            None => None,
        };

        let unpack_target_format = if filter_chain.is_some() {
            wgpu::TextureFormat::Rgba8Unorm
        } else {
            surface_cfg.format
        };

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DirectDraw GPU unpack and blit"),
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
                targets: &[Some(unpack_target_format.into())],
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
            filter_chain,
            frame: None,
            overlay,
        })
    }

    pub fn sync_overlay(&mut self) {
        let _ = self.overlay.sync_to_owner();
    }

    fn upload_frame(
        &mut self,
        width: u32,
        height: u32,
        pitch: u32,
        format: PixelFormat,
        pixels: &[u8],
        lut: &[[u8; 4]; 256],
    ) {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let raw_format = raw_texture_format(format);
        if self
            .frame
            .as_ref()
            .is_some_and(|frame| frame.raw_texture.size() != size || frame.raw_format != raw_format)
        {
            self.frame = None;
        }
        let unpack_format = if self.filter_chain.is_some() {
            wgpu::TextureFormat::Rgba8Unorm
        } else {
            self.config.format
        };
        let frame = self.frame.get_or_insert_with(|| {
            UploadedFrame::new(
                &self.device,
                &self.pipeline,
                unpack_format,
                size,
                raw_format,
            )
        });
        if frame.last_format != Some(format) {
            self.queue
                .write_buffer(&frame.params_buffer, 0, &params_bytes(format));
            frame.last_format = Some(format);
        }
        if frame.last_lut.as_ref() != Some(lut) {
            let lut_bytes = lut.as_flattened();
            self.queue.write_texture(
                frame.lut_texture.as_image_copy(),
                lut_bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256 * 4),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: 256,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            frame.last_lut = Some(*lut);
        }
        self.queue.write_texture(
            frame.raw_texture.as_image_copy(),
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: None,
            },
            size,
        );
    }

    /// Presents the primary surface while holding its mutex ONLY for the brief GPU texture
    /// upload (`queue.write_texture`), never across swapchain acquire or `output.present()`.
    /// `on_uploaded` is invoked before dropping the surface lock so callers can clear their
    /// dirty bit without racing subsequent worker-thread writes.
    pub fn present_surface(
        &mut self,
        primary: &SurfaceSlot,
        on_uploaded: impl FnOnce(),
    ) -> Result<PresentOutcome, PresentError> {
        if self.device_lost.load(Ordering::Relaxed) {
            return Err(PresentError::DeviceLost("GPU device lost".into()));
        }
        let limit = self.device.limits().max_texture_dimension_2d;
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

        // Hold the primary surface mutex ONLY while validating, building the 1 KB LUT, and
        // staging pixels into wgpu's upload buffer (~20-30 us) so concurrent worker threads
        // calling Primary->Lock() are never stalled across swapchain acquire/present.
        let (width, height) = {
            let guard = primary.lock();
            if guard.locked() {
                return Ok(PresentOutcome::Busy);
            }
            let (width, height) = (guard.width, guard.height);
            if width == 0 || height == 0 || width > limit || height > limit {
                return Err(PresentError::Fatal(format!(
                    "unsupported frame dimensions {width}x{height}"
                )));
            }
            let format = guard.pixel_format();
            let pitch = guard.pitch();
            let min_row = width
                .checked_mul(format.bytes() as u32)
                .ok_or_else(|| PresentError::Fatal("frame row size overflow".into()))?;
            let pixels = guard.pixel_slice();
            if pitch < min_row
                || (pitch as usize).checked_mul(height as usize) != Some(pixels.len())
            {
                return Err(PresentError::Fatal(
                    "incorrect raw frame buffer length".into(),
                ));
            }
            let lut = guard
                .build_lut()
                .map_err(|err| PresentError::Fatal(format!("building frame LUT: {err}")))?;
            window::sync_presented_size(width, height);
            self.upload_frame(width, height, pitch, format, pixels, &lut);
            on_uploaded();
            (width, height)
        };

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
        let frame = self.frame.as_ref().expect("frame uploaded above");
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let viewport = compute_viewport(width, height, target_w, target_h, config::get().maintas);
        if let Some(chain) = self.filter_chain.as_mut() {
            {
                let orig_view = chain.prepare_original_target(&self.device, width, height);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("DirectDraw unpack to Original"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: orig_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &frame.bindings, &[]);
                pass.draw(0..3, 0..1);
            }
            chain.render(&self.device, &self.queue, &mut encoder, &view, viewport);
        } else {
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
            let (vx, vy, vw, vh) = viewport;
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
