//! `wgpu` multi-pass RetroArch `.glslp` / `.glsl` execution engine.
#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use crate::shader::lut::load_png_rgba;
use crate::shader::preset::{load_preset, FramebufferFormat, PassConfig, ShaderPreset, WrapMode};
use crate::shader::translate::{
    compile_retroarch_glsl, CompiledPass, TextureBinding, TextureSemantic, UniformField,
    UniformSemantic, UniformType,
};

const QUAD_VERTEX_STRIDE: u64 = 48; // 3 x vec4<f32>

// Fullscreen quad in NDC with top-left UV=(0,0) matching DirectDraw scanline order.
#[rustfmt::skip]
const QUAD_VERTICES: [f32; 48] = [
    // VertexCoord            TexCoord              COLOR
    -1.0,  1.0, 0.0, 1.0,     0.0, 0.0, 0.0, 1.0,   1.0, 1.0, 1.0, 1.0,
     1.0,  1.0, 0.0, 1.0,     1.0, 0.0, 0.0, 1.0,   1.0, 1.0, 1.0, 1.0,
    -1.0, -1.0, 0.0, 1.0,     0.0, 1.0, 0.0, 1.0,   1.0, 1.0, 1.0, 1.0,
     1.0, -1.0, 0.0, 1.0,     1.0, 1.0, 0.0, 1.0,   1.0, 1.0, 1.0, 1.0,
];

const BLIT_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) idx: u32) -> VsOut {
    let uv = vec2<f32>(f32((idx << 1u) & 2u), f32(idx & 2u));
    return VsOut(vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0), uv);
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_smp: sampler;

fn srgb_to_linear_channel(c: f32) -> f32 {
    if c <= 0.04045 {
        return c / 12.92;
    }
    return pow((c + 0.055) / 1.055, 2.4);
}

@fragment
fn fs_direct(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_smp, in.uv);
}

@fragment
fn fs_to_srgb_target(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(src_tex, src_smp, in.uv);
    return vec4<f32>(
        srgb_to_linear_channel(clamp(c.r, 0.0, 1.0)),
        srgb_to_linear_channel(clamp(c.g, 0.0, 1.0)),
        srgb_to_linear_channel(clamp(c.b, 0.0, 1.0)),
        c.a,
    );
}
"#;

struct LoadedLut {
    name: String,
    _texture: wgpu::Texture,
    view: wgpu::TextureView,
    linear: bool,
    wrap_mode: WrapMode,
}

struct RenderTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

impl RenderTarget {
    fn new(
        device: &wgpu::Device,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        extra_usage: wgpu::TextureUsages,
    ) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | extra_usage,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Self {
            texture,
            view,
            width: width.max(1),
            height: height.max(1),
            format,
        }
    }
}

struct CompiledGpuPass {
    config: PassConfig,
    renders_directly: bool,
    target_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    uniform_fields: Vec<UniformField>,
    uniform_bytes: Vec<u8>,
    texture_bindings: Vec<TextureBinding>,
}

pub struct FilterChain {
    passes: Vec<CompiledGpuPass>,
    alias_to_pass: HashMap<String, usize>,
    luts: Vec<LoadedLut>,
    quad_vb: wgpu::Buffer,
    samplers: HashMap<(bool, WrapMode), wgpu::Sampler>,
    clamp_to_border_supported: bool,
    original: Option<RenderTarget>,
    pass_targets: Vec<Option<RenderTarget>>,
    history: Vec<RenderTarget>,
    history_needed: usize,
    history_filled: usize,
    history_cursor: usize,
    blit_pipeline: Option<wgpu::RenderPipeline>,
    blit_bgl: Option<wgpu::BindGroupLayout>,
    frame_count: u32,
}

impl FilterChain {
    pub fn load(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        preset_path: &Path,
        swapchain_format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        let preset = load_preset(preset_path)?;
        Self::from_preset(device, queue, preset, swapchain_format)
    }

