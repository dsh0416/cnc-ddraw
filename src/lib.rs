//! In-process, single-threaded DirectDraw implementation for Win32 games.
#![allow(non_snake_case)]

#[cfg(not(all(windows, target_arch = "x86")))]
compile_error!("Build ddraw.dll for 32-bit x86 Windows.");

mod clipper;
mod compat;
mod config;
mod draw;
mod exports;
mod palette;
mod renderer;
mod simd_blit;
mod surface;
mod window;
