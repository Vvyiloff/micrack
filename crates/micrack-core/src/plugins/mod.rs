//! Plugin abstraction over VST2 and VST3 instances.

pub mod vst2;
pub mod vst3;

use crate::engine::block::Block;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    Vst2,
    Vst3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub vendor: String,
    pub format: PluginFormat,
    pub path: String,
    pub uid: String,
    pub audio_inputs: u32,
    pub audio_outputs: u32,
    pub has_gui: bool,
    pub latency: u32,
}

/// One automatable plugin parameter. `value`/`min`/`max` are normalized 0..1 for
/// both formats; `display` is the plugin's own human-readable string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamInfo {
    pub id: u32,
    pub name: String,
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub unit: String,
    pub display: String,
}

pub enum PluginInstance {
    Vst2(vst2::Vst2Plugin),
    Vst3(vst3::Vst3Plugin),
    /// Placeholder for a plugin missing at preset load; keeps chain position, silent.
    Missing { path: String, format: PluginFormat },
}

impl PluginInstance {
    pub fn info(&self) -> PluginInfo {
        match self {
            Self::Vst2(p) => p.info().clone(),
            Self::Vst3(p) => p.info().clone(),
            Self::Missing { path, format } => PluginInfo {
                name: Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone()),
                vendor: String::new(),
                format: *format,
                path: path.clone(),
                uid: String::new(),
                audio_inputs: 0,
                audio_outputs: 0,
                has_gui: false,
                latency: 0,
            },
        }
    }

    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing { .. })
    }

    pub fn list_parameters(&self) -> Result<Vec<ParamInfo>> {
        match self {
            Self::Vst2(p) => p.list_parameters(),
            Self::Vst3(p) => p.list_parameters(),
            Self::Missing { .. } => Ok(Vec::new()),
        }
    }

    /// `value` is normalized; clamped to 0..1 so an agent can never blast a plugin.
    pub fn set_parameter(&mut self, id: u32, value: f64) -> Result<()> {
        let v = value.clamp(0.0, 1.0);
        match self {
            Self::Vst2(p) => {
                if id >= p.num_params() as u32 {
                    anyhow::bail!("parameter id {id} out of range");
                }
                p.set_parameter(id as i32, v as f32);
                Ok(())
            }
            Self::Vst3(p) => p.set_parameter(id, v),
            Self::Missing { .. } => Ok(()),
        }
    }

    pub fn get_parameter(&self, id: u32) -> Result<f64> {
        match self {
            Self::Vst2(p) => {
                if id >= p.num_params() as u32 {
                    anyhow::bail!("parameter id {id} out of range");
                }
                Ok(p.get_parameter(id as i32) as f64)
            }
            Self::Vst3(p) => p.get_parameter(id),
            Self::Missing { .. } => Ok(0.0),
        }
    }

    pub fn process(&mut self, b: &mut Block) -> Result<()> {
        match self {
            Self::Vst2(p) => p.process(b),
            Self::Vst3(p) => p.process(b),
            Self::Missing { .. } => Ok(()),
        }
    }

    pub fn reconfigure(&mut self, rate: u32, block: usize) -> Result<()> {
        match self {
            Self::Vst2(p) => p.reconfigure(rate, block),
            Self::Vst3(p) => p.reconfigure(rate, block),
            Self::Missing { .. } => Ok(()),
        }
    }

    pub fn save_state(&self) -> Result<Vec<u8>> {
        match self {
            Self::Vst2(p) => p.save_state(),
            Self::Vst3(p) => p.save_state(),
            Self::Missing { .. } => Ok(Vec::new()),
        }
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        match self {
            Self::Vst2(p) => p.load_state(data),
            Self::Vst3(p) => p.load_state(data),
            Self::Missing { .. } => Ok(()),
        }
    }

    /// # Safety: `hwnd` must be a live window owned by the calling (editor) thread.
    pub unsafe fn open_editor(&mut self, hwnd: *mut c_void) -> Result<()> {
        match self {
            Self::Vst2(p) => unsafe { p.open_editor(hwnd) },
            Self::Vst3(p) => unsafe { p.open_editor(hwnd) },
            Self::Missing { .. } => anyhow::bail!("плагин не найден"),
        }
    }

    pub fn close_editor(&mut self) -> Result<()> {
        match self {
            Self::Vst2(p) => p.close_editor(),
            Self::Vst3(p) => p.close_editor(),
            Self::Missing { .. } => Ok(()),
        }
    }

    pub fn editor_idle(&mut self) {
        if let Self::Vst2(p) = self {
            p.editor_idle();
        }
    }

    /// Preferred editor client size (VST2 effEditGetRect; VST3 default).
    pub fn editor_size(&self) -> (i32, i32) {
        match self {
            Self::Vst2(p) => p.editor_size(),
            _ => (640, 480),
        }
    }

    /// Raw idle handle so the editor thread can pump effEditIdle while the slot is back in the chain.
    pub fn vst2_idle(&self) -> Option<vst2::Vst2Idle> {
        match self {
            Self::Vst2(p) => Some(p.idle_handle()),
            _ => None,
        }
    }
}

pub fn load(path: &str, format: PluginFormat, rate: u32, block: usize) -> Result<PluginInstance> {
    match format {
        PluginFormat::Vst2 => Ok(PluginInstance::Vst2(vst2::Vst2Plugin::load(path, rate, block)?)),
        PluginFormat::Vst3 => Ok(PluginInstance::Vst3(vst3::Vst3Plugin::load(path, rate, block)?)),
    }
}

pub fn format_from_path(path: &str) -> PluginFormat {
    if path.to_ascii_lowercase().ends_with(".vst3") { PluginFormat::Vst3 } else { PluginFormat::Vst2 }
}