    pub fn from_preset(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        preset: ShaderPreset,
        swapchain_format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        let clamp_to_border_supported = device
            .features()
            .contains(wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER);

        // Load external LUT images referenced by the preset.
        let mut luts = Vec::with_capacity(preset.luts.len());
        let lut_names: Vec<String> = preset.luts.iter().map(|l| l.name.clone()).collect();
        for lut_cfg in &preset.luts {
            let img = load_png_rgba(&lut_cfg.path)?;
            let size = wgpu::Extent3d {
                width: img.width,
                height: img.height,
                depth_or_array_layers: 1,
            };
            let tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(&format!("RetroArch LUT {}", lut_cfg.name)),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            queue.write_texture(
                tex.as_image_copy(),
                &img.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(img.width * 4),
                    rows_per_image: None,
                },
                size,
            );
            let view = tex.create_view(&Default::default());
            luts.push(LoadedLut {
                name: lut_cfg.name.clone(),
                _texture: tex,
                view,
                linear: lut_cfg.linear,
                wrap_mode: lut_cfg.wrap_mode,
            });
        }

        // Compile each pass and construct its wgpu pipeline.
        let mut aliases: Vec<String> = Vec::new();
        let mut alias_to_pass: HashMap<String, usize> = HashMap::new();
        let mut passes: Vec<CompiledGpuPass> = Vec::with_capacity(preset.passes.len());
        let mut history_needed = 0usize;

        let pass_count = preset.passes.len();
        for (idx, pass_cfg) in preset.passes.into_iter().enumerate() {
            let is_last = idx + 1 == pass_count;
            let raw_src = std::fs::read_to_string(&pass_cfg.shader_path).map_err(|err| {
                format!(
                    "reading shader pass {} ({}): {err}",
                    idx,
                    pass_cfg.shader_path.display()
                )
            })?;
            let pass_dir = pass_cfg.shader_path.parent().unwrap_or(&preset.base_dir);

            let compiled = compile_retroarch_glsl(
                &raw_src,
                pass_dir,
                &aliases,
                &lut_names,
                &preset.parameters,
            )
            .map_err(|err| format!("pass {idx} ({}): {err}", pass_cfg.shader_path.display()))?;

            for tb in &compiled.texture_bindings {
                if let TextureSemantic::PrevFrame(k) = tb.semantic {
                    history_needed = history_needed.max(k + 1);
                }
            }

            // If swapchain is sRGB, route through an intermediate Rgba8Unorm FBO + blit shader
            // so RetroArch gamma-encoded shaders do not get double-gamma'd by the swapchain.
            let renders_directly =
                pass_cfg.renders_directly_to_viewport(is_last) && !swapchain_format.is_srgb();
            let target_format = if renders_directly {
                swapchain_format
            } else {
                fb_format_to_wgpu(pass_cfg.framebuffer_format)
            };

            let gpu_pass = build_gpu_pass(
                device,
                idx,
                pass_cfg,
                compiled,
                renders_directly,
                target_format,
            );

            if let Some(ref alias) = gpu_pass.config.alias {
                aliases.push(alias.clone());
                alias_to_pass.insert(alias.clone(), idx);
            }
            passes.push(gpu_pass);
        }

        let needs_blit = passes.last().is_some_and(|p| !p.renders_directly);
        let (blit_pipeline, blit_bgl) = if needs_blit {
            let (p, bgl) = create_blit_pipeline(device, swapchain_format);
            (Some(p), Some(bgl))
        } else {
            (None, None)
        };

