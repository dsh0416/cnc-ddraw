//! Pure-Rust RetroArch `.glslp` / `.glsl` shader preset loader, GLSL 450 translator,
//! and multi-pass `wgpu` filter chain.
#![forbid(unsafe_code)]

pub mod chain;
pub mod lut;
pub mod preprocess;
pub mod preset;
pub mod translate;

pub use chain::FilterChain;
