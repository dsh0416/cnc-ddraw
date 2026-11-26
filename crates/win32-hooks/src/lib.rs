//! Win32 windowing, overlay presentation surface, logical-to-physical input mapping,
//! hotpatch hooks, configuration (`ddraw.ini`), and compatibility helpers.
#![allow(non_snake_case)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(not(all(windows, target_arch = "x86")))]
compile_error!("Build win32-hooks for 32-bit x86 Windows.");

pub mod compat;
pub mod config;
pub mod win32;
pub mod window;