        let quad_bytes: Vec<u8> = QUAD_VERTICES.iter().flat_map(|f| f.to_le_bytes()).collect();
        let quad_vb = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("RetroArch fullscreen quad"),
            size: quad_bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&quad_vb, 0, &quad_bytes);

        let pass_targets = (0..passes.len()).map(|_| None).collect();

        Ok(Self {
            passes,
            alias_to_pass,
            luts,
            quad_vb,
            samplers: HashMap::new(),
            clamp_to_border_supported,
            original: None,
            pass_targets,
            history: Vec::new(),
            history_needed,
            history_filled: 0,
            history_cursor: 0,
            blit_pipeline,
            blit_bgl,
            frame_count: 0,
        })
    }

    /// Ensures the offscreen `Original` target matches the current DirectDraw logical frame size
    /// and returns its view for Pass 0 (raw pixel unpack).
    pub fn prepare_original_target(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> &wgpu::TextureView {
        let needs_recreate = self
            .original
            .as_ref()
            .is_none_or(|t| t.width != width || t.height != height);
        if needs_recreate {
            let extra = if self.history_needed > 0 {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            };
            self.original = Some(RenderTarget::new(
                device,
                "RetroArch Original",
                width,
                height,
                wgpu::TextureFormat::Rgba8Unorm,
                extra,
            ));
            self.history.clear();
            self.history_filled = 0;
            self.history_cursor = 0;
            for h in 0..self.history_needed {
                self.history.push(RenderTarget::new(
                    device,
                    &format!("RetroArch History {h}"),
                    width,
                    height,
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureUsages::COPY_DST,
                ));
            }
        }
        &self.original.as_ref().expect("created above").view
    }

    /// Executes all shader passes from `Original` to the final swapchain view inside `viewport`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        swapchain_view: &wgpu::TextureView,
        viewport: (u32, u32, u32, u32),
    ) {
        let Some((orig_w, orig_h)) = self.original.as_ref().map(|t| (t.width, t.height)) else {
            return;
        };
        let (vx, vy, vw, vh) = viewport;
        let max_dim = device.limits().max_texture_dimension_2d;

        // 1. Compute and allocate/resize all intermediate pass targets.
        let pass_count = self.passes.len();
        let mut pass_sizes: Vec<(u32, u32)> = Vec::with_capacity(pass_count);
        let mut prev_w = orig_w;
        let mut prev_h = orig_h;
        for (idx, pass) in self.passes.iter().enumerate() {
            let is_last = idx + 1 == pass_count;
            let (out_w, out_h) = if pass.renders_directly {
                (vw.max(1), vh.max(1))
            } else {
                pass.config
                    .output_size(is_last, prev_w, prev_h, vw.max(1), vh.max(1), max_dim)
            };
            pass_sizes.push((out_w, out_h));
            if !pass.renders_directly {
                let needs_alloc = self.pass_targets[idx].as_ref().is_none_or(|t| {
                    t.width != out_w || t.height != out_h || t.format != pass.target_format
                });
                if needs_alloc {
                    self.pass_targets[idx] = Some(RenderTarget::new(
                        device,
                        &format!("RetroArch Pass {idx} FBO"),
                        out_w,
                        out_h,
                        pass.target_format,
                        wgpu::TextureUsages::empty(),
                    ));
                }
            } else {
                self.pass_targets[idx] = None;
            }
            prev_w = out_w;
            prev_h = out_h;
        }

        // Pre-ensure every sampler needed by this frame exists in `self.samplers`.
        let mut sampler_keys: Vec<(bool, WrapMode)> = Vec::new();
        for pass in &self.passes {
            let linear = pass.config.filter_linear.unwrap_or(false);
            sampler_keys.push((linear, pass.config.wrap_mode));
        }
        for lut in &self.luts {
            sampler_keys.push((lut.linear, lut.wrap_mode));
        }
        if let Some(last) = self.passes.last() {
            let linear = last.config.filter_linear.unwrap_or(false);
            sampler_keys.push((linear, WrapMode::ClampToEdge));
        }
        for (linear, wrap) in sampler_keys {
            self.ensure_sampler(device, linear, wrap);
        }

        // 2. Execute each pass in order.
        let frame_count = self.frame_count;
        for idx in 0..pass_count {
            let (src_w, src_h) = if idx == 0 {
                (orig_w, orig_h)
            } else {
                pass_sizes[idx - 1]
            };
            let (out_w, out_h) = pass_sizes[idx];

            // Fill per-pass uniform buffer.
            {
                let pass = &mut self.passes[idx];
                let fc = match pass.config.frame_count_mod {
                    Some(m) if m > 0 => frame_count % m,
                    _ => frame_count,
                };
                pass.uniform_bytes.fill(0);
                for field in &pass.uniform_fields {
                    write_uniform_field(
                        &mut pass.uniform_bytes,
                        field,
                        idx,
                        (src_w, src_h),
                        (out_w, out_h),
                        (orig_w, orig_h),
                        &pass_sizes,
                        &self.alias_to_pass,
                        fc,
                    );
                }
                queue.write_buffer(&pass.uniform_buffer, 0, &pass.uniform_bytes);
            }

            // Build bind group for this pass.
            let pass = &self.passes[idx];
            let mut entries: Vec<wgpu::BindGroupEntry<'_>> =
                Vec::with_capacity(1 + pass.texture_bindings.len() * 2);
            entries.push(wgpu::BindGroupEntry {
                binding: 0,
                resource: pass.uniform_buffer.as_entire_binding(),
            });

            let pass_linear = pass.config.filter_linear.unwrap_or(false);
            let pass_wrap = pass.config.wrap_mode;

            for tb in &pass.texture_bindings {
                let (tex_view, smp_key) =
                    self.resolve_texture_and_sampler(idx, &tb.semantic, pass_linear, pass_wrap);
                let sampler = self
                    .samplers
                    .get(&smp_key)
                    .expect("sampler pre-created above");
                entries.push(wgpu::BindGroupEntry {
                    binding: tb.tex_binding,
                    resource: wgpu::BindingResource::TextureView(tex_view),
                });
                entries.push(wgpu::BindGroupEntry {
                    binding: tb.smp_binding,
                    resource: wgpu::BindingResource::Sampler(sampler),
                });
            }

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(&format!("RetroArch Pass {idx} BindGroup")),
                layout: &pass.bind_group_layout,
                entries: &entries,
            });

            let target_view = if pass.renders_directly {
                swapchain_view
            } else {
                &self.pass_targets[idx]
                    .as_ref()
                    .expect("intermediate FBO allocated")
                    .view
            };

            let clear_op = if pass.renders_directly {
                wgpu::LoadOp::Clear(wgpu::Color::BLACK)
            } else {
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
            };

            {
                let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some(&format!("RetroArch Pass {idx}")),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: clear_op,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                if pass.renders_directly {
                    rpass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
                }
                rpass.set_pipeline(&pass.pipeline);
                rpass.set_bind_group(0, &bind_group, &[]);
                rpass.set_vertex_buffer(0, self.quad_vb.slice(..));
                rpass.draw(0..4, 0..1);
            }
        }

        // 3. If the last pass rendered into an intermediate FBO, blit it into the swapchain viewport.
        if let (Some(blit_pipeline), Some(blit_bgl)) = (&self.blit_pipeline, &self.blit_bgl) {
            let last_idx = pass_count - 1;
            let last_target = self.pass_targets[last_idx]
                .as_ref()
                .expect("last FBO allocated for blit");
            let last_linear = self.passes[last_idx].config.filter_linear.unwrap_or(false);
            let sampler = self
                .samplers
                .get(&(last_linear, WrapMode::ClampToEdge))
                .expect("blit sampler pre-created");
            let blit_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("RetroArch final blit BindGroup"),
                layout: blit_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&last_target.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("RetroArch final viewport blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: swapchain_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            rpass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
            rpass.set_pipeline(blit_pipeline);
            rpass.set_bind_group(0, &blit_bg, &[]);
            rpass.draw(0..3, 0..1);
        }

        // 4. Snapshot `Original` into the history ring buffer if any pass uses `Prev*Texture`.
        if self.history_needed > 0 {
            if let Some(orig) = self.original.as_ref() {
                let slot = self.history_cursor;
                encoder.copy_texture_to_texture(
                    orig.texture.as_image_copy(),
                    self.history[slot].texture.as_image_copy(),
                    wgpu::Extent3d {
                        width: orig.width,
                        height: orig.height,
                        depth_or_array_layers: 1,
                    },
                );
                self.history_cursor = (self.history_cursor + 1) % self.history_needed;
                self.history_filled = (self.history_filled + 1).min(self.history_needed);
            }
        }

        self.frame_count = self.frame_count.wrapping_add(1);
    }

    fn ensure_sampler(&mut self, device: &wgpu::Device, linear: bool, wrap: WrapMode) {
        self.samplers.entry((linear, wrap)).or_insert_with(|| {
            let filter = if linear {
                wgpu::FilterMode::Linear
            } else {
                wgpu::FilterMode::Nearest
            };
            let (address_mode, border_color) = match wrap {
                WrapMode::ClampToBorder if self.clamp_to_border_supported => (
                    wgpu::AddressMode::ClampToBorder,
                    Some(wgpu::SamplerBorderColor::TransparentBlack),
                ),
                WrapMode::ClampToBorder | WrapMode::ClampToEdge => {
                    (wgpu::AddressMode::ClampToEdge, None)
                }
                WrapMode::Repeat => (wgpu::AddressMode::Repeat, None),
                WrapMode::MirroredRepeat => (wgpu::AddressMode::MirrorRepeat, None),
            };
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("RetroArch sampler"),
                address_mode_u: address_mode,
                address_mode_v: address_mode,
                address_mode_w: address_mode,
                mag_filter: filter,
                min_filter: filter,
                mipmap_filter: wgpu::FilterMode::Nearest,
                border_color,
                ..Default::default()
            })
        });
    }

    fn resolve_texture_and_sampler<'a>(
        &'a self,
        pass_idx: usize,
        semantic: &TextureSemantic,
        default_linear: bool,
        default_wrap: WrapMode,
    ) -> (&'a wgpu::TextureView, (bool, WrapMode)) {
        let orig_view = &self.original.as_ref().expect("original exists").view;
        match semantic {
            TextureSemantic::Source => {
                if pass_idx == 0 {
                    (orig_view, (default_linear, default_wrap))
                } else if let Some(prev) = self.pass_targets[pass_idx - 1].as_ref() {
                    (&prev.view, (default_linear, default_wrap))
                } else {
                    (orig_view, (default_linear, default_wrap))
                }
            }
            TextureSemantic::Original => (orig_view, (default_linear, default_wrap)),
            TextureSemantic::PassOutput(k) => {
                if *k < pass_idx {
                    if let Some(t) = self.pass_targets[*k].as_ref() {
                        return (&t.view, (default_linear, default_wrap));
                    }
                }
                (orig_view, (default_linear, default_wrap))
            }
            TextureSemantic::PassPrev(n) => {
                if *n <= pass_idx {
                    if let Some(t) = self.pass_targets[pass_idx - *n].as_ref() {
                        return (&t.view, (default_linear, default_wrap));
                    }
                }
                (orig_view, (default_linear, default_wrap))
            }
            TextureSemantic::Alias(name) => {
                if let Some(&k) = self.alias_to_pass.get(name) {
                    if k < pass_idx {
                        if let Some(t) = self.pass_targets[k].as_ref() {
                            return (&t.view, (default_linear, default_wrap));
                        }
                    }
                }
                (orig_view, (default_linear, default_wrap))
            }
            TextureSemantic::PrevFrame(age) => {
                if self.history_filled > 0 && !self.history.is_empty() {
                    let clamped_age = (*age).min(self.history_filled - 1);
                    let len = self.history.len();
                    // `history_cursor` points to the oldest slot (next to overwrite), so the most
                    // recent previous frame (`age = 0`) is at `(cursor + len - 1) % len`.
                    let idx = (self.history_cursor + len - 1 - (clamped_age % len)) % len;
                    (&self.history[idx].view, (default_linear, default_wrap))
                } else {
                    (orig_view, (default_linear, default_wrap))
                }
            }
            TextureSemantic::Lut(name) => {
                if let Some(lut) = self
                    .luts
                    .iter()
                    .find(|l| l.name == *name || l.name.eq_ignore_ascii_case(name))
                {
                    (&lut.view, (lut.linear, lut.wrap_mode))
                } else {
                    (orig_view, (default_linear, default_wrap))
                }
            }
        }
    }
}

