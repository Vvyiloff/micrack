//! micrack-core — GUI-free live mic VST processing engine.
//! The only surface the GUI sees is `api::EngineHandle` + `api::Telemetry`.

pub mod analysis;
pub mod api;
pub mod engine;
pub mod plugins;
pub mod preset;
pub mod settings;

#[cfg(windows)]
pub mod editor;
#[cfg(windows)]
pub mod voicemeeter;

pub use api::{EngineHandle, Telemetry};