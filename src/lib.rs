//! In-process, single-threaded DirectDraw implementation for Win32 games.
#![allow(non_snake_case)]

#[cfg(not(all(windows, target_arch = "x86")))]
compile_error!("Build ddraw.dll for 32-bit x86 Windows.");

mod clipper;
mod draw;
mod palette;
mod renderer;
mod surface;