fn fb_format_to_wgpu(fmt: FramebufferFormat) -> wgpu::TextureFormat {
    match fmt {
        FramebufferFormat::Rgba8Unorm => wgpu::TextureFormat::Rgba8Unorm,
        FramebufferFormat::Rgba8UnormSrgb => wgpu::TextureFormat::Rgba8UnormSrgb,
        FramebufferFormat::Rgba16Float => wgpu::TextureFormat::Rgba16Float,
    }
}

fn build_gpu_pass(
    device: &wgpu::Device,
    idx: usize,
    config: PassConfig,
    compiled: CompiledPass,
    renders_directly: bool,
    target_format: wgpu::TextureFormat,
) -> CompiledGpuPass {
    let mut bgl_entries: Vec<wgpu::BindGroupLayoutEntry> =
        Vec::with_capacity(1 + compiled.texture_bindings.len() * 2);
    bgl_entries.push(wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    });
    for tb in &compiled.texture_bindings {
        bgl_entries.push(wgpu::BindGroupLayoutEntry {
            binding: tb.tex_binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
        bgl_entries.push(wgpu::BindGroupLayoutEntry {
            binding: tb.smp_binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        });
    }

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(&format!("RetroArch Pass {idx} BGL")),
        entries: &bgl_entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(&format!("RetroArch Pass {idx} PipelineLayout")),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });

    // Match vertex attributes to the exact locations declared by this pass's vertex shader.
    let vertex_attrs = collect_vertex_attributes(&compiled.vertex_module);

    let vs_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&format!("RetroArch Pass {idx} VS")),
        source: wgpu::ShaderSource::Naga(Cow::Owned(compiled.vertex_module)),
    });
    let fs_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&format!("RetroArch Pass {idx} FS")),
        source: wgpu::ShaderSource::Naga(Cow::Owned(compiled.fragment_module)),
    });

    let vertex_buffers = if vertex_attrs.is_empty() {
        Vec::new()
    } else {
        vec![wgpu::VertexBufferLayout {
            array_stride: QUAD_VERTEX_STRIDE,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &vertex_attrs,
        }]
    };

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(&format!("RetroArch Pass {idx} Pipeline")),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &vs_module,
            entry_point: Some("main"),
            buffers: &vertex_buffers,
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &fs_module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            targets: &[Some(target_format.into())],
        }),
        multiview: None,
        cache: None,
    });

    let uniform_size = compiled.uniform_size.max(16);
    let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(&format!("RetroArch Pass {idx} UBO")),
        size: uniform_size as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    CompiledGpuPass {
        config,
        renders_directly,
        target_format,
        pipeline,
        bind_group_layout,
        uniform_buffer,
        uniform_fields: compiled.uniform_fields,
        uniform_bytes: vec![0u8; uniform_size],
        texture_bindings: compiled.texture_bindings,
    }
}

