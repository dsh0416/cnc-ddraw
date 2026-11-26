//! RetroArch `.glslp` preset and standalone `.glsl` loader.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_REFERENCE_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleType {
    Source,
    Viewport,
    Absolute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WrapMode {
    ClampToBorder,
    ClampToEdge,
    Repeat,
    MirroredRepeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramebufferFormat {
    Rgba8Unorm,
    Rgba8UnormSrgb,
    Rgba16Float,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PassConfig {
    pub shader_path: PathBuf,
    pub alias: Option<String>,
    pub filter_linear: Option<bool>,
    pub wrap_mode: WrapMode,
    pub scale_type_x: Option<ScaleType>,
    pub scale_type_y: Option<ScaleType>,
    pub scale_x: Option<f32>,
    pub scale_y: Option<f32>,
    pub framebuffer_format: FramebufferFormat,
    pub frame_count_mod: Option<u32>,
    pub mipmap_input: bool,
}

impl PassConfig {
    pub fn default_for_shader(shader_path: PathBuf) -> Self {
        Self {
            shader_path,
            alias: None,
            filter_linear: None,
            wrap_mode: WrapMode::ClampToBorder,
            scale_type_x: None,
            scale_type_y: None,
            scale_x: None,
            scale_y: None,
            framebuffer_format: FramebufferFormat::Rgba8Unorm,
            frame_count_mod: None,
            mipmap_input: false,
        }
    }

    /// Computes this pass's output texture dimensions according to RetroArch scaling rules.
    pub fn output_size(
        &self,
        is_last: bool,
        src_w: u32,
        src_h: u32,
        vp_w: u32,
        vp_h: u32,
        max_dim: u32,
    ) -> (u32, u32) {
        let default_type = if is_last {
            ScaleType::Viewport
        } else {
            ScaleType::Source
        };
        let tx = self.scale_type_x.unwrap_or(default_type);
        let ty = self.scale_type_y.unwrap_or(default_type);
        let sx = self.scale_x.unwrap_or(1.0);
        let sy = self.scale_y.unwrap_or(1.0);

        let calc = |ty: ScaleType, scale: f32, src: u32, vp: u32| -> u32 {
            let raw = match ty {
                ScaleType::Source => src as f32 * scale,
                ScaleType::Viewport => vp as f32 * scale,
                ScaleType::Absolute => scale,
            };
            if !raw.is_finite() || raw <= 1.0 {
                1
            } else {
                (raw.round() as u32).clamp(1, max_dim.max(1))
            }
        };

        (calc(tx, sx, src_w, vp_w), calc(ty, sy, src_h, vp_h))
    }

    /// True when the final pass can render directly into the swapchain viewport without an
    /// intermediate FBO + blit pass.
    pub fn renders_directly_to_viewport(&self, is_last: bool) -> bool {
        if !is_last || self.framebuffer_format != FramebufferFormat::Rgba8Unorm {
            return false;
        }
        let tx = self.scale_type_x.unwrap_or(ScaleType::Viewport);
        let ty = self.scale_type_y.unwrap_or(ScaleType::Viewport);
        let sx = self.scale_x.unwrap_or(1.0);
        let sy = self.scale_y.unwrap_or(1.0);
        tx == ScaleType::Viewport
            && ty == ScaleType::Viewport
            && (sx - 1.0).abs() < 1e-4
            && (sy - 1.0).abs() < 1e-4
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LutConfig {
    pub name: String,
    pub path: PathBuf,
    pub linear: bool,
    pub wrap_mode: WrapMode,
    pub mipmap: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShaderPreset {
    pub base_dir: PathBuf,
    pub passes: Vec<PassConfig>,
    pub luts: Vec<LutConfig>,
    pub parameters: BTreeMap<String, f32>,
}

/// Loads either a RetroArch `.glslp` preset or a standalone `.glsl` shader (with optional classic
/// cnc-ddraw `<file>.pass1` companion pass).
pub fn load_preset(path: &Path) -> Result<ShaderPreset, String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());

    if ext.as_deref() == Some("glsl") {
        return load_standalone_glsl(path);
    }
    load_glslp_recursive(path, 0)
}

fn load_standalone_glsl(path: &Path) -> Result<ShaderPreset, String> {
    if !path.is_file() {
        return Err(format!("shader file not found: {}", path.display()));
    }
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut pass0 = PassConfig::default_for_shader(path.to_path_buf());
    pass0.scale_type_x = Some(ScaleType::Viewport);
    pass0.scale_type_y = Some(ScaleType::Viewport);
    pass0.scale_x = Some(1.0);
    pass0.scale_y = Some(1.0);

    let mut passes = vec![pass0];

    // Classic cnc-ddraw convention: `<shader>.glsl.pass1` runs as a second pass.
    let mut pass1_os = path.as_os_str().to_os_string();
    pass1_os.push(".pass1");
    let pass1_path = PathBuf::from(pass1_os);
    if pass1_path.is_file() {
        let mut pass1 = PassConfig::default_for_shader(pass1_path);
        pass1.scale_type_x = Some(ScaleType::Viewport);
        pass1.scale_type_y = Some(ScaleType::Viewport);
        pass1.scale_x = Some(1.0);
        pass1.scale_y = Some(1.0);
        passes.push(pass1);
    }

    Ok(ShaderPreset {
        base_dir,
        passes,
        luts: Vec::new(),
        parameters: BTreeMap::new(),
    })
}

fn load_glslp_recursive(path: &Path, depth: usize) -> Result<ShaderPreset, String> {
    if depth > MAX_REFERENCE_DEPTH {
        return Err(format!(
            "exceeded maximum #reference depth ({MAX_REFERENCE_DEPTH}) at {}",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("reading shader preset {}: {err}", path.display()))?;
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    parse_glslp_str(&text, &base_dir, depth)
}

pub fn parse_glslp_str(text: &str, base_dir: &Path, depth: usize) -> Result<ShaderPreset, String> {
    let mut reference: Option<PathBuf> = None;
    let mut raw_pairs: Vec<(String, String)> = Vec::new();
    let mut kv: BTreeMap<String, String> = BTreeMap::new();
    let mut kv_lower: BTreeMap<String, String> = BTreeMap::new();

    for raw_line in text.lines() {
        let line = raw_line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#reference") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                let rel = unquote(trimmed);
                reference = Some(resolve_relative(base_dir, rel));
            }
            continue;
        }
        let cleaned = strip_hash_comment(line).trim();
        if cleaned.is_empty() {
            continue;
        }
        let Some((raw_key, raw_val)) = cleaned.split_once('=') else {
            continue;
        };
        let key = raw_key.trim().to_string();
        let val = unquote(raw_val.trim()).to_string();
        if key.is_empty() {
            continue;
        }
        kv_lower.insert(key.to_ascii_lowercase(), val.clone());
        kv.insert(key.clone(), val.clone());
        raw_pairs.push((key, val));
    }

    let mut preset = if let Some(ref_path) = reference {
        load_glslp_recursive(&ref_path, depth + 1)?
    } else {
        ShaderPreset {
            base_dir: base_dir.to_path_buf(),
            passes: Vec::new(),
            luts: Vec::new(),
            parameters: BTreeMap::new(),
        }
    };

    if let Some(count_str) = kv_lower.get("shaders") {
        let count: usize = count_str
            .parse()
            .map_err(|_| format!("invalid shaders count '{count_str}'"))?;
        if count == 0 {
            return Err("shader preset declares shaders = 0".into());
        }
        let mut passes = Vec::with_capacity(count);
        for i in 0..count {
            let shader_key = format!("shader{i}");
            let rel = kv_lower
                .get(&shader_key)
                .ok_or_else(|| format!("missing required key '{shader_key}' in preset"))?;
            let shader_path = resolve_relative(base_dir, rel);
            let mut pass = PassConfig::default_for_shader(shader_path);

            if let Some(alias) = kv_lower.get(&format!("alias{i}")) {
                if !alias.is_empty() {
                    pass.alias = Some(alias.clone());
                }
            }
            if let Some(v) = kv_lower.get(&format!("filter_linear{i}")) {
                pass.filter_linear = parse_bool(v);
            }
            if let Some(v) = kv_lower.get(&format!("wrap_mode{i}")) {
                if let Some(w) = parse_wrap_mode(v) {
                    pass.wrap_mode = w;
                }
            }
            if let Some(v) = kv_lower.get(&format!("scale_type{i}")) {
                let st = parse_scale_type(v);
                pass.scale_type_x = st;
                pass.scale_type_y = st;
            }
            if let Some(v) = kv_lower.get(&format!("scale_type_x{i}")) {
                if let Some(st) = parse_scale_type(v) {
                    pass.scale_type_x = Some(st);
                }
            }
            if let Some(v) = kv_lower.get(&format!("scale_type_y{i}")) {
                if let Some(st) = parse_scale_type(v) {
                    pass.scale_type_y = Some(st);
                }
            }
            if let Some(v) = kv_lower.get(&format!("scale{i}")) {
                if let Ok(s) = v.parse::<f32>() {
                    pass.scale_x = Some(s);
                    pass.scale_y = Some(s);
                }
            }
            if let Some(v) = kv_lower.get(&format!("scale_x{i}")) {
                if let Ok(s) = v.parse::<f32>() {
                    pass.scale_x = Some(s);
                }
            }
            if let Some(v) = kv_lower.get(&format!("scale_y{i}")) {
                if let Ok(s) = v.parse::<f32>() {
                    pass.scale_y = Some(s);
                }
            }
            let is_float = kv_lower
                .get(&format!("float_framebuffer{i}"))
                .and_then(|v| parse_bool(v))
                .unwrap_or(false);
            let is_srgb = kv_lower
                .get(&format!("srgb_framebuffer{i}"))
                .and_then(|v| parse_bool(v))
                .unwrap_or(false);
            pass.framebuffer_format = if is_float {
                FramebufferFormat::Rgba16Float
            } else if is_srgb {
                FramebufferFormat::Rgba8UnormSrgb
            } else {
                FramebufferFormat::Rgba8Unorm
            };
            if let Some(v) = kv_lower.get(&format!("frame_count_mod{i}")) {
                if let Ok(m) = v.parse::<u32>() {
                    if m > 0 {
                        pass.frame_count_mod = Some(m);
                    }
                }
            }
            if let Some(v) = kv_lower.get(&format!("mipmap_input{i}")) {
                pass.mipmap_input = parse_bool(v).unwrap_or(false);
            }

            passes.push(pass);
        }
        preset.passes = passes;
    }

    // Track LUT keys so they aren't misinterpreted as numeric parameters.
    let mut lut_names: Vec<String> = Vec::new();
    if let Some(tex_list) = kv_lower.get("textures") {
        for raw_name in tex_list.split(';') {
            let name = raw_name.trim();
            if name.is_empty() {
                continue;
            }
            lut_names.push(name.to_string());
            let path_val = kv
                .get(name)
                .or_else(|| kv_lower.get(&name.to_ascii_lowercase()));
            let Some(rel) = path_val else {
                continue;
            };
            let lut_path = resolve_relative(base_dir, rel);
            let linear = kv_lower
                .get(&format!("{}_linear", name.to_ascii_lowercase()))
                .and_then(|v| parse_bool(v))
                .unwrap_or(true);
            let wrap_mode = kv_lower
                .get(&format!("{}_wrap_mode", name.to_ascii_lowercase()))
                .and_then(|v| parse_wrap_mode(v))
                .unwrap_or(WrapMode::ClampToBorder);
            let mipmap = kv_lower
                .get(&format!("{}_mipmap", name.to_ascii_lowercase()))
                .and_then(|v| parse_bool(v))
                .unwrap_or(false);

            if let Some(existing) = preset.luts.iter_mut().find(|l| l.name == name) {
                existing.path = lut_path;
                existing.linear = linear;
                existing.wrap_mode = wrap_mode;
                existing.mipmap = mipmap;
            } else {
                preset.luts.push(LutConfig {
                    name: name.to_string(),
                    path: lut_path,
                    linear,
                    wrap_mode,
                    mipmap,
                });
            }
        }
    }

    // Collect parameter overrides (both explicitly listed in `parameters = "A;B"` and direct overrides).
    let pass_count = preset.passes.len();
    for (key, val) in &raw_pairs {
        let lower = key.to_ascii_lowercase();
        if is_reserved_preset_key(&lower, pass_count, &lut_names) {
            continue;
        }
        if let Ok(num) = val.parse::<f32>() {
            if num.is_finite() {
                preset.parameters.insert(key.clone(), num);
            }
        }
    }

    if preset.passes.is_empty() {
        return Err("shader preset contains no shader passes".into());
    }

    Ok(preset)
}

fn is_reserved_preset_key(lower: &str, pass_count: usize, lut_names: &[String]) -> bool {
    if matches!(
        lower,
        "shaders" | "textures" | "parameters" | "feedback_pass"
    ) {
        return true;
    }
    for name in lut_names {
        let ln = name.to_ascii_lowercase();
        if lower == ln
            || lower == format!("{ln}_linear")
            || lower == format!("{ln}_wrap_mode")
            || lower == format!("{ln}_mipmap")
        {
            return true;
        }
    }
    const PASS_PREFIXES: &[&str] = &[
        "shader",
        "alias",
        "filter_linear",
        "wrap_mode",
        "scale_type",
        "scale_type_x",
        "scale_type_y",
        "scale",
        "scale_x",
        "scale_y",
        "float_framebuffer",
        "srgb_framebuffer",
        "frame_count_mod",
        "mipmap_input",
    ];
    for prefix in PASS_PREFIXES {
        if let Some(suffix) = lower.strip_prefix(prefix) {
            if let Ok(idx) = suffix.parse::<usize>() {
                if idx < pass_count.max(64) {
                    return true;
                }
            }
        }
    }
    false
}

fn resolve_relative(base_dir: &Path, raw: &str) -> PathBuf {
    let normalized = raw.replace('\\', "/");
    let p = Path::new(&normalized);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

fn strip_hash_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn unquote(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_scale_type(s: &str) -> Option<ScaleType> {
    match s.trim().to_ascii_lowercase().as_str() {
        "source" => Some(ScaleType::Source),
        "viewport" => Some(ScaleType::Viewport),
        "absolute" => Some(ScaleType::Absolute),
        _ => None,
    }
}

fn parse_wrap_mode(s: &str) -> Option<WrapMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "clamp_to_border" => Some(WrapMode::ClampToBorder),
        "clamp_to_edge" => Some(WrapMode::ClampToEdge),
        "repeat" => Some(WrapMode::Repeat),
        "mirrored_repeat" => Some(WrapMode::MirroredRepeat),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multipass_preset_with_luts_and_parameters() {
        let preset_txt = r#"
            # Multi-pass CRT + scaling preset
            shaders = "2"

            shader0 = "shaders/pass0.glsl"
            alias0 = "FirstPass"
            filter_linear0 = "false"
            wrap_mode0 = "clamp_to_edge"
            scale_type_x0 = "source"
            scale_x0 = "2.0"
            scale_type_y0 = "absolute"
            scale_y0 = "480"
            float_framebuffer0 = "true"
            frame_count_mod0 = "60"

            shader1 = "shaders/pass1.glsl"
            filter_linear1 = "true"
            scale_type1 = "viewport"
            scale1 = "1.0"

            textures = "SamplerLUT1;SamplerLUT2"
            SamplerLUT1 = "luts/grade.png"
            SamplerLUT1_linear = "false"
            SamplerLUT1_wrap_mode = "repeat"
            SamplerLUT2 = "luts/mask.png"

            parameters = "SHARPNESS;SCANLINE_STRENGTH"
            SHARPNESS = "1.75"
            SCANLINE_STRENGTH = "0.5"
        "#;
        let base = Path::new("C:/games/shaders");
        let preset = parse_glslp_str(preset_txt, base, 0).unwrap();
        assert_eq!(preset.passes.len(), 2);

        let p0 = &preset.passes[0];
        assert_eq!(p0.shader_path, base.join("shaders/pass0.glsl"));
        assert_eq!(p0.alias.as_deref(), Some("FirstPass"));
        assert_eq!(p0.filter_linear, Some(false));
        assert_eq!(p0.wrap_mode, WrapMode::ClampToEdge);
        assert_eq!(p0.framebuffer_format, FramebufferFormat::Rgba16Float);
        assert_eq!(p0.frame_count_mod, Some(60));
        assert_eq!(
            p0.output_size(false, 320, 240, 1920, 1080, 8192),
            (640, 480)
        );
        assert!(!p0.renders_directly_to_viewport(false));

        let p1 = &preset.passes[1];
        assert_eq!(p1.filter_linear, Some(true));
        assert_eq!(
            p1.output_size(true, 640, 480, 1440, 1080, 8192),
            (1440, 1080)
        );
        assert!(p1.renders_directly_to_viewport(true));

        assert_eq!(preset.luts.len(), 2);
        assert_eq!(preset.luts[0].name, "SamplerLUT1");
        assert!(!preset.luts[0].linear);
        assert_eq!(preset.luts[0].wrap_mode, WrapMode::Repeat);
        assert_eq!(preset.luts[1].name, "SamplerLUT2");
        assert!(preset.luts[1].linear);

        assert_eq!(preset.parameters.get("SHARPNESS"), Some(&1.75));
        assert_eq!(preset.parameters.get("SCANLINE_STRENGTH"), Some(&0.5));
    }
}
