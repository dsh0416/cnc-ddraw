//! Legacy RetroArch GLSL (110–130) to Vulkan GLSL 450 translator and `naga` compiler.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::shader::preprocess::{preprocess_glsl, ShaderStage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UniformSemantic {
    MvpMatrix,
    TextureSize,
    InputSize,
    OutputSize,
    FrameCount,
    FrameDirection,
    OrigTextureSize,
    OrigInputSize,
    PassTextureSize(usize),
    PassInputSize(usize),
    PassPrevTextureSize(usize),
    PassPrevInputSize(usize),
    PrevTextureSize(usize),
    PrevInputSize(usize),
    AliasTextureSize(String),
    AliasInputSize(String),
    Parameter(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UniformType {
    Float,
    Int,
    Uint,
    Vec2,
    Vec3,
    Vec4,
    IVec2,
    UVec2,
    Mat2,
    Mat3,
    Mat4,
}

impl UniformType {
    fn from_glsl(name: &str) -> Option<Self> {
        match name {
            "float" => Some(Self::Float),
            "int" => Some(Self::Int),
            "uint" => Some(Self::Uint),
            "vec2" => Some(Self::Vec2),
            "vec3" => Some(Self::Vec3),
            "vec4" => Some(Self::Vec4),
            "ivec2" => Some(Self::IVec2),
            "uvec2" => Some(Self::UVec2),
            "mat2" => Some(Self::Mat2),
            "mat3" => Some(Self::Mat3),
            "mat4" => Some(Self::Mat4),
            _ => None,
        }
    }

    fn glsl_name(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Int => "int",
            Self::Uint => "uint",
            Self::Vec2 => "vec2",
            Self::Vec3 => "vec3",
            Self::Vec4 => "vec4",
            Self::IVec2 => "ivec2",
            Self::UVec2 => "uvec2",
            Self::Mat2 => "mat2",
            Self::Mat3 => "mat3",
            Self::Mat4 => "mat4",
        }
    }

    /// `(size, align)` under GLSL `std140` layout rules.
    fn std140_layout(self) -> (usize, usize) {
        match self {
            Self::Float | Self::Int | Self::Uint => (4, 4),
            Self::Vec2 | Self::IVec2 | Self::UVec2 => (8, 8),
            Self::Vec3 => (12, 16),
            Self::Vec4 => (16, 16),
            Self::Mat2 => (32, 16),
            Self::Mat3 => (48, 16),
            Self::Mat4 => (64, 16),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UniformField {
    pub name: String,
    pub ty: UniformType,
    pub offset: usize,
    pub semantic: UniformSemantic,
    pub default_param: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextureSemantic {
    Source,
    Original,
    PassOutput(usize),
    PassPrev(usize),
    PrevFrame(usize),
    Alias(String),
    Lut(String),
}

#[derive(Clone, Debug)]
pub struct TextureBinding {
    pub name: String,
    pub tex_binding: u32,
    pub smp_binding: u32,
    pub semantic: TextureSemantic,
}

pub struct CompiledPass {
    pub vertex_module: naga::Module,
    pub fragment_module: naga::Module,
    pub uniform_fields: Vec<UniformField>,
    pub uniform_size: usize,
    pub texture_bindings: Vec<TextureBinding>,
}

/// Translates a RetroArch `.glsl` shader file (containing `#if defined(VERTEX)` /
/// `#elif defined(FRAGMENT)`) into validated `naga` modules for `wgpu`.
pub fn compile_retroarch_glsl(
    raw_source: &str,
    base_dir: &Path,
    aliases: &[String],
    lut_names: &[String],
    preset_params: &BTreeMap<String, f32>,
) -> Result<CompiledPass, String> {
    let vs_pp = preprocess_glsl(raw_source, base_dir, ShaderStage::Vertex)
        .map_err(|e| format!("vertex preprocess: {e}"))?;
    let fs_pp = preprocess_glsl(raw_source, base_dir, ShaderStage::Fragment)
        .map_err(|e| format!("fragment preprocess: {e}"))?;

    let mut param_defaults: BTreeMap<String, f32> = BTreeMap::new();
    for p in vs_pp.parameters.iter().chain(fs_pp.parameters.iter()) {
        param_defaults.entry(p.name.clone()).or_insert(p.default);
    }
    for (k, &v) in preset_params {
        param_defaults.insert(k.clone(), v);
    }

    let vs_tokens = lex_glsl(&vs_pp.source);
    let fs_tokens = lex_glsl(&fs_pp.source);

    let mut pass_state = PassRewriteState::new(aliases, lut_names, &param_defaults);
    let vs_body = pass_state.rewrite_stage(&vs_tokens, ShaderStage::Vertex)?;
    let fs_body = pass_state.rewrite_stage(&fs_tokens, ShaderStage::Fragment)?;

    let (uniform_fields, uniform_size) = pass_state.finalize_uniforms();
    let vs_450 = build_stage_source(&vs_body, &uniform_fields, &pass_state.textures, ShaderStage::Vertex);
    let fs_450 = build_stage_source(&fs_body, &uniform_fields, &pass_state.textures, ShaderStage::Fragment);

    let vertex_module = parse_and_validate_glsl(&vs_450, naga::ShaderStage::Vertex)
        .map_err(|e| format!("vertex GLSL compile error: {e}"))?;
    let fragment_module = parse_and_validate_glsl(&fs_450, naga::ShaderStage::Fragment)
        .map_err(|e| format!("fragment GLSL compile error: {e}"))?;

    Ok(CompiledPass {
        vertex_module,
        fragment_module,
        uniform_fields,
        uniform_size,
        texture_bindings: pass_state.textures,
    })
}

fn parse_and_validate_glsl(
    source: &str,
    stage: naga::ShaderStage,
) -> Result<naga::Module, String> {
    let mut frontend = naga::front::glsl::Frontend::default();
    let options = naga::front::glsl::Options::from(stage);
    let module = frontend
        .parse(&options, source)
        .map_err(|errs| errs.emit_to_string(source))?;
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    );
    validator
        .validate(&module)
        .map_err(|err| err.emit_to_string(source))?;
    Ok(module)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Number(String),
    Punct(String),
}

impl Tok {
    fn as_str(&self) -> &str {
        match self {
            Self::Ident(s) | Self::Number(s) | Self::Punct(s) => s.as_str(),
        }
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self, Self::Punct(s) if s == p)
    }

    fn is_ident(&self, id: &str) -> bool {
        matches!(self, Self::Ident(s) if s == id)
    }
}

fn lex_glsl(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;

    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()) {
            let start = i;
            if c == b'0' && i + 1 < b.len() && matches!(b[i + 1], b'x' | b'X') {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
                while i < b.len() && matches!(b[i], b'u' | b'U') {
                    i += 1;
                }
            } else {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < b.len() && b[i] == b'.' {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < b.len() && matches!(b[i], b'e' | b'E') {
                    i += 1;
                    if i < b.len() && matches!(b[i], b'+' | b'-') {
                        i += 1;
                    }
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                while i < b.len() && matches!(b[i], b'f' | b'F' | b'u' | b'U' | b'l' | b'L') {
                    i += 1;
                }
            }
            out.push(Tok::Number(src[start..i].to_string()));
            continue;
        }

        // Three-character operators.
        if i + 2 < b.len() {
            let s3 = &src[i..i + 3];
            if matches!(s3, "<<=" | ">>=") {
                out.push(Tok::Punct(s3.to_string()));
                i += 3;
                continue;
            }
        }
        // Two-character operators.
        if i + 1 < b.len() {
            let s2 = &src[i..i + 2];
            if matches!(
                s2,
                "++" | "--"
                    | "=="
                    | "!="
                    | "<="
                    | ">="
                    | "&&"
                    | "||"
                    | "<<"
                    | ">>"
                    | "+="
                    | "-="
                    | "*="
                    | "/="
                    | "%="
                    | "&="
                    | "|="
                    | "^="
            ) {
                out.push(Tok::Punct(s2.to_string()));
                i += 2;
                continue;
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(Tok::Punct(ch.to_string()));
        i += ch.len_utf8();
    }
    out
}

struct PendingUniform {
    name: String,
    ty: UniformType,
    semantic: UniformSemantic,
    default_param: f32,
}

struct PassRewriteState<'a> {
    aliases: &'a [String],
    lut_names: &'a [String],
    param_defaults: &'a BTreeMap<String, f32>,
    uniforms: Vec<PendingUniform>,
    uniform_index: HashMap<String, usize>,
    textures: Vec<TextureBinding>,
    texture_index: HashMap<String, usize>,
    varying_locations: HashMap<String, u32>,
    next_varying_location: u32,
    next_extra_attr_location: u32,
}

impl<'a> PassRewriteState<'a> {
    fn new(
        aliases: &'a [String],
        lut_names: &'a [String],
        param_defaults: &'a BTreeMap<String, f32>,
    ) -> Self {
        Self {
            aliases,
            lut_names,
            param_defaults,
            uniforms: Vec::new(),
            uniform_index: HashMap::new(),
            textures: Vec::new(),
            texture_index: HashMap::new(),
            varying_locations: HashMap::new(),
            next_varying_location: 0,
            next_extra_attr_location: 3,
        }
    }

    fn ensure_uniform(&mut self, name: &str, ty: UniformType) {
        if self.uniform_index.contains_key(name) {
            return;
        }
        let semantic = classify_uniform_semantic(name, self.aliases, self.param_defaults);
        let default_param = self.param_defaults.get(name).copied().unwrap_or(0.0);
        let idx = self.uniforms.len();
        self.uniform_index.insert(name.to_string(), idx);
        self.uniforms.push(PendingUniform {
            name: name.to_string(),
            ty,
            semantic,
            default_param,
        });
    }

    fn ensure_texture(&mut self, name: &str) {
        if self.texture_index.contains_key(name) {
            return;
        }
        let idx = self.textures.len();
        let tex_binding = (idx as u32) * 2 + 1;
        let smp_binding = (idx as u32) * 2 + 2;
        let semantic = classify_texture_semantic(name, self.aliases, self.lut_names);
        self.texture_index.insert(name.to_string(), idx);
        self.textures.push(TextureBinding {
            name: name.to_string(),
            tex_binding,
            smp_binding,
            semantic,
        });
    }

    fn assign_varying_location(&mut self, name: &str, ty: &str, array_len: u32) -> u32 {
        if let Some(&loc) = self.varying_locations.get(name) {
            return loc;
        }
        let loc = self.next_varying_location;
        let per_elem = match ty {
            "mat2" => 2,
            "mat3" => 3,
            "mat4" => 4,
            _ => 1,
        };
        self.next_varying_location += per_elem * array_len.max(1);
        self.varying_locations.insert(name.to_string(), loc);
        loc
    }

    fn assign_vertex_attr_location(&mut self, name: &str) -> u32 {
        match name {
            "VertexCoord" => 0,
            "TexCoord" | "LUTTexCoord" => 1,
            "COLOR" => 2,
            _ => {
                let loc = self.next_extra_attr_location;
                self.next_extra_attr_location += 1;
                loc
            }
        }
    }

    fn finalize_uniforms(&self) -> (Vec<UniformField>, usize) {
        let mut fields = Vec::with_capacity(self.uniforms.len());
        let mut offset = 0usize;
        for u in &self.uniforms {
            let (size, align) = u.ty.std140_layout();
            offset = align_up(offset, align);
            fields.push(UniformField {
                name: u.name.clone(),
                ty: u.ty,
                offset,
                semantic: u.semantic.clone(),
                default_param: u.default_param,
            });
            offset += size;
        }
        let total_size = align_up(offset, 16).max(16);
        (fields, total_size)
    }

    fn rewrite_stage(&mut self, raw_tokens: &[Tok], stage: ShaderStage) -> Result<String, String> {
        // Pre-pass 1: strip inline precision qualifiers (`lowp`/`mediump`/`highp`) and standalone
        // `precision <prec> <type>;` statements.
        let tokens = strip_precision_tokens(raw_tokens);

        // Pre-pass 2: scan top-level declarations (uniforms, samplers, helper functions with
        // sampler2D parameters, legacy fixed-function builtins, explicit fragment outputs).
        let mut sampler_fn_params: HashMap<String, Vec<usize>> = HashMap::new();
        let mut declared_attrs: HashSet<String> = HashSet::new();
        let mut has_explicit_frag_out = false;
        let mut uses_gl_frag_color = false;
        let mut uses_legacy_vertex = (false, false, false, false); // Vertex, TexCoord0, Color, MVP

        for tok in &tokens {
            if let Tok::Ident(id) = tok {
                match id.as_str() {
                    "gl_FragColor" | "gl_FragData" if stage == ShaderStage::Fragment => {
                        uses_gl_frag_color = true;
                    }
                    "gl_Vertex" if stage == ShaderStage::Vertex => uses_legacy_vertex.0 = true,
                    "gl_MultiTexCoord0" if stage == ShaderStage::Vertex => {
                        uses_legacy_vertex.1 = true;
                    }
                    "gl_Color" if stage == ShaderStage::Vertex => uses_legacy_vertex.2 = true,
                    "gl_ModelViewProjectionMatrix" if stage == ShaderStage::Vertex => {
                        uses_legacy_vertex.3 = true;
                    }
                    _ => {}
                }
            }
        }

        // Parse top-level items cleanly.
        let mut stage_decls: Vec<String> = Vec::new();
        let mut body_items: Vec<TopItem> = Vec::new();

        let mut i = 0usize;
        while i < tokens.len() {
            if tokens[i].is_punct(";") {
                i += 1;
                continue;
            }
            if let Some((decl, next_i)) = try_parse_top_var_decl(&tokens, i) {
                if let Some(storage) = decl.storage {
                    match (stage, storage) {
                        (_, StorageKind::Uniform) => {
                            if decl.ty == "sampler2D" {
                                for d in &decl.declarators {
                                    self.ensure_texture(&d.name);
                                }
                            } else if let Some(uty) = UniformType::from_glsl(&decl.ty) {
                                for d in &decl.declarators {
                                    self.ensure_uniform(&d.name, uty);
                                }
                            } else {
                                return Err(format!(
                                    "unsupported uniform type '{}' in shader",
                                    decl.ty
                                ));
                            }
                            i = next_i;
                            continue;
                        }
                        (ShaderStage::Vertex, StorageKind::Attribute | StorageKind::In) => {
                            for d in &decl.declarators {
                                declared_attrs.insert(d.name.clone());
                                let loc = self.assign_vertex_attr_location(&d.name);
                                stage_decls.push(format!(
                                    "layout(location = {loc}) in {} {}{};",
                                    decl.ty, d.name, d.array_suffix
                                ));
                            }
                            i = next_i;
                            continue;
                        }
                        (ShaderStage::Vertex, StorageKind::Varying | StorageKind::Out) => {
                            for d in &decl.declarators {
                                let loc =
                                    self.assign_varying_location(&d.name, &decl.ty, d.array_len);
                                stage_decls.push(format!(
                                    "layout(location = {loc}) {}out {} {}{};",
                                    decl.interp_prefix(),
                                    decl.ty,
                                    d.name,
                                    d.array_suffix
                                ));
                            }
                            i = next_i;
                            continue;
                        }
                        (ShaderStage::Fragment, StorageKind::Varying | StorageKind::In) => {
                            for d in &decl.declarators {
                                let loc =
                                    self.assign_varying_location(&d.name, &decl.ty, d.array_len);
                                stage_decls.push(format!(
                                    "layout(location = {loc}) {}in {} {}{};",
                                    decl.interp_prefix(),
                                    decl.ty,
                                    d.name,
                                    d.array_suffix
                                ));
                            }
                            i = next_i;
                            continue;
                        }
                        (ShaderStage::Fragment, StorageKind::Out) => {
                            has_explicit_frag_out = true;
                            for (idx, d) in decl.declarators.iter().enumerate() {
                                stage_decls.push(format!(
                                    "layout(location = {idx}) out {} {}{};",
                                    decl.ty, d.name, d.array_suffix
                                ));
                            }
                            i = next_i;
                            continue;
                        }
                        (ShaderStage::Fragment, StorageKind::Attribute) => {
                            // Ignore stray vertex attributes if left in fragment section.
                            i = next_i;
                            continue;
                        }
                    }
                }
            }

            // Otherwise consume a top-level item (function definition, prototype, struct, const/global var).
            let (item_end, fn_info) = scan_top_item(&tokens, i);
            if let Some((fn_name, sampler_indices)) = &fn_info {
                if !sampler_indices.is_empty() {
                    sampler_fn_params.insert(fn_name.clone(), sampler_indices.clone());
                }
            }
            body_items.push(TopItem {
                start: i,
                end: item_end,
            });
            i = item_end;
        }

        // Auto-inject legacy fixed-function inputs/outputs if referenced.
        if stage == ShaderStage::Vertex {
            if uses_legacy_vertex.0 && !declared_attrs.contains("VertexCoord") {
                stage_decls.push("layout(location = 0) in vec4 VertexCoord;".into());
            }
            if uses_legacy_vertex.1 && !declared_attrs.contains("TexCoord") {
                stage_decls.push("layout(location = 1) in vec4 TexCoord;".into());
            }
            if uses_legacy_vertex.2 && !declared_attrs.contains("COLOR") {
                stage_decls.push("layout(location = 2) in vec4 COLOR;".into());
            }
            if uses_legacy_vertex.3 {
                self.ensure_uniform("MVPMatrix", UniformType::Mat4);
            }
        }
        if stage == ShaderStage::Fragment && uses_gl_frag_color && !has_explicit_frag_out {
            stage_decls.push("layout(location = 0) out vec4 _ra_frag_color;".into());
        }

        // Rewrite all retained top-level items (bodies, helper functions, global constants, structs).
        let global_samplers: HashSet<String> =
            self.textures.iter().map(|t| t.name.clone()).collect();
        let global_uniforms: HashSet<String> =
            self.uniforms.iter().map(|u| u.name.clone()).collect();

        let mut out_tokens: Vec<Tok> = Vec::new();
        for item in body_items {
            let slice = &tokens[item.start..item.end];
            rewrite_top_item(
                slice,
                stage,
                !has_explicit_frag_out,
                &global_samplers,
                &global_uniforms,
                &sampler_fn_params,
                &mut out_tokens,
            );
        }

        let mut stage_out = String::new();
        for decl in stage_decls {
            stage_out.push_str(&decl);
            stage_out.push('\n');
        }
        stage_out.push_str(&format_tokens(&out_tokens));
        Ok(stage_out)
    }
}

fn build_stage_source(
    stage_body: &str,
    uniform_fields: &[UniformField],
    textures: &[TextureBinding],
    _stage: ShaderStage,
) -> String {
    let mut out = String::with_capacity(stage_body.len() + 512);
    out.push_str("#version 450\n");

    if !uniform_fields.is_empty() && stage_body.contains("_ra_u.") {
        out.push_str("layout(std140, set = 0, binding = 0) uniform _RaUniforms {\n");
        for f in uniform_fields {
            out.push_str("    ");
            out.push_str(f.ty.glsl_name());
            out.push(' ');
            out.push_str(&f.name);
            out.push_str(";\n");
        }
        out.push_str("} _ra_u;\n");
    }

    for t in textures {
        let tex_sym = format!("_ra_tex_{}", t.name);
        let smp_sym = format!("_ra_smp_{}", t.name);
        if stage_body.contains(&tex_sym) {
            out.push_str(&format!(
                "layout(set = 0, binding = {}) uniform texture2D {};\n",
                t.tex_binding, tex_sym
            ));
        }
        if stage_body.contains(&smp_sym) {
            out.push_str(&format!(
                "layout(set = 0, binding = {}) uniform sampler {};\n",
                t.smp_binding, smp_sym
            ));
        }
    }

    out.push_str(stage_body);
    out
}

fn strip_precision_tokens(tokens: &[Tok]) -> Vec<Tok> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].is_ident("precision") {
            while i < tokens.len() && !tokens[i].is_punct(";") {
                i += 1;
            }
            if i < tokens.len() {
                i += 1;
            }
            continue;
        }
        if matches!(
            tokens[i].as_str(),
            "lowp" | "mediump" | "highp" | "COMPAT_PRECISION"
        ) && matches!(tokens[i], Tok::Ident(_))
        {
            i += 1;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageKind {
    Uniform,
    Attribute,
    Varying,
    In,
    Out,
}

struct Declarator {
    name: String,
    array_suffix: String,
    array_len: u32,
}

struct TopVarDecl {
    storage: Option<StorageKind>,
    interp: Vec<String>,
    ty: String,
    declarators: Vec<Declarator>,
}

impl TopVarDecl {
    fn interp_prefix(&self) -> String {
        if self.interp.is_empty() {
            String::new()
        } else {
            let mut s = self.interp.join(" ");
            s.push(' ');
            s
        }
    }
}

fn try_parse_top_var_decl(tokens: &[Tok], start: usize) -> Option<(TopVarDecl, usize)> {
    let mut i = start;
    let mut storage: Option<StorageKind> = None;
    let mut interp: Vec<String> = Vec::new();

    // Parse leading qualifiers (optional `layout(...)`, interpolation, storage).
    loop {
        let tok = tokens.get(i)?;
        if tok.is_ident("layout") && tokens.get(i + 1).is_some_and(|t| t.is_punct("(")) {
            let close = find_matching_paren(tokens, i + 1)?;
            i = close + 1;
            continue;
        }
        if let Tok::Ident(id) = tok {
            match id.as_str() {
                "flat" | "smooth" | "noperspective" | "centroid" | "sample" | "invariant" => {
                    interp.push(id.clone());
                    i += 1;
                    continue;
                }
                "uniform" => {
                    storage = Some(StorageKind::Uniform);
                    i += 1;
                    continue;
                }
                "attribute" => {
                    storage = Some(StorageKind::Attribute);
                    i += 1;
                    continue;
                }
                "varying" => {
                    storage = Some(StorageKind::Varying);
                    i += 1;
                    continue;
                }
                "in" => {
                    storage = Some(StorageKind::In);
                    i += 1;
                    continue;
                }
                "out" => {
                    storage = Some(StorageKind::Out);
                    i += 1;
                    continue;
                }
                _ => break,
            }
        }
        break;
    }

    storage?;

    // Next token must be a type identifier (not a `{` interface block).
    let Tok::Ident(ty) = tokens.get(i)? else {
        return None;
    };
    i += 1;
    // Skip interface blocks like `uniform BlockName { ... } inst;`.
    if tokens.get(i).is_some_and(|t| t.is_punct("{")) {
        return None;
    }

    let mut declarators = Vec::new();
    loop {
        let Tok::Ident(name) = tokens.get(i)? else {
            return None;
        };
        i += 1;
        // Function definition/prototype, not a variable declaration.
        if tokens.get(i).is_some_and(|t| t.is_punct("(")) {
            return None;
        }
        let mut array_suffix = String::new();
        let mut array_len = 1u32;
        if tokens.get(i).is_some_and(|t| t.is_punct("[")) {
            let close = find_matching_bracket(tokens, i)?;
            let inner: String = tokens[i + 1..close]
                .iter()
                .map(Tok::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if let Ok(n) = inner.trim().parse::<u32>() {
                array_len = n.max(1);
            }
            array_suffix = format!("[{inner}]");
            i = close + 1;
        }
        // Optional `= initializer` (e.g. default uniform initializer in GLSL 120/420).
        if tokens.get(i).is_some_and(|t| t.is_punct("=")) {
            i += 1;
            let mut paren = 0usize;
            while i < tokens.len() {
                if tokens[i].is_punct("(") || tokens[i].is_punct("[") {
                    paren += 1;
                } else if tokens[i].is_punct(")") || tokens[i].is_punct("]") {
                    paren = paren.saturating_sub(1);
                } else if paren == 0 && (tokens[i].is_punct(",") || tokens[i].is_punct(";")) {
                    break;
                }
                i += 1;
            }
        }
        declarators.push(Declarator {
            name: name.clone(),
            array_suffix,
            array_len,
        });
        if tokens.get(i).is_some_and(|t| t.is_punct(",")) {
            i += 1;
            continue;
        }
        if tokens.get(i).is_some_and(|t| t.is_punct(";")) {
            i += 1;
            return Some((
                TopVarDecl {
                    storage,
                    interp,
                    ty: ty.clone(),
                    declarators,
                },
                i,
            ));
        }
        return None;
    }
}

struct TopItem {
    start: usize,
    end: usize,
}

fn scan_top_item(tokens: &[Tok], start: usize) -> (usize, Option<(String, Vec<usize>)>) {
    // Check if this item starts a function header: `<ret_type> <name> ( ... ) { ... }` or `;`.
    let mut fn_info = None;
    if let (Some(Tok::Ident(_ret)), Some(Tok::Ident(name)), Some(lp)) =
        (tokens.get(start), tokens.get(start + 1), tokens.get(start + 2))
    {
        if lp.is_punct("(") {
            if let Some(rp) = find_matching_paren(tokens, start + 2) {
                let sampler_indices = find_sampler_param_indices(&tokens[start + 3..rp]);
                fn_info = Some((name.clone(), sampler_indices));
            }
        }
    }

    let mut i = start;
    let mut brace_depth = 0usize;
    let mut saw_brace = false;
    while i < tokens.len() {
        if tokens[i].is_punct("{") {
            brace_depth += 1;
            saw_brace = true;
        } else if tokens[i].is_punct("}") {
            brace_depth = brace_depth.saturating_sub(1);
            if brace_depth == 0 && saw_brace {
                i += 1;
                // Struct definitions end with `};` or `} var_name;`.
                if tokens.get(start).is_some_and(|t| t.is_ident("struct")) {
                    while i < tokens.len() && !tokens[i].is_punct(";") {
                        i += 1;
                    }
                    if i < tokens.len() {
                        i += 1;
                    }
                }
                return (i, fn_info);
            }
        } else if tokens[i].is_punct(";") && brace_depth == 0 {
            return (i + 1, fn_info);
        }
        i += 1;
    }
    (tokens.len(), fn_info)
}

fn find_sampler_param_indices(param_tokens: &[Tok]) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut param_idx = 0usize;
    let mut cur_has_sampler = false;
    let mut depth = 0usize;
    for tok in param_tokens {
        if tok.is_punct("(") || tok.is_punct("[") {
            depth += 1;
        } else if tok.is_punct(")") || tok.is_punct("]") {
            depth = depth.saturating_sub(1);
        } else if tok.is_punct(",") && depth == 0 {
            if cur_has_sampler {
                indices.push(param_idx);
            }
            param_idx += 1;
            cur_has_sampler = false;
        } else if depth == 0 && tok.is_ident("sampler2D") {
            cur_has_sampler = true;
        }
    }
    if cur_has_sampler {
        indices.push(param_idx);
    }
    indices
}

fn rewrite_top_item(
    tokens: &[Tok],
    stage: ShaderStage,
    inject_frag_color: bool,
    global_samplers: &HashSet<String>,
    global_uniforms: &HashSet<String>,
    sampler_fn_params: &HashMap<String, Vec<usize>>,
    out: &mut Vec<Tok>,
) {
    let mut local_samplers: HashSet<String> = HashSet::new();
    let mut shadowed_scopes: Vec<HashSet<String>> = vec![HashSet::new()];

    // Check if this is a function definition/prototype and rewrite any `sampler2D` parameters.
    let mut body_start = 0usize;
    if let (Some(Tok::Ident(_ret)), Some(Tok::Ident(_name)), Some(lp)) =
        (tokens.first(), tokens.get(1), tokens.get(2))
    {
        if lp.is_punct("(") {
            if let Some(rp) = find_matching_paren(tokens, 2) {
                out.push(tokens[0].clone());
                out.push(tokens[1].clone());
                out.push(tokens[2].clone());
                rewrite_fn_param_list(
                    &tokens[3..rp],
                    &mut local_samplers,
                    shadowed_scopes.last_mut().unwrap(),
                    out,
                );
                out.push(tokens[rp].clone());
                body_start = rp + 1;
            }
        }
    }

    rewrite_expr_slice(
        &tokens[body_start..],
        stage,
        inject_frag_color,
        global_samplers,
        &local_samplers,
        global_uniforms,
        sampler_fn_params,
        &mut shadowed_scopes,
        out,
    );
}

fn rewrite_fn_param_list(
    params: &[Tok],
    local_samplers: &mut HashSet<String>,
    shadowed: &mut HashSet<String>,
    out: &mut Vec<Tok>,
) {
    let mut segments: Vec<&[Tok]> = Vec::new();
    let mut seg_start = 0;
    let mut depth = 0usize;
    for (idx, tok) in params.iter().enumerate() {
        if tok.is_punct("(") || tok.is_punct("[") {
            depth += 1;
        } else if tok.is_punct(")") || tok.is_punct("]") {
            depth = depth.saturating_sub(1);
        } else if tok.is_punct(",") && depth == 0 {
            segments.push(&params[seg_start..idx]);
            seg_start = idx + 1;
        }
    }
    if seg_start < params.len() {
        segments.push(&params[seg_start..]);
    }

    for (seg_idx, seg) in segments.iter().enumerate() {
        if seg_idx > 0 {
            out.push(Tok::Punct(",".into()));
        }
        if let Some(s_pos) = seg.iter().position(|t| t.is_ident("sampler2D")) {
            let param_name = seg
                .get(s_pos + 1)
                .and_then(|t| match t {
                    Tok::Ident(id) => Some(id.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| format!("_anon_smp_{seg_idx}"));
            local_samplers.insert(param_name.clone());
            shadowed.insert(param_name.clone());
            out.push(Tok::Ident("texture2D".into()));
            out.push(Tok::Ident(format!("_ra_tex_{param_name}")));
            out.push(Tok::Punct(",".into()));
            out.push(Tok::Ident("sampler".into()));
            out.push(Tok::Ident(format!("_ra_smp_{param_name}")));
        } else {
            // Track parameter identifier so it isn't mistaken for a global uniform.
            if let Some(Tok::Ident(last_id)) = seg.last() {
                if !is_glsl_type_keyword(last_id) {
                    shadowed.insert(last_id.clone());
                }
            }
            out.extend(seg.iter().cloned());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rewrite_expr_slice(
    tokens: &[Tok],
    stage: ShaderStage,
    inject_frag_color: bool,
    global_samplers: &HashSet<String>,
    local_samplers: &HashSet<String>,
    global_uniforms: &HashSet<String>,
    sampler_fn_params: &HashMap<String, Vec<usize>>,
    shadowed_scopes: &mut Vec<HashSet<String>>,
    out: &mut Vec<Tok>,
) {
    let is_known_sampler =
        |name: &str| global_samplers.contains(name) || local_samplers.contains(name);
    let is_shadowed = |scopes: &[HashSet<String>], name: &str| {
        scopes.iter().any(|s| s.contains(name))
    };

    let mut i = 0usize;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.is_punct("{") {
            shadowed_scopes.push(HashSet::new());
            out.push(tok.clone());
            i += 1;
            continue;
        }
        if tok.is_punct("}") {
            if shadowed_scopes.len() > 1 {
                shadowed_scopes.pop();
            }
            out.push(tok.clone());
            i += 1;
            continue;
        }

        let prev_is_dot = i > 0 && tokens[i - 1].is_punct(".");

        // Track simple local variable declarations (`<type> <ident>` at statement start) so
        // locals that happen to share a name with a global uniform are not rewritten.
        if !prev_is_dot {
            if let Tok::Ident(ty) = tok {
                if is_glsl_type_keyword(ty) {
                    if let Some(Tok::Ident(var_name)) = tokens.get(i + 1) {
                        let after_var = tokens.get(i + 2);
                        let is_decl = after_var.is_some_and(|t| {
                            t.is_punct("=")
                                || t.is_punct(";")
                                || t.is_punct(",")
                                || t.is_punct("[")
                        });
                        if is_decl {
                            if let Some(scope) = shadowed_scopes.last_mut() {
                                scope.insert(var_name.clone());
                            }
                        }
                    }
                }
            }
        }

        if let Tok::Ident(id) = tok {
            if !prev_is_dot {
                // Legacy fragment output builtins.
                if stage == ShaderStage::Fragment && inject_frag_color {
                    if id == "gl_FragColor" {
                        out.push(Tok::Ident("_ra_frag_color".into()));
                        i += 1;
                        continue;
                    }
                    if id == "gl_FragData"
                        && tokens.get(i + 1).is_some_and(|t| t.is_punct("["))
                        && tokens.get(i + 2).is_some_and(|t| t.as_str() == "0")
                        && tokens.get(i + 3).is_some_and(|t| t.is_punct("]"))
                    {
                        out.push(Tok::Ident("_ra_frag_color".into()));
                        i += 4;
                        continue;
                    }
                }

                // Legacy vertex builtins.
                if stage == ShaderStage::Vertex {
                    match id.as_str() {
                        "gl_Vertex" => {
                            out.push(Tok::Ident("VertexCoord".into()));
                            i += 1;
                            continue;
                        }
                        "gl_MultiTexCoord0" => {
                            out.push(Tok::Ident("TexCoord".into()));
                            i += 1;
                            continue;
                        }
                        "gl_Color" => {
                            out.push(Tok::Ident("COLOR".into()));
                            i += 1;
                            continue;
                        }
                        "gl_ModelViewProjectionMatrix" => {
                            out.push(Tok::Ident("_ra_u".into()));
                            out.push(Tok::Punct(".".into()));
                            out.push(Tok::Ident("MVPMatrix".into()));
                            i += 1;
                            continue;
                        }
                        _ => {}
                    }
                }

                // Function calls (texture builtins or helper functions taking sampler2D).
                if tokens.get(i + 1).is_some_and(|t| t.is_punct("(")) {
                    if let Some(rp) = find_matching_paren(tokens, i + 1) {
                        let mapped_combined = match id.as_str() {
                            "texture2D" | "texture" => Some("texture"),
                            "texture2DLod" | "textureLod" => Some("textureLod"),
                            "texture2DProj" | "textureProj" => Some("textureProj"),
                            "texture2DGradEXT" | "texture2DGradARB" | "textureGrad" => {
                                Some("textureGrad")
                            }
                            "textureOffset" => Some("textureOffset"),
                            "textureLodOffset" => Some("textureLodOffset"),
                            "textureGradOffset" => Some("textureGradOffset"),
                            "textureProjLod" => Some("textureProjLod"),
                            "textureProjOffset" => Some("textureProjOffset"),
                            _ => None,
                        };
                        if let Some(new_fn) = mapped_combined {
                            let args = split_call_args(&tokens[i + 2..rp]);
                            out.push(Tok::Ident(new_fn.into()));
                            out.push(Tok::Punct("(".into()));
                            for (a_idx, arg) in args.iter().enumerate() {
                                if a_idx > 0 {
                                    out.push(Tok::Punct(",".into()));
                                }
                                if a_idx == 0 {
                                    if let Some(smp_name) = unwrap_single_ident(arg) {
                                        if is_known_sampler(smp_name) {
                                            out.push(Tok::Ident("sampler2D".into()));
                                            out.push(Tok::Punct("(".into()));
                                            out.push(Tok::Ident(format!("_ra_tex_{smp_name}")));
                                            out.push(Tok::Punct(",".into()));
                                            out.push(Tok::Ident(format!("_ra_smp_{smp_name}")));
                                            out.push(Tok::Punct(")".into()));
                                            continue;
                                        }
                                    }
                                }
                                rewrite_expr_slice(
                                    arg,
                                    stage,
                                    inject_frag_color,
                                    global_samplers,
                                    local_samplers,
                                    global_uniforms,
                                    sampler_fn_params,
                                    shadowed_scopes,
                                    out,
                                );
                            }
                            out.push(Tok::Punct(")".into()));
                            i = rp + 1;
                            continue;
                        }

                        if matches!(
                            id.as_str(),
                            "texelFetch" | "texelFetchOffset" | "textureSize" | "textureQueryLevels"
                        ) {
                            let args = split_call_args(&tokens[i + 2..rp]);
                            out.push(Tok::Ident(id.clone()));
                            out.push(Tok::Punct("(".into()));
                            for (a_idx, arg) in args.iter().enumerate() {
                                if a_idx > 0 {
                                    out.push(Tok::Punct(",".into()));
                                }
                                if a_idx == 0 {
                                    if let Some(smp_name) = unwrap_single_ident(arg) {
                                        if is_known_sampler(smp_name) {
                                            out.push(Tok::Ident(format!("_ra_tex_{smp_name}")));
                                            continue;
                                        }
                                    }
                                }
                                rewrite_expr_slice(
                                    arg,
                                    stage,
                                    inject_frag_color,
                                    global_samplers,
                                    local_samplers,
                                    global_uniforms,
                                    sampler_fn_params,
                                    shadowed_scopes,
                                    out,
                                );
                            }
                            out.push(Tok::Punct(")".into()));
                            i = rp + 1;
                            continue;
                        }

                        if let Some(smp_indices) = sampler_fn_params.get(id) {
                            let args = split_call_args(&tokens[i + 2..rp]);
                            out.push(Tok::Ident(id.clone()));
                            out.push(Tok::Punct("(".into()));
                            for (a_idx, arg) in args.iter().enumerate() {
                                if a_idx > 0 {
                                    out.push(Tok::Punct(",".into()));
                                }
                                if smp_indices.contains(&a_idx) {
                                    if let Some(smp_name) = unwrap_single_ident(arg) {
                                        if is_known_sampler(smp_name) {
                                            out.push(Tok::Ident(format!("_ra_tex_{smp_name}")));
                                            out.push(Tok::Punct(",".into()));
                                            out.push(Tok::Ident(format!("_ra_smp_{smp_name}")));
                                            continue;
                                        }
                                    }
                                }
                                rewrite_expr_slice(
                                    arg,
                                    stage,
                                    inject_frag_color,
                                    global_samplers,
                                    local_samplers,
                                    global_uniforms,
                                    sampler_fn_params,
                                    shadowed_scopes,
                                    out,
                                );
                            }
                            out.push(Tok::Punct(")".into()));
                            i = rp + 1;
                            continue;
                        }
                    }
                } else if global_uniforms.contains(id) && !is_shadowed(shadowed_scopes, id) {
                    out.push(Tok::Ident("_ra_u".into()));
                    out.push(Tok::Punct(".".into()));
                    out.push(Tok::Ident(id.clone()));
                    i += 1;
                    continue;
                }
            }
        }

        out.push(tok.clone());
        i += 1;
    }
}

fn find_matching_paren(tokens: &[Tok], lparen: usize) -> Option<usize> {
    if !tokens.get(lparen)?.is_punct("(") {
        return None;
    }
    let mut depth = 1usize;
    let mut i = lparen + 1;
    while i < tokens.len() {
        if tokens[i].is_punct("(") {
            depth += 1;
        } else if tokens[i].is_punct(")") {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn find_matching_bracket(tokens: &[Tok], lbracket: usize) -> Option<usize> {
    if !tokens.get(lbracket)?.is_punct("[") {
        return None;
    }
    let mut depth = 1usize;
    let mut i = lbracket + 1;
    while i < tokens.len() {
        if tokens[i].is_punct("[") {
            depth += 1;
        } else if tokens[i].is_punct("]") {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn split_call_args(tokens: &[Tok]) -> Vec<&[Tok]> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.is_punct("(") || tok.is_punct("[") {
            depth += 1;
        } else if tok.is_punct(")") || tok.is_punct("]") {
            depth = depth.saturating_sub(1);
        } else if tok.is_punct(",") && depth == 0 {
            args.push(&tokens[start..i]);
            start = i + 1;
        }
    }
    args.push(&tokens[start..]);
    args
}

fn unwrap_single_ident(mut slice: &[Tok]) -> Option<&str> {
    while slice.len() >= 2 && slice.first()?.is_punct("(") && slice.last()?.is_punct(")") {
        if find_matching_paren(slice, 0) == Some(slice.len() - 1) {
            slice = &slice[1..slice.len() - 1];
        } else {
            break;
        }
    }
    if slice.len() == 1 {
        if let Tok::Ident(ref id) = slice[0] {
            return Some(id.as_str());
        }
    }
    None
}

fn is_glsl_type_keyword(s: &str) -> bool {
    matches!(
        s,
        "float"
            | "int"
            | "uint"
            | "bool"
            | "vec2"
            | "vec3"
            | "vec4"
            | "ivec2"
            | "ivec3"
            | "ivec4"
            | "uvec2"
            | "uvec3"
            | "uvec4"
            | "bvec2"
            | "bvec3"
            | "bvec4"
            | "mat2"
            | "mat3"
            | "mat4"
            | "mat2x2"
            | "mat2x3"
            | "mat2x4"
            | "mat3x2"
            | "mat3x3"
            | "mat3x4"
            | "mat4x2"
            | "mat4x3"
            | "mat4x4"
    )
}

fn format_tokens(tokens: &[Tok]) -> String {
    let mut out = String::with_capacity(tokens.len() * 4);
    for (idx, tok) in tokens.iter().enumerate() {
        if idx > 0 {
            let prev = &tokens[idx - 1];
            let needs_space = match (prev, tok) {
                (Tok::Ident(_) | Tok::Number(_), Tok::Ident(_) | Tok::Number(_)) => true,
                (Tok::Punct(p), _) if p == "," => true,
                (Tok::Punct(p), Tok::Punct(q))
                    if matches!(
                        (p.as_str(), q.as_str()),
                        ("+", "+")
                            | ("-", "-")
                            | ("+", "=")
                            | ("-", "=")
                            | ("=", "=")
                            | ("!", "=")
                            | ("<", "<")
                            | ("<", "=")
                            | (">", ">")
                            | (">", "=")
                            | ("&", "&")
                            | ("|", "|")
                            | ("/", "/")
                            | ("/", "*")
                    ) =>
                {
                    true
                }
                _ => false,
            };
            if needs_space {
                out.push(' ');
            }
        }
        out.push_str(tok.as_str());
        if tok.is_punct(";") || tok.is_punct("{") || tok.is_punct("}") {
            out.push('\n');
        }
    }
    out
}

fn align_up(offset: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (offset + align - 1) & !(align - 1)
}

fn classify_texture_semantic(
    name: &str,
    aliases: &[String],
    lut_names: &[String],
) -> TextureSemantic {
    if let Some(lut) = lut_names.iter().find(|l| l.as_str() == name) {
        return TextureSemantic::Lut(lut.clone());
    }
    match name {
        "Texture" => return TextureSemantic::Source,
        "OrigTexture" => return TextureSemantic::Original,
        "PrevTexture" => return TextureSemantic::PrevFrame(0),
        _ => {}
    }
    if let Some(stem) = name.strip_suffix("Texture") {
        if let Some(n_str) = stem.strip_prefix("PassPrev") {
            if let Ok(n) = n_str.parse::<usize>() {
                if n >= 1 {
                    return TextureSemantic::PassPrev(n);
                }
            }
        }
        if let Some(n_str) = stem.strip_prefix("Pass") {
            if let Ok(n) = n_str.parse::<usize>() {
                if n >= 1 {
                    return TextureSemantic::PassOutput(n - 1);
                }
            }
        }
        if let Some(n_str) = stem.strip_prefix("Prev") {
            if let Ok(n) = n_str.parse::<usize>() {
                return TextureSemantic::PrevFrame(n);
            }
        }
        if let Some(alias) = aliases.iter().find(|a| a.as_str() == stem) {
            return TextureSemantic::Alias(alias.clone());
        }
    }
    if let Some(lut) = lut_names.iter().find(|l| l.eq_ignore_ascii_case(name)) {
        return TextureSemantic::Lut(lut.clone());
    }
    TextureSemantic::Source
}

fn classify_uniform_semantic(
    name: &str,
    aliases: &[String],
    param_defaults: &BTreeMap<String, f32>,
) -> UniformSemantic {
    match name {
        "MVPMatrix" => return UniformSemantic::MvpMatrix,
        "TextureSize" => return UniformSemantic::TextureSize,
        "InputSize" => return UniformSemantic::InputSize,
        "OutputSize" => return UniformSemantic::OutputSize,
        "FrameCount" => return UniformSemantic::FrameCount,
        "FrameDirection" => return UniformSemantic::FrameDirection,
        "OrigTextureSize" => return UniformSemantic::OrigTextureSize,
        "OrigInputSize" => return UniformSemantic::OrigInputSize,
        "PrevTextureSize" => return UniformSemantic::PrevTextureSize(0),
        "PrevInputSize" => return UniformSemantic::PrevInputSize(0),
        _ => {}
    }
    if let Some(stem) = name.strip_suffix("TextureSize") {
        if let Some(n) = stem.strip_prefix("PassPrev").and_then(|s| s.parse::<usize>().ok()) {
            if n >= 1 {
                return UniformSemantic::PassPrevTextureSize(n);
            }
        }
        if let Some(n) = stem.strip_prefix("Pass").and_then(|s| s.parse::<usize>().ok()) {
            if n >= 1 {
                return UniformSemantic::PassTextureSize(n - 1);
            }
        }
        if let Some(n) = stem.strip_prefix("Prev").and_then(|s| s.parse::<usize>().ok()) {
            return UniformSemantic::PrevTextureSize(n);
        }
        if let Some(alias) = aliases.iter().find(|a| a.as_str() == stem) {
            return UniformSemantic::AliasTextureSize(alias.clone());
        }
    }
    if let Some(stem) = name.strip_suffix("InputSize") {
        if let Some(n) = stem.strip_prefix("PassPrev").and_then(|s| s.parse::<usize>().ok()) {
            if n >= 1 {
                return UniformSemantic::PassPrevInputSize(n);
            }
        }
        if let Some(n) = stem.strip_prefix("Pass").and_then(|s| s.parse::<usize>().ok()) {
            if n >= 1 {
                return UniformSemantic::PassInputSize(n - 1);
            }
        }
        if let Some(n) = stem.strip_prefix("Prev").and_then(|s| s.parse::<usize>().ok()) {
            return UniformSemantic::PrevInputSize(n);
        }
        if let Some(alias) = aliases.iter().find(|a| a.as_str() == stem) {
            return UniformSemantic::AliasInputSize(alias.clone());
        }
    }
    if param_defaults.contains_key(name) {
        return UniformSemantic::Parameter(name.to_string());
    }
    UniformSemantic::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_legacy_glsl120_with_gl_fragcolor_and_helper_sampler() {
        let src = r#"
#pragma parameter SCAN_STRENGTH "Scanline Strength" 0.35 0.0 1.0 0.05

#if defined(VERTEX)
attribute vec4 VertexCoord;
attribute vec4 TexCoord;
attribute vec4 COLOR;
uniform mat4 MVPMatrix;
uniform vec2 TextureSize, InputSize, OutputSize;
varying vec4 v_tex;
varying vec2 v_scale;
void main() {
    gl_Position = MVPMatrix * VertexCoord;
    v_tex = TexCoord;
    v_scale = OutputSize / TextureSize;
}
#elif defined(FRAGMENT)
precision mediump float;
uniform sampler2D Texture;
uniform sampler2D OrigTexture;
uniform sampler2D MaskLUT;
uniform int FrameCount;
uniform vec2 OutputSize;
uniform float SCAN_STRENGTH;
varying vec4 v_tex;
varying vec2 v_scale;

vec4 sample_offset(sampler2D img, vec2 uv, vec2 off) {
    return texture2D(img, uv + off);
}

void main() {
    vec4 base = sample_offset(Texture, v_tex.xy, vec2(0.0));
    vec4 orig = texture2D(OrigTexture, v_tex.xy);
    vec4 mask = texture2D(MaskLUT, v_tex.xy);
    float scan = 1.0 - SCAN_STRENGTH * step(0.5, fract(v_tex.y * OutputSize.y * 0.5));
    float flicker = float(FrameCount & 1) * 0.01;
    gl_FragColor = mix(base, orig, 0.25) * mask * (scan + flicker);
}
#endif
"#;
        let mut overrides = BTreeMap::new();
        overrides.insert("SCAN_STRENGTH".into(), 0.5);
        let compiled = compile_retroarch_glsl(
            src,
            Path::new("."),
            &[],
            &["MaskLUT".into()],
            &overrides,
        )
        .expect("should compile legacy GLSL 120 shader through naga");

        assert_eq!(compiled.texture_bindings.len(), 3);
        assert_eq!(compiled.texture_bindings[0].semantic, TextureSemantic::Source);
        assert_eq!(compiled.texture_bindings[1].semantic, TextureSemantic::Original);
        assert_eq!(
            compiled.texture_bindings[2].semantic,
            TextureSemantic::Lut("MaskLUT".into())
        );
        let param = compiled
            .uniform_fields
            .iter()
            .find(|f| f.name == "SCAN_STRENGTH")
            .expect("SCAN_STRENGTH uniform present");
        assert_eq!(param.semantic, UniformSemantic::Parameter("SCAN_STRENGTH".into()));
        assert!((param.default_param - 0.5).abs() < 1e-6);
    }

    #[test]
    fn compiles_modern_compat_macro_multipass_shader() {
        let src = r#"
#if __VERSION__ >= 130
#define RA_VAR out
#define RA_ATTR in
#define RA_TEX texture
#else
#define RA_VAR varying
#define RA_ATTR attribute
#define RA_TEX texture2D
#endif

#if defined(VERTEX)
RA_ATTR vec4 VertexCoord;
RA_ATTR vec4 TexCoord;
uniform mat4 MVPMatrix;
uniform vec2 TextureSize;
RA_VAR vec2 uv_main;
RA_VAR vec2 uv_step;
void main() {
    gl_Position = MVPMatrix * VertexCoord;
    uv_main = TexCoord.xy;
    uv_step = 1.0 / TextureSize;
}
#elif defined(FRAGMENT)
#undef RA_VAR
#define RA_VAR in
uniform sampler2D Texture;
uniform sampler2D BlurPassTexture;
uniform vec2 BlurPassTextureSize;
uniform float FrameCount;
RA_VAR vec2 uv_main;
RA_VAR vec2 uv_step;
out vec4 FragColor;

void main() {
    vec4 a = RA_TEX(Texture, uv_main);
    vec4 b = RA_TEX(BlurPassTexture, uv_main + 1.0 / BlurPassTextureSize);
    float pulse = 0.5 + 0.5 * sin(FrameCount * 0.1);
    FragColor = mix(a, b, 0.2 * pulse);
}
#endif
"#;
        let compiled = compile_retroarch_glsl(
            src,
            Path::new("."),
            &["BlurPass".into()],
            &[],
            &BTreeMap::new(),
        )
        .expect("should compile modern COMPAT macro shader");

        assert!(compiled
            .texture_bindings
            .iter()
            .any(|t| t.semantic == TextureSemantic::Alias("BlurPass".into())));
        assert!(compiled
            .uniform_fields
            .iter()
            .any(|u| u.semantic == UniformSemantic::AliasTextureSize("BlurPass".into())));
        assert!(compiled
            .uniform_fields
            .iter()
            .any(|u| u.semantic == UniformSemantic::FrameCount && u.ty == UniformType::Float));
    }
}