fn collect_vertex_attributes(module: &naga::Module) -> Vec<wgpu::VertexAttribute> {
    let mut attrs = Vec::new();
    let Some(ep) = module.entry_points.first() else {
        return attrs;
    };
    for arg in &ep.function.arguments {
        let Some(naga::Binding::Location { location, .. }) = arg.binding else {
            continue;
        };
        let offset = match location {
            0 => 0,
            1 => 16,
            2 => 32,
            _ => 0,
        };
        let format = match module.types[arg.ty].inner {
            naga::TypeInner::Vector {
                size: naga::VectorSize::Bi,
                ..
            } => wgpu::VertexFormat::Float32x2,
            naga::TypeInner::Vector {
                size: naga::VectorSize::Tri,
                ..
            } => wgpu::VertexFormat::Float32x3,
            _ => wgpu::VertexFormat::Float32x4,
        };
        attrs.push(wgpu::VertexAttribute {
            format,
            offset,
            shader_location: location,
        });
    }
    attrs
}

fn create_blit_pipeline(
    device: &wgpu::Device,
    swapchain_format: wgpu::TextureFormat,
) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("RetroArch final blit"),
        source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("RetroArch final blit BGL"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("RetroArch final blit layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let entry_point = if swapchain_format.is_srgb() {
        "fs_to_srgb_target"
    } else {
        "fs_direct"
    };
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("RetroArch final blit pipeline"),
        layout: Some(&layout),
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
            entry_point: Some(entry_point),
            compilation_options: Default::default(),
            targets: &[Some(swapchain_format.into())],
        }),
        multiview: None,
        cache: None,
    });
    (pipeline, bgl)
}

