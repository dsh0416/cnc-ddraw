//! Core state and compatibility contracts for the Rust cnc-ddraw rewrite.

pub mod config;
pub mod contract;
pub mod state;

pub use config::{Config, ConfigKey, CONFIG_KEYS};
pub use state::{DirectDrawState, DisplayMode};
