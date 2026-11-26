//! In-process, thread-safe DirectDraw implementation for Win32 games.
#![allow(non_snake_case)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(not(all(windows, target_arch = "x86")))]
compile_error!("Build ddraw.dll for 32-bit x86 Windows.");

pub(crate) use win32_hooks::{compat, config, win32, window};

mod clipper;
mod draw;
mod exports;
mod ffi;
mod palette;
mod renderer;
mod simd_blit;
mod surface;