#[allow(clippy::too_many_arguments)]
fn write_uniform_field(
    buf: &mut [u8],
    field: &UniformField,
    pass_idx: usize,
    src_size: (u32, u32),
    out_size: (u32, u32),
    orig_size: (u32, u32),
    pass_sizes: &[(u32, u32)],
    alias_to_pass: &HashMap<String, usize>,
    frame_count: u32,
) {
    let off = field.offset;
    match &field.semantic {
        UniformSemantic::MvpMatrix => {
            #[rustfmt::skip]
            let identity: [f32; 16] = [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 0.0, 1.0,
            ];
            write_f32s(buf, off, &identity);
        }
        UniformSemantic::TextureSize | UniformSemantic::InputSize => {
            write_size_field(buf, off, field.ty, src_size);
        }
        UniformSemantic::OutputSize => {
            write_size_field(buf, off, field.ty, out_size);
        }
        UniformSemantic::OrigTextureSize
        | UniformSemantic::OrigInputSize
        | UniformSemantic::PrevTextureSize(_)
        | UniformSemantic::PrevInputSize(_) => {
            write_size_field(buf, off, field.ty, orig_size);
        }
        UniformSemantic::PassTextureSize(k) | UniformSemantic::PassInputSize(k) => {
            let sz = if *k < pass_idx {
                pass_sizes.get(*k).copied().unwrap_or(orig_size)
            } else {
                orig_size
            };
            write_size_field(buf, off, field.ty, sz);
        }
        UniformSemantic::PassPrevTextureSize(n) | UniformSemantic::PassPrevInputSize(n) => {
            let sz = if *n <= pass_idx {
                pass_sizes.get(pass_idx - *n).copied().unwrap_or(orig_size)
            } else {
                orig_size
            };
            write_size_field(buf, off, field.ty, sz);
        }
        UniformSemantic::AliasTextureSize(alias) | UniformSemantic::AliasInputSize(alias) => {
            let sz = alias_to_pass
                .get(alias)
                .filter(|&&k| k < pass_idx)
                .and_then(|&k| pass_sizes.get(k).copied())
                .unwrap_or(orig_size);
            write_size_field(buf, off, field.ty, sz);
        }
        UniformSemantic::FrameCount => match field.ty {
            UniformType::Float => write_f32s(buf, off, &[frame_count as f32]),
            UniformType::Int => write_bytes(buf, off, &(frame_count as i32).to_le_bytes()),
            UniformType::Uint => write_bytes(buf, off, &frame_count.to_le_bytes()),
            _ => {}
        },
        UniformSemantic::FrameDirection => match field.ty {
            UniformType::Float => write_f32s(buf, off, &[1.0]),
            UniformType::Int => write_bytes(buf, off, &1i32.to_le_bytes()),
            UniformType::Uint => write_bytes(buf, off, &1u32.to_le_bytes()),
            _ => {}
        },
        UniformSemantic::Parameter(_) => match field.ty {
            UniformType::Float => write_f32s(buf, off, &[field.default_param]),
            UniformType::Int => write_bytes(buf, off, &(field.default_param as i32).to_le_bytes()),
            UniformType::Uint => write_bytes(
                buf,
                off,
                &(field.default_param.max(0.0) as u32).to_le_bytes(),
            ),
            _ => {}
        },
        UniformSemantic::Unknown => {}
    }
}

