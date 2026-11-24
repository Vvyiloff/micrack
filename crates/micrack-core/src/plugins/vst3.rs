//! VST3 host via vst3-host 0.11 (proven in micrack-spike/src/bin/vst3.rs).

use super::{ParamInfo, PluginFormat, PluginInfo};
use crate::engine::block::Block;
use anyhow::{Context, Result};
use std::ffi::c_void;
use vst3_host::audio::AudioBuffers;
use vst3_host::plugin::{Plugin, WindowHandle};
use vst3_host::simple;

pub struct Vst3Plugin {
    plugin: Plugin,
    info: PluginInfo,
    buffers: AudioBuffers,
    /// Capacity the buffers were allocated with (= setupProcessing max block).
    /// Every process call shrinks the buffer Vecs to the real frame count; see
    /// `process` for why the length, not `block_size`, is what the host reads.
    block: usize,
}

impl Vst3Plugin {
    pub fn load(path: &str, rate: u32, block: usize) -> Result<Self> {
        let mut plugin = simple::load_plugin(path).context("vst3 load_plugin")?;
        let pi = plugin.info().clone();
        plugin.reconfigure(rate as f64, block).context("vst3 reconfigure")?;
        let in_ch = pi.audio_inputs.max(1) as usize;
        let out_ch = pi.audio_outputs.max(1) as usize;
        let buffers = AudioBuffers::new(in_ch, out_ch, block, rate as f64);
        plugin.start_processing().context("vst3 start_processing")?;
        let info = PluginInfo {
            name: pi.name,
            vendor: pi.vendor,
            format: PluginFormat::Vst3,
            path: path.to_string(),
            uid: pi.uid,
            audio_inputs: in_ch as u32,
            audio_outputs: out_ch as u32,
            has_gui: pi.has_gui,
            latency: plugin.latency_samples(),
        };
        Ok(Self { plugin, info, buffers, block })
    }

    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    pub fn list_parameters(&self) -> Result<Vec<ParamInfo>> {
        let params = self.plugin.get_parameters().context("vst3 get_parameters")?;
        Ok(params
            .into_iter()
            .map(|p| ParamInfo {
                display: self.plugin.format_parameter(p.id, p.value).unwrap_or_default(),
                id: p.id,
                name: p.name,
                value: p.value,
                min: p.min,
                max: p.max,
                unit: p.unit,
            })
            .collect())
    }

    /// `value` is normalized 0..1 (the VST3 parameter space).
    pub fn set_parameter(&mut self, id: u32, value: f64) -> Result<()> {
        self.plugin.set_parameter(id, value.clamp(0.0, 1.0)).context("vst3 set_parameter")
    }

    pub fn get_parameter(&self, id: u32) -> Result<f64> {
        self.plugin.get_parameter(id).context("vst3 get_parameter")
    }

    pub fn process(&mut self, b: &mut Block) -> Result<()> {
        let n = b.len.min(self.block);
        if n == 0 {
            return Ok(());
        }
        // vst3-host derives ProcessData::numSamples from the buffer Vec LENGTH
        // (CallerAudioBuffers::Flat -> map(Vec::len)), NOT from
        // AudioBuffers::block_size. Leaving the Vecs at the configured max while
        // filling only `n` makes the plugin process a stale tail from the previous
        // call, corrupting its internal state (sidechain/envelope/saturation
        // history) -> crackle. WASAPI delivers variable periods, so this fired on
        // every callback. truncate+resize cannot reallocate here (capacity ==
        // block >= n), so this stays allocation-free on the audio thread.
        for v in self.buffers.inputs.iter_mut() {
            v.truncate(n);
            v.resize(n, 0.0);
        }
        for v in self.buffers.outputs.iter_mut() {
            v.truncate(n);
            v.resize(n, 0.0);
        }
        self.buffers.block_size = n;
        let stereo_in = self.buffers.inputs.len() > 1;
        self.buffers.inputs[0][..n].copy_from_slice(&b.left[..n]);
        if stereo_in {
            self.buffers.inputs[1][..n].copy_from_slice(&b.right[..n]);
        }
        self.plugin.process_audio(&mut self.buffers).context("vst3 process_audio")?;
        let stereo_out = self.buffers.outputs.len() > 1;
        for i in 0..n {
            let l = self.buffers.outputs[0][i];
            b.left[i] = l;
            b.right[i] = if stereo_out { self.buffers.outputs[1][i] } else { l };
        }
        Ok(())
    }

    pub fn reconfigure(&mut self, rate: u32, block: usize) -> Result<()> {
        self.plugin.stop_processing().ok();
        self.plugin.reconfigure(rate as f64, block).context("vst3 reconfigure")?;
        self.buffers = AudioBuffers::new(
            self.info.audio_inputs.max(1) as usize,
            self.info.audio_outputs.max(1) as usize,
            block,
            rate as f64,
        );
        self.block = block;
        self.plugin.start_processing().context("vst3 start_processing")?;
        Ok(())
    }

    pub fn save_state(&self) -> Result<Vec<u8>> {
        self.plugin.save_state().context("vst3 save_state")
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.plugin.load_state(data).context("vst3 load_state")
    }

    /// # Safety: hwnd must be a live window on the calling thread.
    pub unsafe fn open_editor(&mut self, hwnd: *mut c_void) -> Result<()> {
        let handle = unsafe { WindowHandle::from_raw(hwnd) };
        self.plugin.open_editor(handle).context("vst3 open_editor")
    }

    pub fn close_editor(&mut self) -> Result<()> {
        self.plugin.close_editor().context("vst3 close_editor")
    }
}