fn write_size_field(buf: &mut [u8], off: usize, ty: UniformType, (w, h): (u32, u32)) {
    let wf = w.max(1) as f32;
    let hf = h.max(1) as f32;
    match ty {
        UniformType::Vec2 => write_f32s(buf, off, &[wf, hf]),
        UniformType::Vec4 => write_f32s(buf, off, &[wf, hf, 1.0 / wf, 1.0 / hf]),
        UniformType::IVec2 => {
            write_bytes(buf, off, &(w as i32).to_le_bytes());
            write_bytes(buf, off + 4, &(h as i32).to_le_bytes());
        }
        UniformType::UVec2 => {
            write_bytes(buf, off, &w.to_le_bytes());
            write_bytes(buf, off + 4, &h.to_le_bytes());
        }
        _ => {}
    }
}

fn write_f32s(buf: &mut [u8], off: usize, vals: &[f32]) {
    for (i, v) in vals.iter().enumerate() {
        write_bytes(buf, off + i * 4, &v.to_le_bytes());
    }
}

fn write_bytes(buf: &mut [u8], off: usize, bytes: &[u8]) {
    if off + bytes.len() <= buf.len() {
        buf[off..off + bytes.len()].copy_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: None,
            ..Default::default()
        }))
        .ok()?;
        let mut features = wgpu::Features::empty();
        if adapter
            .features()
            .contains(wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER)
        {
            features |= wgpu::Features::ADDRESS_MODE_CLAMP_TO_BORDER;
        }
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("shader test device"),
            required_features: features,
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .ok()
    }

    fn readback_pixel(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> [u8; 4] {
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().unwrap().unwrap();
        let data = slice.get_mapped_range();
        let px = [data[0], data[1], data[2], data[3]];
        drop(data);
        staging.unmap();
        px
    }

    #[test]
    fn multipass_glslp_with_lut_alias_history_and_params_renders_on_gpu() {
        let Some((device, queue)) = test_device() else {
            return;
        };

        let tmp = std::env::temp_dir().join(format!("cnc_ddraw_glslp_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Write a 1x1 PNG LUT with green tint (128, 255, 255, 255).
        let lut_path = tmp.join("tint.png");
        {
            let file = std::fs::File::create(&lut_path).unwrap();
            let mut enc = png::Encoder::new(file, 1, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&[128, 255, 255, 255]).unwrap();
        }

        // Pass 0: multiplies red channel by overridden parameter BOOST and exercises multi-line macro
        // + helper function + PrevTexture history.
        let pass0 = r#"
#pragma parameter BOOST "Red Boost" 1.0 0.0 4.0 0.1
#define FETCH(img, c) texture2D(img, c)

#if defined(VERTEX)
attribute vec4 VertexCoord;
attribute vec4 TexCoord;
uniform mat4 MVPMatrix;
varying vec2 v_uv;
void main() {
    gl_Position = MVPMatrix * VertexCoord;
    v_uv = TexCoord.xy;
}
#elif defined(FRAGMENT)
uniform sampler2D Texture;
uniform sampler2D PrevTexture;
uniform float BOOST;
varying vec2 v_uv;

vec4 read_it(sampler2D s, vec2 uv) {
    return FETCH(
        s,
        uv
    );
}

void main() {
    vec4 cur = read_it(Texture, v_uv);
    vec4 prev = texture2D(PrevTexture, v_uv);
    gl_FragColor = vec4(cur.r * BOOST, cur.g, prev.b, 1.0);
}
#endif
"#;
        std::fs::write(tmp.join("pass0.glsl"), pass0).unwrap();

        // Pass 1: combines Pass 0 via alias `BoostedTexture` and external PNG LUT `TintLUT`.
        let pass1 = r#"
#if defined(VERTEX)
in vec4 VertexCoord;
in vec4 TexCoord;
uniform mat4 MVPMatrix;
out vec2 uv;
void main() {
    gl_Position = MVPMatrix * VertexCoord;
    uv = TexCoord.xy;
}
#elif defined(FRAGMENT)
uniform sampler2D BoostedTexture;
uniform sampler2D TintLUT;
in vec2 uv;
out vec4 FragColor;
void main() {
    vec4 c = texture(BoostedTexture, uv);
    vec4 lut = texture(TintLUT, uv);
    FragColor = vec4(c.rgb * lut.rgb, 1.0);
}
#endif
"#;
        std::fs::write(tmp.join("pass1.glsl"), pass1).unwrap();

        let preset_txt = r#"
shaders = 2
shader0 = "pass0.glsl"
alias0 = "Boosted"
filter_linear0 = false
scale_type0 = source
scale0 = 2.0
float_framebuffer0 = true

shader1 = "pass1.glsl"
filter_linear1 = true
scale_type1 = viewport
scale1 = 1.0

textures = "TintLUT"
TintLUT = "tint.png"
TintLUT_linear = false

parameters = "BOOST"
BOOST = "2.0"
"#;
        let preset_path = tmp.join("test.glslp");
        std::fs::write(&preset_path, preset_txt).unwrap();

        // Test both direct non-sRGB target and sRGB target (which exercises the blit path).
        for fmt in [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ] {
            let mut chain =
                FilterChain::load(&device, &queue, &preset_path, fmt).expect("load FilterChain");
            let out_tex = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("test out"),
                size: wgpu::Extent3d {
                    width: 4,
                    height: 4,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: fmt,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let out_view = out_tex.create_view(&Default::default());

            // Frame 0: fill Original with (0.5, 0.25, 0.75, 1.0).
            // Pass 0 -> (0.5 * 2.0 = 1.0, 0.25, 0.75, 1.0).
            // Pass 1 -> * LUT (128/255 ≈ 0.502, 1.0, 1.0) => (~128, ~64, ~191, 255).
            let mut enc = device.create_command_encoder(&Default::default());
            {
                let orig = chain.prepare_original_target(&device, 2, 2);
                let _rpass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("fill original"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: orig,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.5,
                                g: 0.25,
                                b: 0.75,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            chain.render(&device, &queue, &mut enc, &out_view, (0, 0, 4, 4));
            queue.submit([enc.finish()]);

            let px = readback_pixel(&device, &queue, &out_tex);
            assert!(
                (px[0] as i32 - 128).abs() <= 2
                    && (px[1] as i32 - 64).abs() <= 2
                    && (px[2] as i32 - 191).abs() <= 2
                    && px[3] == 255,
                "unexpected pixel for {fmt:?}: {px:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
