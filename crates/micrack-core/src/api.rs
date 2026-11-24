//! THE GUI CONTRACT. `EngineHandle` is Clone+Send+Sync; the GUI never touches
//! rings/atomics directly. A control thread (std::thread + mpsc) owns plugin
//! load/unload, presets, scanning and the cpal streams; the audio thread owns the
//! live chain and receives commands via rtrb. Telemetry is an atomic snapshot.

use crate::analysis::{self, VoiceAnalysis};
use crate::engine::block::{Block, db_to_gain};
use crate::engine::callback::{AudioProcessor, INPUT_RING_BLOCKS, MON_RING_BLOCKS, MonitorFeed};
use crate::engine::chain::{Command, MAX_SLOTS, Slot};
use crate::plugins::{self, ParamInfo, PluginFormat, PluginInfo, PluginInstance};
use crate::{preset, settings};
use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::{Relaxed, SeqCst}};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Plugin identity = its file path (unique on disk).
pub type PluginId = String;

/// Longest voice sample an agent may record, in seconds.
const MAX_REC_SECONDS: usize = 10;
/// Full-auto safety limit for input/master gain.
const MAX_GAIN_DB: f32 = 24.0;

fn clamp_db(db: f32) -> f32 {
    db.clamp(-MAX_GAIN_DB, MAX_GAIN_DB)
}

/// Which side of the chain a recording taps: dry mic input or processed output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordTap {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostId {
    Wasapi,
    Asio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSel {
    pub host: HostId,
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub input: DeviceSel,
    pub output: DeviceSel,
    pub sample_rate: u32,
    pub block_size: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EngineStatus {
    pub running: bool,
    pub sample_rate: u32,
    pub block_size: usize,
    pub latency_ms: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub id: HostId,
    pub name: String,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub default: bool,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub id: PluginId,
    pub name: String,
    pub vendor: String,
    pub format: PluginFormat,
    pub path: String,
    pub has_gui: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlotState {
    pub index: usize,
    pub id: u64,
    pub name: String,
    pub format: PluginFormat,
    pub bypass: bool,
    pub editor_open: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    PluginMissing { path: String },
    Error { message: String },
    Info { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub done: usize,
    pub total: usize,
    pub current: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LoadReport {
    pub loaded: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Telemetry {
    pub input_peak: f32,
    pub output_peak: f32,
    pub slot_peaks: Vec<f32>,
    pub chain: Vec<SlotState>,
    pub underruns: u64,
    pub overflows: u64,
    pub callback_max_ns: u64,
    pub callback_load_pct: f32,
    pub lock_skips: u64,
    pub drift_ratio: f32,
    pub latency_ms: f32,
    pub scan: Option<ScanProgress>,
    pub notices: Vec<Notice>,
    /// Total notices posted since app start. The list retains only the latest 32.
    pub notice_count: u64,
    /// Diagnostics (lock-free): worst / mean `process()` time per chain slot, ns.
    pub slot_proc_max_ns: Vec<u64>,
    pub slot_proc_avg_ns: Vec<u64>,
    /// Plugin `process()` calls that returned an error (previously swallowed).
    pub plugin_errors: u64,
    /// Blocks whose output contained NaN/Inf.
    pub nonfinite_blocks: u64,
    /// Headphones-monitor feed gaps (drops + starvation), independent of the mic path.
    pub monitor_gaps: u64,
    /// Actual frames per audio callback: min / max / last seen.
    pub block_min: u32,
    pub block_max: u32,
    pub block_last: u32,
}

/// Shared atomics: audio thread writes meters/counters and reads gains;
/// control thread writes gains/status and the `side` metadata (GUI-only mutex,
/// never touched by the audio callback).
pub(crate) struct Tel {
    pub running: AtomicBool,
    pub rate: AtomicU32,
    pub block: AtomicU32,
    pub latency_ms: AtomicU32,
    pub input_gain: AtomicU32,
    pub master_gain: AtomicU32,
    pub input_peak: AtomicU32,
    pub output_peak: AtomicU32,
    pub slot_peaks: [AtomicU32; MAX_SLOTS],
    pub underruns: AtomicU64,
    pub overflows: AtomicU64,
    pub callback_max_ns: AtomicU64,
    pub record_arm: AtomicBool,
    /// A callback has claimed the recorder. Do not re-arm its ring until it finishes.
    pub record_inflight: AtomicBool,
    /// 0 = dry input tap, 1 = processed output tap.
    pub record_tap: AtomicU32,
    pub record_dropped: AtomicU64,
    /// Headphones monitor: audio thread pushes the processed block only while set.
    pub monitor_arm: AtomicBool,
    /// Monitor feed gaps: producer drops (ring full) + starved consumer frames.
    /// Kept apart from underruns/overflows, which describe the mic path.
    pub monitor_gaps: AtomicU64,
    /// B2: blocks where a slot passed dry because the control thread held its lock.
    pub lock_skips: AtomicU64,
    /// B3: current drift-compensation read ratio (f32 bits, ~1.0).
    pub drift_ratio: AtomicU32,
    /// M5: set by cpal stream error callbacks; the control-thread watchdog reconnects.
    pub stream_fault: AtomicBool,
    /// Input ring fill in samples, published by the input callback for priming.
    pub input_fill: AtomicU32,
    /// Diagnostics, all written by the audio thread with relaxed stores.
    pub slot_proc_max_ns: [AtomicU64; MAX_SLOTS],
    pub slot_proc_sum_ns: [AtomicU64; MAX_SLOTS],
    pub proc_calls: AtomicU64,
    pub plugin_errors: AtomicU64,
    pub nonfinite: AtomicU64,
    pub block_min: AtomicU32,
    pub block_max: AtomicU32,
    pub block_last: AtomicU32,
    pub side: Mutex<Side>,
}

pub(crate) struct Side {
    pub chain: Vec<SlotState>,
    pub scan: Option<ScanProgress>,
    pub notices: Vec<Notice>,
    pub notice_count: u64,
}

impl Tel {
    pub(crate) fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            rate: AtomicU32::new(48000),
            block: AtomicU32::new(256),
            latency_ms: AtomicU32::new(0),
            input_gain: AtomicU32::new(1f32.to_bits()),
            master_gain: AtomicU32::new(1f32.to_bits()),
            input_peak: AtomicU32::new(0),
            output_peak: AtomicU32::new(0),
            slot_peaks: [const { AtomicU32::new(0) }; MAX_SLOTS],
            underruns: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            callback_max_ns: AtomicU64::new(0),
            record_arm: AtomicBool::new(false),
            record_inflight: AtomicBool::new(false),
            record_tap: AtomicU32::new(0),
            record_dropped: AtomicU64::new(0),
            monitor_arm: AtomicBool::new(false),
            monitor_gaps: AtomicU64::new(0),
            lock_skips: AtomicU64::new(0),
            drift_ratio: AtomicU32::new(1f32.to_bits()),
            stream_fault: AtomicBool::new(false),
            input_fill: AtomicU32::new(0),
            slot_proc_max_ns: [const { AtomicU64::new(0) }; MAX_SLOTS],
            slot_proc_sum_ns: [const { AtomicU64::new(0) }; MAX_SLOTS],
            proc_calls: AtomicU64::new(0),
            plugin_errors: AtomicU64::new(0),
            nonfinite: AtomicU64::new(0),
            block_min: AtomicU32::new(u32::MAX),
            block_max: AtomicU32::new(0),
            block_last: AtomicU32::new(0),
            side: Mutex::new(Side { chain: Vec::new(), scan: None, notices: Vec::new(), notice_count: 0 }),
        }
    }

    /// Poison-tolerant side lock: one panicked caller must not cascade into
    /// every later telemetry/control call.
    pub(crate) fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(|e| e.into_inner())
    }
}

enum CtrlMsg {
    Start(EngineConfig, mpsc::Sender<Result<EngineStatus>>),
    Stop(mpsc::Sender<()>),
    Add(PluginId, usize, mpsc::Sender<Result<()>>),
    Remove(usize),
    Move(usize, usize),
    SetBypass(usize, bool),
    SetGains(f32, f32),
    OpenEditor(usize, mpsc::Sender<Result<()>>),
    CloseEditor(usize),
    SavePreset(PathBuf, mpsc::Sender<Result<()>>),
    LoadPreset(PathBuf, mpsc::Sender<Result<LoadReport>>),
    PluginList(mpsc::Sender<Vec<PluginEntry>>),
    Rescan,
    ScanPaths(Vec<String>),
    ScanDirs(mpsc::Sender<Vec<String>>),
    AddScanDir(String, mpsc::Sender<Result<()>>),
    RemoveScanDir(String, mpsc::Sender<Result<()>>),
    ListParams(u64, mpsc::Sender<Result<Vec<ParamInfo>>>),
    SetParam(u64, u32, f64, mpsc::Sender<Result<()>>),
    GetParam(u64, u32, mpsc::Sender<Result<f64>>),
    RecordSample(f32, RecordTap, mpsc::Sender<Result<PathBuf>>),
    SetMonitor(bool, Option<String>, mpsc::Sender<Result<()>>),
    GetConfig(mpsc::Sender<Option<EngineConfig>>),
    GetLastAsio(Direction, mpsc::Sender<Option<String>>),
}

#[derive(Clone)]
pub struct EngineHandle {
    shared: Arc<HandleShared>,
}

struct HandleShared {
    tx: Mutex<mpsc::Sender<CtrlMsg>>,
    tel: Arc<Tel>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Last non-empty device list per host+direction. ASIO enumeration loads each
    /// driver, and asio-sys allows only one loaded driver per process, so it comes
    /// back EMPTY while a stream is live — the GUI must still show the devices.
    dev_cache: Mutex<HashMap<(HostId, Direction), Vec<DeviceInfo>>>,
}

impl Drop for HandleShared {
    fn drop(&mut self) {
        // Disconnect the control channel, then join so plugin/COM teardown on the
        // control thread completes before the handle is gone. Without this the
        // detached thread races a later VST3 load in the same process (unload vs
        // use of the same module) and can access-violate.
        let (dummy, gone) = mpsc::channel();
        drop(gone);
        if let Ok(mut tx) = self.tx.lock() {
            *tx = dummy; // dropping the old Sender disconnects the control rx
        }
        if let Ok(mut j) = self.join.lock() {
            if let Some(h) = j.take() {
                let _ = h.join();
            }
        }
    }
}

impl EngineHandle {
    /// Spawns the control thread (and, on Windows, lazily the editor thread).
    pub fn new() -> EngineHandle {
        // Snapshot the ASIO device list before any stream exists: once the control
        // thread auto-starts, the loaded driver blocks enumeration for the whole run.
        let mut dev_cache = HashMap::new();
        for dir in [Direction::Input, Direction::Output] {
            let list = enumerate_devices(HostId::Asio, dir);
            if !list.is_empty() {
                dev_cache.insert((HostId::Asio, dir), list);
            }
        }
        let tel = Arc::new(Tel::new());
        let (tx, rx) = mpsc::channel();
        let t = tel.clone();
        let join = std::thread::Builder::new()
            .name("micrack-control".into())
            .spawn(move || {
                let mut eng = Engine::new(t);
                loop {
                    match rx.recv_timeout(Duration::from_millis(16)) {
                        Ok(msg) => eng.handle(msg),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    #[cfg(windows)]
                    {
                        let closed = eng.editors.pump();
                        if !closed.is_empty() {
                            let mut side = eng.tel.side();
                            for s in side.chain.iter_mut() {
                                if closed.contains(&s.id) {
                                    s.editor_open = false;
                                }
                            }
                        }
                    }
                    eng.tick();
                }
                // App exit: orderly stop returns slots to the control thread and
                // persists last-session.json before teardown.
                eng.stop_inner();
                eng.autosave_flush();
                #[cfg(windows)]
                eng.editors.close_all();
            })
            .expect("spawn control thread");
        EngineHandle { shared: Arc::new(HandleShared { tx: Mutex::new(tx), tel, join: Mutex::new(Some(join)), dev_cache: Mutex::new(dev_cache) }) }
    }

    fn post(&self, msg: CtrlMsg) {
        let _ = self.shared.tx.lock().unwrap_or_else(|e| e.into_inner()).send(msg);
    }

    fn request<T>(&self, make: impl FnOnce(mpsc::Sender<T>) -> CtrlMsg) -> Result<T> {
        let (rtx, rrx) = mpsc::channel();
        self.shared.tx.lock().unwrap_or_else(|e| e.into_inner()).send(make(rtx)).map_err(|_| anyhow!("engine control thread gone"))?;
        rrx.recv().map_err(|_| anyhow!("engine control thread gone"))
    }

    /// Semi-auto VB-CABLE helper: WASAPI-only enumeration (safe while an ASIO
    /// stream is live — ASIO enumeration would load drivers and come back empty).
    pub fn virtual_cable_status(&self) -> VirtualCableStatus {
        let ins = enumerate_devices(HostId::Wasapi, Direction::Input);
        let outs = enumerate_devices(HostId::Wasapi, Direction::Output);
        let cable_output = ins.iter().find(|d| d.name.contains("CABLE Output")).map(|d| d.name.clone());
        let cable_input = outs.iter().find(|d| d.name.contains("CABLE Input")).map(|d| d.name.clone());
        let voicemeeter = ins.iter().chain(outs.iter()).any(|d| d.name.contains("Voicemeeter"));
        VirtualCableStatus { cable_input, cable_output, voicemeeter }
    }

    pub fn hosts(&self) -> Vec<HostInfo> {
        let avail = cpal::available_hosts();
        vec![
            HostInfo { id: HostId::Wasapi, name: "WASAPI".into(), available: avail.contains(&cpal::HostId::Wasapi) },
            HostInfo { id: HostId::Asio, name: "ASIO".into(), available: avail.contains(&cpal::HostId::Asio) },
        ]
    }

    pub fn devices(&self, host: HostId, dir: Direction) -> Vec<DeviceInfo> {
        let fresh = enumerate_devices(host, dir);
        let mut list = {
            let mut cache = self.shared.dev_cache.lock().unwrap_or_else(|e| e.into_inner());
            if fresh.is_empty() {
                // Live ASIO stream: enumeration cannot load drivers. Serve the snapshot
                // taken before the stream started so the dropdowns never go blank.
                cache.get(&(host, dir)).cloned().unwrap_or_default()
            } else {
                cache.insert((host, dir), fresh.clone());
                fresh
            }
        };
        // ASIO has no system default, so cpal marks whichever driver enumerates
        // first — the GUI then auto-picks an arbitrary cable on a host switch.
        // Prefer the device configured for this side; when switching from another
        // host, fall back to the last ASIO device that actually started.
        if host == HostId::Asio {
            let configured = self
                .config()
                .map(|c| match dir {
                    Direction::Input => c.input,
                    Direction::Output => c.output,
                })
                .filter(|sel| sel.host == HostId::Asio)
                .map(|sel| sel.device);
            let remembered = self
                .request(|tx| CtrlMsg::GetLastAsio(dir, tx))
                .unwrap_or(None);
            if let Some(name) = configured.or(remembered) {
                if list.iter().any(|d| d.name == name) {
                    for d in &mut list {
                        d.default = d.name == name;
                    }
                }
            }
        }
        list
    }

    pub fn start(&self, cfg: EngineConfig) -> Result<EngineStatus> {
        self.request(|tx| CtrlMsg::Start(cfg, tx))?
    }

    pub fn stop(&self) {
        let _ = self.request(|tx| CtrlMsg::Stop(tx));
    }

    pub fn status(&self) -> EngineStatus {
        let t = &self.shared.tel;
        EngineStatus {
            running: t.running.load(Relaxed),
            sample_rate: t.rate.load(Relaxed),
            block_size: t.block.load(Relaxed) as usize,
            latency_ms: f32::from_bits(t.latency_ms.load(Relaxed)),
        }
    }

    pub fn plugin_list(&self) -> Vec<PluginEntry> {
        self.request(|tx| CtrlMsg::PluginList(tx)).unwrap_or_default()
    }

    pub fn rescan(&self) {
        self.post(CtrlMsg::Rescan);
    }

    pub fn scan_plugins(&self, paths: Vec<String>) {
        self.post(CtrlMsg::ScanPaths(paths));
    }

    /// Effective plugin scan folders: built-in defaults + user-added dirs.
    pub fn scan_dirs(&self) -> Vec<String> {
        self.request(|tx| CtrlMsg::ScanDirs(tx)).unwrap_or_default()
    }

    /// Add a user plugin folder (persisted, scanned recursively) and rescan.
    pub fn add_scan_dir(&self, path: String) -> Result<()> {
        self.request(|tx| CtrlMsg::AddScanDir(path, tx))?
    }

    /// Remove a user-added plugin folder (persisted) and rescan.
    pub fn remove_scan_dir(&self, path: String) -> Result<()> {
        self.request(|tx| CtrlMsg::RemoveScanDir(path, tx))?
    }

    pub fn add(&self, id: PluginId, index: usize) -> Result<()> {
        self.request(|tx| CtrlMsg::Add(id, index, tx))?
    }

    pub fn remove(&self, index: usize) {
        self.post(CtrlMsg::Remove(index));
    }

    pub fn move_slot(&self, from: usize, to: usize) {
        self.post(CtrlMsg::Move(from, to));
    }

    pub fn set_bypass(&self, index: usize, bypass: bool) {
        self.post(CtrlMsg::SetBypass(index, bypass));
    }

    /// Clamped to ±24 dB (full-auto safety limit).
    pub fn set_gains(&self, input_db: f32, master_db: f32) {
        self.post(CtrlMsg::SetGains(input_db, master_db));
    }

    /// Currently applied gain targets in dB (after clamping).
    pub fn gains_db(&self) -> (f32, f32) {
        let to_db = |a: &AtomicU32| 20.0 * f32::from_bits(a.load(Relaxed)).max(1e-6).log10();
        (to_db(&self.shared.tel.input_gain), to_db(&self.shared.tel.master_gain))
    }

    /// Saved/running engine config (devices, rate, buffer) for GUI hydration.
    pub fn config(&self) -> Option<EngineConfig> {
        self.request(CtrlMsg::GetConfig).unwrap_or(None)
    }

    /// On-demand "listen to my processed voice" toggle. micrack owns a second
    /// output stream on the headphones device; Voicemeeter routing is not touched
    /// (its Remote API is read-only on this install). `device` overrides the
    /// persisted monitor device; empty string = Windows default playback.
    pub fn set_monitor(&self, enabled: bool, device: Option<String>) -> Result<()> {
        self.request(|tx| CtrlMsg::SetMonitor(enabled, device, tx))?
    }

    pub fn monitor_on(&self) -> bool {
        self.shared.tel.monitor_arm.load(Relaxed)
    }

    /// Parameters of the slot with this stable id (`SlotState::id`, not index).
    pub fn list_parameters(&self, slot_id: u64) -> Result<Vec<ParamInfo>> {
        self.request(|tx| CtrlMsg::ListParams(slot_id, tx))?
    }

    /// `value` is normalized 0..1 and clamped.
    pub fn set_parameter(&self, slot_id: u64, id: u32, value: f64) -> Result<()> {
        self.request(|tx| CtrlMsg::SetParam(slot_id, id, value, tx))?
    }

    pub fn get_parameter(&self, slot_id: u64, id: u32) -> Result<f64> {
        self.request(|tx| CtrlMsg::GetParam(slot_id, id, tx))?
    }

    /// Record `seconds` (clamped 0.5..10) of live audio to a WAV and return its path.
    /// Blocks the control thread for the duration; the engine must be running.
    pub fn record_sample(&self, seconds: f32, tap: RecordTap) -> Result<PathBuf> {
        self.request(|tx| CtrlMsg::RecordSample(seconds, tap, tx))?
    }

    /// Offline analysis of a recorded WAV: loudness, dynamics, sibilance, hints.
    pub fn analyze_voice(&self, path: &Path) -> Result<VoiceAnalysis> {
        analysis::analyze_voice(path)
    }

    pub fn open_editor(&self, index: usize) -> Result<()> {
        self.request(|tx| CtrlMsg::OpenEditor(index, tx))?
    }

    pub fn close_editor(&self, index: usize) {
        self.post(CtrlMsg::CloseEditor(index));
    }

    pub fn save_preset(&self, path: &Path) -> Result<()> {
        self.request(|tx| CtrlMsg::SavePreset(path.to_path_buf(), tx))?
    }

    pub fn load_preset(&self, path: &Path) -> Result<LoadReport> {
        self.request(|tx| CtrlMsg::LoadPreset(path.to_path_buf(), tx))?
    }

    pub fn telemetry(&self) -> Telemetry {
        let t = &self.shared.tel;
        let side = t.side();
        let rate = t.rate.load(Relaxed);
        let block = t.block.load(Relaxed);
        let max_ns = t.callback_max_ns.load(Relaxed);
        let block_ns = if rate > 0 { block as f64 / rate as f64 * 1e9 } else { 0.0 };
        Telemetry {
            input_peak: f32::from_bits(t.input_peak.load(Relaxed)),
            output_peak: f32::from_bits(t.output_peak.load(Relaxed)),
            slot_peaks: (0..side.chain.len()).map(|i| f32::from_bits(t.slot_peaks[i].load(Relaxed))).collect(),
            chain: side.chain.clone(),
            underruns: t.underruns.load(Relaxed),
            overflows: t.overflows.load(Relaxed),
            callback_max_ns: max_ns,
            callback_load_pct: if block_ns > 0.0 { (max_ns as f64 / block_ns * 100.0) as f32 } else { 0.0 },
            lock_skips: t.lock_skips.load(Relaxed),
            drift_ratio: f32::from_bits(t.drift_ratio.load(Relaxed)),
            latency_ms: f32::from_bits(t.latency_ms.load(Relaxed)),
            scan: side.scan.clone(),
            notices: side.notices.clone(),
            notice_count: side.notice_count,
            slot_proc_max_ns: (0..side.chain.len()).map(|i| t.slot_proc_max_ns[i].load(Relaxed)).collect(),
            slot_proc_avg_ns: {
                let calls = t.proc_calls.load(Relaxed).max(1);
                (0..side.chain.len()).map(|i| t.slot_proc_sum_ns[i].load(Relaxed) / calls).collect()
            },
            plugin_errors: t.plugin_errors.load(Relaxed),
            nonfinite_blocks: t.nonfinite.load(Relaxed),
            monitor_gaps: t.monitor_gaps.load(Relaxed),
            block_min: {
                let v = t.block_min.load(Relaxed);
                if v == u32::MAX { 0 } else { v }
            },
            block_max: t.block_max.load(Relaxed),
            block_last: t.block_last.load(Relaxed),
        }
    }
}

impl Default for EngineHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize)]
pub struct VirtualCableStatus {
    pub cable_input: Option<String>,
    pub cable_output: Option<String>,
    pub voicemeeter: bool,
}

fn host_for(id: HostId) -> Result<cpal::Host> {
    match id {
        HostId::Wasapi => cpal::host_from_id(cpal::HostId::Wasapi).context("WASAPI host unavailable"),
        HostId::Asio => cpal::host_from_id(cpal::HostId::Asio).context("ASIO host unavailable"),
    }
}

fn enumerate_devices(host: HostId, dir: Direction) -> Vec<DeviceInfo> {
    let Ok(h) = host_for(host) else { return Vec::new() };
    let default_name = match dir {
        Direction::Input => h.default_input_device(),
        Direction::Output => h.default_output_device(),
    }
    .and_then(|d| d.description().ok())
    .map(|d| d.name().to_string())
    .unwrap_or_default();
    let devs = match dir {
        Direction::Input => h.input_devices(),
        Direction::Output => h.output_devices(),
    };
    let Ok(devs) = devs else { return Vec::new() };
    devs.map(|d| {
        let name = d.description().map(|x| x.name().to_string()).unwrap_or_default();
        let cfg = match dir {
            Direction::Input => d.default_input_config().ok(),
            Direction::Output => d.default_output_config().ok(),
        };
        DeviceInfo {
            default: name == default_name,
            name,
            sample_rate: cfg.as_ref().map(|c| c.sample_rate()).unwrap_or(0),
            channels: cfg.map(|c| c.channels()).unwrap_or(0),
        }
    })
    .collect()
}

fn find_device(host: &cpal::Host, dir: Direction, name: &str) -> Result<cpal::Device> {
    if name.is_empty() || name.eq_ignore_ascii_case("default") {
        return match dir {
            Direction::Input => host.default_input_device(),
            Direction::Output => host.default_output_device(),
        }
        .context("no default device");
    }
    let devs = match dir {
        Direction::Input => host.input_devices(),
        Direction::Output => host.output_devices(),
    }
    .context("device enumeration")?;
    for d in devs {
        if d.description().map(|x| x.name() == name).unwrap_or(false) {
            return Ok(d);
        }
    }
    bail!("устройство не найдено: {name}")
}

// ---------------------------------------------------------------------------
// Control thread state
// ---------------------------------------------------------------------------

/// Process a few silent blocks on the control thread BEFORE the plugin joins
/// the live chain, so lazy plugin init/alloc never runs under the audio
/// callback's first process lock (insert crackle).
fn prewarm(inst: &mut PluginInstance, block: usize) {
    let mut b = Block::new(block.max(1));
    b.set_len(block);
    for _ in 0..4 {
        b.clear();
        let _ = inst.process(&mut b);
    }
}

/// Push a command to the audio ring, retrying briefly while full (the callback
/// drains every block). False = callback stalled: callers must leave the shadow
/// chain untouched and surface a notice, so GUI and audio never diverge.
fn push_cmd(r: &mut Running, cmd: Command) -> bool {
    let mut pending = cmd;
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match r.cmd.push(pending) {
            Ok(()) => return true,
            Err(rtrb::PushError::Full(c)) => {
                if Instant::now() >= deadline {
                    return false;
                }
                pending = c;
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

struct Running {
    cmd: rtrb::Producer<Command>,
    ret: rtrb::Consumer<Slot>,
    in_stream: cpal::Stream,
    out_stream: cpal::Stream,
    cfg: EngineConfig,
    /// Recording consumers (dry input / processed output), drained by record_sample.
    rec_in: rtrb::Consumer<f32>,
    rec_out: rtrb::Consumer<f32>,
    /// Monitor ring consumer, moved into the monitor stream on first enable.
    mon_cons: Option<rtrb::Consumer<f32>>,
    /// Built on first enable, then kept for the run: pausing stops the WASAPI
    /// client, so the ring (and its audio-thread producer) never needs swapping.
    mon: Option<cpal::Stream>,
}

fn collect_recording(
    cons: &mut rtrb::Consumer<f32>,
    target: usize,
    timeout: Duration,
    mut wait: impl FnMut(),
) -> Vec<f32> {
    let mut samples = Vec::with_capacity(target);
    let deadline = Instant::now() + timeout;
    loop {
        while samples.len() < target {
            let Ok(sample) = cons.pop() else { break };
            samples.push(sample);
        }
        if samples.len() == target || Instant::now() >= deadline {
            return samples;
        }
        wait();
    }
}

#[cfg(test)]
mod recorder_tests {
    use super::*;

    #[test]
    fn capture_drains_small_ring_and_stops_at_exact_stereo_frame() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::new(8);
        let mut next = 0;
        let capture = collect_recording(&mut consumer, 16, Duration::from_secs(1), || {
            for _ in 0..6 {
                producer.push(next as f32).expect("capture must drain between blocks");
                next += 1;
            }
        });
        assert!(capture.iter().copied().eq((0..16).map(|n| n as f32)));
        assert_eq!(consumer.pop().unwrap(), 16.0);
        assert_eq!(consumer.pop().unwrap(), 17.0);
        assert!(consumer.pop().is_err());
    }
}

/// M5: bounded reconnect schedule after a stream/device fault.
struct Reconnect {
    cfg: EngineConfig,
    attempts: u32,
    next: Instant,
}

struct Engine {
    tel: Arc<Tel>,
    offline: Vec<Slot>,
    running: Option<Running>,
    settings: settings::Settings,
    cache: Vec<settings::CacheEntry>,
    entries: Vec<PluginEntry>,
    gains_db: (f32, f32),
    /// Plugin latencies mirroring chain order (control-side bookkeeping).
    latencies: Vec<u32>,
    /// Control-side registry of live instances keyed by slot id. Each entry is an
    /// Arc clone of the same mutex the audio chain holds, so editors can lock the
    /// instance for open/close/idle while audio keeps processing it.
    instances: HashMap<u64, Arc<Mutex<PluginInstance>>>,
    /// Parameter lists cached at load time (control thread, plugin not yet in
    /// the chain), so list_parameters never enumerates under the audio lock.
    params: HashMap<u64, Vec<ParamInfo>>,
    /// Pending device-loss reconnect (M5), serviced by tick().
    reconnect: Option<Reconnect>,
    /// Pending auto-start (N4), serviced by tick() so Engine::new never blocks
    /// the control loop. Reuses the Reconnect schedule (cfg/attempts/next).
    auto_start: Option<Reconnect>,
    /// When the current streams started; watchdog grace against start transients.
    started_at: Instant,
    /// Autosave skipped because audio held an instance lock; tick() retries (B2).
    autosave_pending: bool,
    autosave_next: Instant,
    #[cfg(windows)]
    editors: crate::editor::Editors,
}

impl Engine {
    fn new(tel: Arc<Tel>) -> Self {
        let settings = settings::load_settings();
        let gains_db = (settings.input_gain_db, settings.master_gain_db);
        tel.input_gain.store(db_to_gain(gains_db.0).to_bits(), Relaxed);
        tel.master_gain.store(db_to_gain(gains_db.1).to_bits(), Relaxed);
        let mut eng = Self {
            tel,
            offline: Vec::new(),
            running: None,
            settings,
            cache: settings::load_cache(),
            entries: Vec::new(),
            gains_db,
            latencies: Vec::new(),
            instances: HashMap::new(),
            params: HashMap::new(),
            reconnect: None,
            auto_start: None,
            started_at: Instant::now(),
            autosave_pending: false,
            autosave_next: Instant::now(),
            #[cfg(windows)]
            editors: crate::editor::Editors::new(),
        };
        // Auto-restore the previous session before the GUI connects. Missing
        // plugins become bypassed placeholders, never a startup failure.
        let session = settings::app_dir().join("last-session.json");
        let restored = if session.exists() {
            match eng.load_preset_inner(&session) {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!("last-session restore: {e}");
                    eng.notice(Notice::Error { message: format!("не удалось восстановить сессию: {e}; движок остановлен") });
                    false
                }
            }
        } else {
            true
        };
        // Auto-start whenever a last config exists (M1) — not gated on the
        // session file — so processed audio flows immediately on launch. DEFERRED
        // to tick() (N4): the control loop must start at once so GUI requests are
        // not queued behind ~8 s of retries + priming. Bounded retry (3 x 1 s)
        // covers devices still booting (Voicemeeter). Fail-safe: a FAILED restore
        // never auto-starts the raw mic at a possibly hot gain.
        if restored {
            if let Some(cfg) = eng.settings.last_config.clone() {
                eng.auto_start = Some(Reconnect { cfg, attempts: 0, next: Instant::now() });
            }
        }
        eng
    }

    /// Control-loop housekeeping (~every 16 ms): device-loss watchdog (M5) and
    /// deferred autosave retry (B2: snapshot skipped while audio held a lock).
    fn tick(&mut self) {
        // 500 ms grace: stream start/teardown transients must not trip the
        // watchdog into stopping healthy fresh streams.
        let grace = Instant::now() >= self.started_at + Duration::from_millis(500);
        if self.running.is_some() && grace && self.tel.stream_fault.swap(false, Relaxed) {
            let cfg = self.running.as_ref().unwrap().cfg.clone();
            tracing::warn!("stream fault detected; scheduling reconnect");
            self.notice(Notice::Info { message: "ошибка аудиоустройства — переподключение".into() });
            self.stop_inner();
            self.reconnect = Some(Reconnect { cfg, attempts: 0, next: Instant::now() + Duration::from_secs(1) });
        }
        let due = self.reconnect.as_ref().map(|rc| Instant::now() >= rc.next).unwrap_or(false);
        if due {
            // Take the schedule out: start_inner clears self.reconnect, so reschedule
            // from the local copy or a failed attempt would never retry (M5 fix).
            let mut rc = self.reconnect.take().unwrap();
            rc.attempts += 1;
            let attempts = rc.attempts;
            match self.start_inner(rc.cfg.clone()) {
                Ok(_) => {
                    self.notice(Notice::Info { message: "аудиоустройство переподключено".into() });
                }
                Err(e) => {
                    tracing::warn!("reconnect attempt {attempts}: {e}");
                    if attempts >= 5 {
                        self.notice(Notice::Error { message: format!("устройство потеряно; движок остановлен после 5 попыток переподключения: {e}") });
                    } else {
                        rc.next = Instant::now() + Duration::from_secs(1 << attempts.min(4));
                        self.reconnect = Some(rc);
                    }
                }
            }
        }
        // N4: deferred auto-start, serviced here so Engine::new never blocks the
        // control loop. 3 attempts, 1 s apart; cancelled by any manual start/stop.
        let due = self.auto_start.as_ref().map(|a| Instant::now() >= a.next).unwrap_or(false);
        if due {
            let mut a = self.auto_start.take().unwrap();
            a.attempts += 1;
            let attempts = a.attempts;
            match self.start_inner(a.cfg.clone()) {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("auto-start attempt {attempts}: {e}");
                    if attempts >= 3 {
                        self.notice(Notice::Error { message: format!("автозапуск не удался после 3 попыток: {e}") });
                    } else {
                        a.next = Instant::now() + Duration::from_secs(1);
                        self.auto_start = Some(a);
                    }
                }
            }
        }
        if self.autosave_pending && Instant::now() >= self.autosave_next {
            self.autosave_next = Instant::now() + Duration::from_millis(500);
            self.autosave_flush();
        }
    }

    fn handle(&mut self, msg: CtrlMsg) {
        // Opportunistic teardown of slots returned by Remove (off the audio thread).
        if let Some(r) = &mut self.running {
            while let Ok(slot) = r.ret.pop() {
                drop(slot);
            }
        }
        match msg {
            CtrlMsg::Start(cfg, tx) => {
                self.auto_start = None; // a manual start supersedes pending auto-start
                let _ = tx.send(self.start_inner(cfg));
            }
            CtrlMsg::Stop(tx) => {
                self.stop_inner();
                let _ = tx.send(());
            }
            CtrlMsg::Add(id, index, tx) => {
                let _ = tx.send(self.add_inner(&id, index));
            }
            CtrlMsg::Remove(i) => self.remove_inner(i),
            CtrlMsg::Move(f, t) => self.move_inner(f, t),
            CtrlMsg::SetBypass(i, b) => {
                if let Some(r) = &mut self.running {
                    if !push_cmd(r, Command::SetBypass(i, b)) {
                        self.notice(Notice::Error { message: "очередь аудиокоманд переполнена: изменение обхода отброшено (завис обработчик?)".into() });
                        return;
                    }
                } else if let Some(s) = self.offline.get_mut(i) {
                    s.bypass = b;
                }
                if let Some(s) = self.tel.side().chain.get_mut(i) {
                    s.bypass = b;
                }
                self.autosave();
            }
            CtrlMsg::SetGains(input_db, master_db) => {
                let (input_db, master_db) = (clamp_db(input_db), clamp_db(master_db));
                self.gains_db = (input_db, master_db);
                self.tel.input_gain.store(db_to_gain(input_db).to_bits(), Relaxed);
                self.tel.master_gain.store(db_to_gain(master_db).to_bits(), Relaxed);
                self.settings.input_gain_db = input_db;
                self.settings.master_gain_db = master_db;
                self.autosave();
            }
            CtrlMsg::OpenEditor(i, tx) => {
                let _ = tx.send(self.open_editor_inner(i));
            }
            CtrlMsg::CloseEditor(i) => self.close_editor_inner(i),
            CtrlMsg::SavePreset(path, tx) => {
                let _ = tx.send(self.save_preset_inner(&path));
            }
            CtrlMsg::LoadPreset(path, tx) => {
                let res = self.load_preset_inner(&path);
                if res.is_ok() {
                    self.autosave();
                }
                let _ = tx.send(res);
            }
            CtrlMsg::PluginList(tx) => {
                if self.entries.is_empty() {
                    self.do_scan();
                }
                let _ = tx.send(self.entries.clone());
            }
            CtrlMsg::Rescan => self.do_scan(),
            CtrlMsg::ScanPaths(paths) => {
                for p in paths {
                    if !self.settings.extra_dirs.contains(&p) {
                        self.settings.extra_dirs.push(p);
                    }
                }
                let _ = settings::save_settings(&self.settings);
                self.do_scan();
            }
            CtrlMsg::ScanDirs(tx) => {
                let mut dirs = settings::default_dirs();
                dirs.extend(self.settings.extra_dirs.iter().cloned());
                let _ = tx.send(dirs);
            }
            CtrlMsg::AddScanDir(p, tx) => {
                let res = if !Path::new(&p).is_dir() {
                    Err(anyhow!("not a directory: {p}"))
                } else {
                    if !self.settings.extra_dirs.contains(&p) {
                        self.settings.extra_dirs.push(p);
                    }
                    let _ = settings::save_settings(&self.settings);
                    self.do_scan();
                    Ok(())
                };
                let _ = tx.send(res);
            }
            CtrlMsg::RemoveScanDir(p, tx) => {
                let res = if self.settings.extra_dirs.iter().any(|d| d == &p) {
                    self.settings.extra_dirs.retain(|d| d != &p);
                    let _ = settings::save_settings(&self.settings);
                    self.do_scan();
                    Ok(())
                } else {
                    Err(anyhow!("not a user scan dir: {p}"))
                };
                let _ = tx.send(res);
            }
            CtrlMsg::ListParams(id, tx) => {
                // Served from the load-time cache: no instance lock, no audio stall.
                let _ = tx.send(self.params.get(&id).cloned().with_context(|| format!("slot id {id} not found")));
            }
            CtrlMsg::SetParam(id, param, value, tx) => {
                let _ = tx.send(self.set_param_inner(id, param, value));
            }
            CtrlMsg::GetParam(id, param, tx) => {
                let res = self.instance(id).and_then(|i| {
                    // try_lock read (N1): never hold the instance against the audio
                    // callback. Brief retry covers the per-block process lock; fall
                    // back to the last-set cached value if the slot stays busy.
                    let deadline = Instant::now() + Duration::from_millis(100);
                    loop {
                        match i.try_lock() {
                            Ok(g) => return g.get_parameter(param),
                            Err(std::sync::TryLockError::Poisoned(e)) => return e.into_inner().get_parameter(param),
                            Err(std::sync::TryLockError::WouldBlock) => {
                                if Instant::now() >= deadline {
                                    return self
                                        .params
                                        .get(&id)
                                        .and_then(|ps| ps.iter().find(|p| p.id == param))
                                        .map(|p| p.value)
                                        .with_context(|| format!("slot {id} busy; parameter {param} not cached"));
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }
                    }
                });
                let _ = tx.send(res);
            }
            CtrlMsg::RecordSample(seconds, tap, tx) => {
                let _ = tx.send(self.record_sample_inner(seconds, tap));
            }
            CtrlMsg::SetMonitor(enabled, device, tx) => {
                let _ = tx.send(self.set_monitor_inner(enabled, device));
            }
            CtrlMsg::GetConfig(tx) => {
                let _ = tx.send(self.running.as_ref().map(|r| r.cfg.clone()).or_else(|| self.settings.last_config.clone()));
            }
            CtrlMsg::GetLastAsio(dir, tx) => {
                let _ = tx.send(match dir {
                    Direction::Input => self.settings.last_asio_input.clone(),
                    Direction::Output => self.settings.last_asio_output.clone(),
                });
            }
        }
    }

    /// Same instance the audio chain processes; callers lock it only briefly.
    fn instance(&self, slot_id: u64) -> Result<Arc<Mutex<PluginInstance>>> {
        self.instances.get(&slot_id).cloned().with_context(|| format!("slot id {slot_id} not found"))
    }

    /// N1a: while running, parameter writes travel to the AUDIO thread via the
    /// command ring — the control thread never locks the instance for tuning, so
    /// agent set_parameter storms cannot contend with the callback. Offline: no
    /// audio thread, apply directly. The load-time cache validates the id and
    /// mirrors the written value for list_parameters/get_parameter fallback.
    fn set_param_inner(&mut self, id: u64, param: u32, value: f64) -> Result<()> {
        let value = value.clamp(0.0, 1.0);
        self.instance(id)?; // slot must exist
        if let Some(ps) = self.params.get(&id) {
            if !ps.is_empty() && !ps.iter().any(|p| p.id == param) {
                bail!("параметр {param} не найден в слоте {id}");
            }
        }
        if let Some(r) = &mut self.running {
            if !push_cmd(r, Command::SetParam(id, param, value)) {
                self.notice(Notice::Error { message: "очередь аудиокоманд переполнена: изменение параметра отброшено (завис обработчик?)".into() });
                bail!("очередь аудиокоманд переполнена (завис обработчик?)");
            }
        } else {
            self.instance(id)?.lock().unwrap_or_else(|e| e.into_inner()).set_parameter(param, value)?;
        }
        if let Some(p) = self.params.get_mut(&id).and_then(|ps| ps.iter_mut().find(|p| p.id == param)) {
            p.value = value;
        }
        Ok(())
    }

    fn record_sample_inner(&mut self, seconds: f32, tap: RecordTap) -> Result<PathBuf> {
        if !seconds.is_finite() {
            bail!("длительность записи должна быть конечным числом");
        }
        let secs = seconds.clamp(0.5, MAX_REC_SECONDS as f32);
        let rate = self.tel.rate.load(Relaxed);
        let r = self.running.as_mut().context("engine not running: start it before recording a sample")?;
        let previous_block_deadline = Instant::now() + Duration::from_millis(500);
        while self.tel.record_inflight.load(SeqCst) {
            if Instant::now() >= previous_block_deadline {
                bail!("предыдущая запись ещё завершает аудиоблок");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let cons = if tap == RecordTap::Input { &mut r.rec_in } else { &mut r.rec_out };
        while cons.pop().is_ok() {} // discard anything stale
        self.tel.record_dropped.store(0, Relaxed);
        self.tel.record_tap.store(if tap == RecordTap::Input { 0 } else { 1 }, Relaxed);
        let target = ((f64::from(secs) * f64::from(rate)).round() as usize) * 2;
        let timeout = Duration::from_secs_f32(secs) + Duration::from_millis(500);
        self.tel.record_arm.store(true, SeqCst);
        // Drain throughout capture. The callback may finish just after the target
        // is reached, but the WAV always ends on the requested stereo frame.
        let samples = collect_recording(cons, target, timeout, || {
            #[cfg(windows)]
            self.editors.pump(); // keep plugin GUIs alive during the capture
            std::thread::sleep(Duration::from_millis(8));
        });
        self.tel.record_arm.store(false, SeqCst);
        let dropped = self.tel.record_dropped.load(Relaxed);
        if dropped > 0 {
            self.notice(Notice::Info { message: format!("запись потеряла {dropped} кадров (очередь переполнена)") });
        }
        if samples.len() != target {
            bail!("записано {}/{} кадров за {secs:.1} с (проверьте аудиопоток)", samples.len() / 2, target / 2);
        }
        let dir = PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned()))
            .join("micrack")
            .join("recordings");
        std::fs::create_dir_all(&dir).context("create recordings dir")?;
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let path = dir.join(format!("sample-{ts}.wav"));
        let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).context("create wav")?;
        for s in &samples {
            w.write_sample(*s).context("write wav sample")?;
        }
        w.finalize().context("finalize wav")?;
        Ok(path)
    }

    /// Monitor toggle. Every device call happens here on the control thread; the
    /// audio thread only ever reads monitor_arm. The stream is built on the first
    /// enable and then paused/played, so its ring consumer is moved exactly once
    /// and the audio thread's producer never has to be replaced.
    fn set_monitor_inner(&mut self, enabled: bool, device: Option<String>) -> Result<()> {
        if let Some(d) = device {
            let d = d.trim().to_string();
            let new = if d.is_empty() { None } else { Some(d) };
            if new != self.settings.monitor_device {
                self.settings.monitor_device = new;
                let _ = settings::save_settings(&self.settings);
                if self.running.as_ref().map(|r| r.mon.is_some()).unwrap_or(false) {
                    bail!("устройство монитора сохранено; оно применится после перезапуска движка");
                }
            }
        }
        if !enabled {
            self.tel.monitor_arm.store(false, Relaxed);
            let Some(r) = self.running.as_ref() else { return Ok(()) };
            let Some(s) = r.mon.as_ref() else { return Ok(()) };
            // Let the 20 ms fade-out render before the WASAPI client stops.
            std::thread::sleep(Duration::from_millis(80));
            s.pause().context("остановка монитора")?;
            return Ok(());
        }
        let tel = self.tel.clone();
        let rate = tel.rate.load(Relaxed);
        let block = tel.block.load(Relaxed) as usize;
        let name = self.settings.monitor_device.clone().unwrap_or_default();
        let r = self.running.as_mut().context("движок не запущен: сначала включите обработку")?;
        let mut built = None;
        if r.mon.is_none() {
            let host = host_for(HostId::Wasapi)?;
            let dev = find_device(&host, Direction::Output, &name)?;
            let def = dev.default_output_config().context("конфигурация устройства монитора")?;
            if def.sample_format() != SampleFormat::F32 {
                bail!("монитор требует f32 (WASAPI shared); устройство отдаёт {:?}", def.sample_format());
            }
            if def.sample_rate() != rate {
                bail!("частота монитора {} не совпадает с частотой движка {rate}", def.sample_rate());
            }
            let ch = def.channels() as usize;
            let sc = StreamConfig { channels: ch as u16, sample_rate: rate, buffer_size: BufferSize::Default };
            let cons = r.mon_cons.take().context("монитор недоступен до перезапуска движка")?;
            let mut feed = MonitorFeed::new(cons, ch, rate, block);
            let arm = tel.clone();
            // A headphones fault must never trip the mic-path reconnect watchdog.
            let gaps = tel.clone();
            let err_fn = move |e| {
                tracing::warn!("monitor stream error: {e}");
                gaps.monitor_gaps.fetch_add(1, Relaxed);
            };
            let stream = dev
                .build_output_stream(
                    sc,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        feed.run(data, arm.monitor_arm.load(Relaxed), &arm.monitor_gaps);
                    },
                    err_fn,
                    None,
                )
                .context("не удалось открыть выход монитора")?;
            r.mon = Some(stream);
            built = Some(if name.is_empty() { "устройство воспроизведения по умолчанию".to_string() } else { name });
        }
        r.mon.as_ref().unwrap().play().context("запуск монитора")?;
        tel.monitor_arm.store(true, Relaxed);
        if let Some(label) = built {
            self.notice(Notice::Info { message: format!("монитор: {label}") });
        }
        Ok(())
    }

    fn notice(&self, n: Notice) {
        let mut side = self.tel.side();
        side.notice_count += 1;
        side.notices.push(n);
        if side.notices.len() > 32 {
            let excess = side.notices.len() - 32;
            side.notices.drain(..excess);
        }
    }

    fn rate_block(&self) -> (u32, usize) {
        self.running
            .as_ref()
            .map(|r| (r.cfg.sample_rate, r.cfg.block_size))
            .or_else(|| self.settings.last_config.as_ref().map(|c| (c.sample_rate, c.block_size)))
            .unwrap_or((48000, 256))
    }

    fn reindex(&self) {
        let mut side = self.tel.side();
        for (i, s) in side.chain.iter_mut().enumerate() {
            s.index = i;
        }
    }

    /// Honest mic->output latency (N3), not just block+plugin. Components:
    ///   processing = (block + sum(plugin latency)) / rate
    ///   input ring = (INPUT_RING_BLOCKS / 2) * block / rate  (Drift keeps it half-full)
    ///   output buf ~= one WASAPI shared-mode period (>= ~10 ms) or one ASIO block
    /// The ring half-fill dominates; the old block+plugin figure (~12 ms) hid the
    /// ~21-43 ms of ring buffering plus the output buffer, so the GUI now shows the
    /// real mic->out figure instead of a number ~4x too small.
    fn update_latency(&self) {
        let (rate, block) = self.rate_block();
        let ms = if self.running.is_some() && rate > 0 {
            let block_ms = block as f32 / rate as f32 * 1000.0;
            let plugin_ms = self.latencies.iter().sum::<u32>() as f32 / rate as f32 * 1000.0;
            let ring_ms = (INPUT_RING_BLOCKS as f32 / 2.0) * block_ms;
            let out_ms = match self.running.as_ref().map(|r| r.cfg.output.host) {
                Some(HostId::Asio) => block_ms,
                _ => block_ms.max(10.0),
            };
            block_ms + plugin_ms + ring_ms + out_ms
        } else {
            0.0
        };
        self.tel.latency_ms.store(ms.to_bits(), Relaxed);
    }

    fn cache_upsert(&mut self, info: &PluginInfo) {
        let mtime = settings::file_mtime(Path::new(&info.path));
        self.cache.retain(|c| c.path != info.path);
        self.cache.push(settings::CacheEntry { path: info.path.clone(), mtime, info: info.clone() });
        let _ = settings::save_cache(&self.cache);
        // refresh scan entry metadata if present
        if let Some(e) = self.entries.iter_mut().find(|e| e.path == info.path) {
            e.name = info.name.clone();
            e.vendor = info.vendor.clone();
            e.has_gui = info.has_gui;
        }
    }

    fn shadow_insert(&mut self, index: usize, id: u64, info: &PluginInfo, bypass: bool) {
        let st = SlotState { index, id, name: info.name.clone(), format: info.format, bypass, editor_open: false };
        let mut side = self.tel.side();
        let i = index.min(side.chain.len());
        side.chain.insert(i, st);
        drop(side);
        self.latencies.insert(i, info.latency);
        self.reindex();
        self.update_latency();
    }

    fn shadow_remove(&mut self, index: usize) {
        let mut side = self.tel.side();
        if index < side.chain.len() {
            side.chain.remove(index);
        }
        drop(side);
        if index < self.latencies.len() {
            self.latencies.remove(index);
        }
        self.reindex();
        self.update_latency();
    }

    fn add_inner(&mut self, id: &str, index: usize) -> Result<()> {
        let (rate, block) = self.rate_block();
        let mut inst = plugins::load(id, plugins::format_from_path(id), rate, block)?;
        prewarm(&mut inst, block);
        let info = inst.info();
        self.cache_upsert(&info);
        let len = self.tel.side().chain.len();
        if len >= MAX_SLOTS {
            bail!("цепочка заполнена ({MAX_SLOTS} слотов)");
        }
        // Enumerate while the plugin is still private to this thread — never
        // under the audio callback's instance lock.
        let params = inst.list_parameters().unwrap_or_default();
        let shared = Arc::new(Mutex::new(inst));
        let slot = Slot::new(shared.clone());
        let slot_id = slot.id;
        let i = index.min(len);
        if let Some(r) = &mut self.running {
            if !push_cmd(r, Command::Insert(i, slot)) {
                bail!("очередь аудиокоманд переполнена (завис обработчик?)");
            }
        } else {
            self.offline.insert(i, slot);
        }
        self.instances.insert(slot_id, shared);
        self.params.insert(slot_id, params);
        self.shadow_insert(i, slot_id, &info, false);
        self.autosave();
        Ok(())
    }

    fn remove_inner(&mut self, index: usize) {
        let id = match self.tel.side().chain.get(index) {
            Some(s) => s.id,
            None => return,
        };
        if self.tel.side().chain[index].editor_open {
            self.close_editor_inner(index);
        }
        if let Some(r) = &mut self.running {
            if !push_cmd(r, Command::Remove(index)) {
                self.notice(Notice::Error { message: "очередь аудиокоманд переполнена: удаление отброшено (завис обработчик?)".into() });
                return;
            }
            // Wait for the returned slot so teardown happens here (control thread)
            // and later Shutdown accounting stays exact.
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                if let Ok(slot) = r.ret.pop() {
                    drop(slot);
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        } else if index < self.offline.len() {
            self.offline.remove(index); // teardown on control thread
        }
        // Drop the registry's Arc clone; with the chain/offline clone gone and the
        // editor closed, the last reference drops here (control-thread teardown).
        self.instances.remove(&id);
        self.params.remove(&id);
        self.shadow_remove(index);
        self.autosave();
    }

    fn move_inner(&mut self, from: usize, to: usize) {
        let len = self.tel.side().chain.len();
        if from >= len || len == 0 {
            return;
        }
        let to = to.min(len - 1);
        if let Some(r) = &mut self.running {
            if !push_cmd(r, Command::Move(from, to)) {
                self.notice(Notice::Error { message: "очередь аудиокоманд переполнена: перемещение отброшено (завис обработчик?)".into() });
                return;
            }
        } else if from < self.offline.len() {
            let s = self.offline.remove(from);
            self.offline.insert(to.min(self.offline.len()), s);
        }
        {
            let mut side = self.tel.side();
            let st = side.chain.remove(from);
            let len = side.chain.len();
            side.chain.insert(to.min(len), st);
        }
        let l = self.latencies.remove(from);
        self.latencies.insert(to.min(self.latencies.len()), l);
        self.reindex();
        self.autosave();
    }

    // -- live editor ---------------------------------------------------------

    fn open_editor_inner(&mut self, index: usize) -> Result<()> {
        #[cfg(windows)]
        {
            let (len, open, id) = {
                let side = self.tel.side();
                match side.chain.get(index) {
                    Some(s) => (side.chain.len(), s.editor_open, s.id),
                    None => (side.chain.len(), false, 0),
                }
            };
            if index >= len {
                bail!("индекс слота вне диапазона");
            }
            if open {
                return Ok(());
            }
            // Share the chain's instance with the editor; no parking, no placeholder.
            let inst = self.instances.get(&id).cloned().context("instance not found")?;
            self.editors.open(id, inst)?;
            self.tel.side().chain[index].editor_open = true;
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = index;
            bail!("редакторы доступны только на Windows")
        }
    }

    fn close_editor_inner(&mut self, index: usize) {
        #[cfg(windows)]
        {
            let (open, id) = match self.tel.side().chain.get(index) {
                Some(s) => (s.editor_open, s.id),
                None => (false, 0),
            };
            if !open {
                return;
            }
            // The instance stays in the chain; just close the window + editor.
            self.editors.close(id);
            if let Some(s) = self.tel.side().chain.get_mut(index) {
                s.editor_open = false;
            }
        }
        #[cfg(not(windows))]
        let _ = index;
    }

    // -- start/stop ----------------------------------------------------------

    /// A rejected device/config must never leave the user without audio. The
    /// attempt tears the working streams down before it can fail on a device
    /// lookup, so on error put the previous route back and still report why the
    /// new one was refused.
    fn start_inner(&mut self, cfg: EngineConfig) -> Result<EngineStatus> {
        let prev = self.running.as_ref().map(|r| r.cfg.clone());
        let monitor = self.tel.monitor_arm.load(Relaxed);
        match self.start_attempt(cfg) {
            Ok(s) => Ok(s),
            Err(e) => {
                if let Some(p) = prev {
                    if self.running.is_none() && self.start_attempt(p).is_ok() {
                        self.notice(Notice::Info { message: "прежний аудиомаршрут восстановлен".into() });
                        if monitor {
                            let _ = self.set_monitor_inner(true, None);
                        }
                    }
                }
                Err(e)
            }
        }
    }

    fn start_attempt(&mut self, cfg: EngineConfig) -> Result<EngineStatus> {
        self.reconnect = None;
        let want_monitor = self.tel.monitor_arm.load(Relaxed);
        if self.running.is_some() {
            self.stop_inner();
        }
        let mut block = cfg.block_size.clamp(16, 4096);
        let in_host = host_for(cfg.input.host)?;
        let out_host = host_for(cfg.output.host)?;
        let in_dev = find_device(&in_host, Direction::Input, &cfg.input.device)?;
        let out_dev = find_device(&out_host, Direction::Output, &cfg.output.device)?;
        let in_def = in_dev.default_input_config().context("input config")?;
        // ASIO drivers own the buffer size (Voicemeeter: fixed 512). Match it so
        // the engine block equals the real callback size instead of silently
        // falling back to Default with a mismatched block.
        if cfg.input.host == HostId::Asio {
            if let cpal::SupportedBufferSize::Range { min, max } = in_def.buffer_size() {
                block = block.clamp(*min as usize, (*max as usize).max(*min as usize));
            }
        }
        let out_def = out_dev.default_output_config().context("output config")?;
        if in_def.sample_format() != SampleFormat::F32 || out_def.sample_format() != SampleFormat::F32 {
            bail!("нужны устройства f32 (WASAPI shared); получено in={:?} out={:?}", in_def.sample_format(), out_def.sample_format());
        }
        let in_ch = in_def.channels() as usize;
        let out_ch = out_def.channels() as usize;
        // M5: a stream error flags the control-thread watchdog (tick), which
        // stops the dead streams and reconnects with bounded backoff.
        let fault_tel = self.tel.clone();
        let err_fn = move |e| {
            tracing::warn!("micrack stream error: {e}");
            fault_tel.stream_fault.store(true, Relaxed);
        };

        // Input stream: try requested fixed buffer, then default buffer, then device default.
        let in_candidates = [
            StreamConfig { channels: in_ch as u16, sample_rate: cfg.sample_rate, buffer_size: BufferSize::Fixed(block as u32) },
            StreamConfig { channels: in_ch as u16, sample_rate: cfg.sample_rate, buffer_size: BufferSize::Default },
            in_def.config(),
        ];
        let mut built_in = None;
        for sc in in_candidates {
            let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(block * in_ch.max(1) * INPUT_RING_BLOCKS);
            let ov = self.tel.clone();
            let ring_cap = block * in_ch.max(1) * INPUT_RING_BLOCKS;
            match in_dev.build_input_stream(
                sc.clone(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // The input plays before the output starts consuming (prime wait).
                    // Frames dropped in that window are startup priming, not runtime
                    // health — underruns already get a grace period, overflows must too.
                    let live = ov.running.load(Relaxed);
                    for frame in data.chunks(in_ch.max(1)) {
                        if prod.slots() < in_ch {
                            if live {
                                ov.overflows.fetch_add(1, Relaxed);
                            }
                            continue;
                        }
                        for &s in frame {
                            let _ = prod.push(s);
                        }
                    }
                    ov.input_fill.store((ring_cap - prod.slots()) as u32, Relaxed);
                },
                err_fn.clone(),
                None,
            ) {
                Ok(stream) => {
                    built_in = Some((stream, cons, sc));
                    break;
                }
                Err(e) => tracing::warn!("input config {sc:?} failed: {e}"),
            }
        }
        let (in_stream, mut cons, in_sc) = built_in.context("build input stream (all configs failed)")?;
        let actual_rate = in_sc.sample_rate;

        // Probe output configs with a silent closure so the real chain is never
        // moved into a stream build that might fail.
        let out_candidates = [
            StreamConfig { channels: out_ch as u16, sample_rate: actual_rate, buffer_size: BufferSize::Fixed(block as u32) },
            StreamConfig { channels: out_ch as u16, sample_rate: actual_rate, buffer_size: BufferSize::Default },
            StreamConfig { channels: out_ch as u16, sample_rate: out_def.sample_rate(), buffer_size: BufferSize::Default },
        ];
        let mut out_sc = None;
        for sc in out_candidates {
            match out_dev.build_output_stream(sc.clone(), |_: &mut [f32], _: &cpal::OutputCallbackInfo| {}, err_fn.clone(), None) {
                Ok(probe) => {
                    drop(probe);
                    out_sc = Some(sc);
                    break;
                }
                Err(e) => tracing::warn!("output config {sc:?} failed: {e}"),
            }
        }
        let out_sc = out_sc.context("build output stream (all configs failed)")?;
        if out_sc.sample_rate != actual_rate {
            bail!("несовпадение частоты дискретизации входа/выхода ({} против {})", actual_rate, out_sc.sample_rate);
        }

        let mut reconf_errors = Vec::new();
        for slot in &self.offline {
            let mut inst = slot.instance.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = inst.reconfigure(actual_rate, block) {
                reconf_errors.push(format!("перенастройка {}: {e}", inst.info().name));
            }
        }
        for msg in reconf_errors {
            self.notice(Notice::Error { message: msg });
        }

        let (cmd_p, cmd_c) = rtrb::RingBuffer::<Command>::new(64);
        let (ret_p, ret_c) = rtrb::RingBuffer::<Slot>::new(MAX_SLOTS * 2);
        let chain = std::mem::take(&mut self.offline);
        // Stereo interleaved, MAX_REC_SECONDS at 48 kHz — preallocated here, never on the audio thread.
        let rec_cap = 48000 * MAX_REC_SECONDS * 2;
        let (rec_in_p, rec_in_c) = rtrb::RingBuffer::<f32>::new(rec_cap);
        let (rec_out_p, rec_out_c) = rtrb::RingBuffer::<f32>::new(rec_cap);
        // Monitor feed ring: allocated once per run so enabling/disabling never
        // has to hand a new producer to the audio thread.
        let (mon_p, mon_c) = rtrb::RingBuffer::<f32>::new(block * 2 * MON_RING_BLOCKS);
        self.tel.record_arm.store(false, Relaxed);
        self.tel.monitor_arm.store(false, Relaxed);
        self.tel.monitor_gaps.store(0, Relaxed);
        let mut proc = AudioProcessor::new(chain, cmd_c, ret_p, block, in_ch, self.tel.clone(), rec_in_p, rec_out_p, mon_p);
        // Build the output BEFORE playing the input: WASAPI output init takes
        // tens of ms, and a live input would overflow the ring while nothing
        // consumes it. Neither stream runs yet, so the ring stays empty.
        let out_stream = out_dev
            .build_output_stream(
                out_sc,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    proc.run(&mut cons, data, out_ch);
                },
                err_fn,
                None,
            )
            .context("build output stream")?;
        in_stream.play().context("play input")?;
        // Prime to the drift target (half full) before the output starts: USB
        // mics can take ~100 ms to begin delivering (longer right after a
        // release/reopen on preset reload); a short deadline starved the first
        // callbacks. Bounded wait on the input callback's fill gauge.
        let prime_target = (block * in_ch.max(1) * (INPUT_RING_BLOCKS / 2)) as u32;
        let prime_deadline = Instant::now() + Duration::from_millis(1000);
        while self.tel.input_fill.load(Relaxed) < prime_target && Instant::now() < prime_deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        out_stream.play().context("play output")?;

        let actual_cfg = EngineConfig { sample_rate: actual_rate, block_size: block, ..cfg.clone() };
        self.running = Some(Running {
            cmd: cmd_p,
            ret: ret_c,
            in_stream,
            out_stream,
            cfg: actual_cfg.clone(),
            rec_in: rec_in_c,
            rec_out: rec_out_c,
            mon_cons: Some(mon_c),
            mon: None,
        });
        if actual_cfg.input.host == HostId::Asio {
            self.settings.last_asio_input = Some(actual_cfg.input.device.clone());
        }
        if actual_cfg.output.host == HostId::Asio {
            self.settings.last_asio_output = Some(actual_cfg.output.device.clone());
        }
        self.settings.last_config = Some(actual_cfg);
        let _ = settings::save_settings(&self.settings);
        self.tel.stream_fault.store(false, Relaxed);
        self.tel.input_fill.store(0, Relaxed);
        self.started_at = Instant::now();
        self.tel.running.store(true, Relaxed);
        self.tel.rate.store(actual_rate, Relaxed);
        self.tel.block.store(block as u32, Relaxed);
        self.tel.callback_max_ns.store(0, Relaxed);
        // Health counters describe THIS run, not the process lifetime: a hiccup
        // hours ago must not keep the GUI's "Сбои" red forever.
        self.tel.underruns.store(0, Relaxed);
        self.tel.overflows.store(0, Relaxed);
        self.tel.lock_skips.store(0, Relaxed);
        self.tel.plugin_errors.store(0, Relaxed);
        self.tel.nonfinite.store(0, Relaxed);
        self.tel.proc_calls.store(0, Relaxed);
        self.tel.block_min.store(u32::MAX, Relaxed);
        self.tel.block_max.store(0, Relaxed);
        for i in 0..MAX_SLOTS {
            self.tel.slot_proc_max_ns[i].store(0, Relaxed);
            self.tel.slot_proc_sum_ns[i].store(0, Relaxed);
        }
        self.update_latency();
        // Preset reload / reconnect must not leave the GUI's monitor button lying.
        if want_monitor {
            if let Err(e) = self.set_monitor_inner(true, None) {
                self.notice(Notice::Error { message: format!("монитор не восстановлен: {e}") });
            }
        }
        Ok(self.status())
    }

    fn stop_inner(&mut self) {
        self.reconnect = None;
        self.auto_start = None;
        self.tel.record_arm.store(false, Relaxed);
        // Fade the headphones out before the stream dies, so stop/reload never clicks.
        if self.running.as_ref().map(|r| r.mon.is_some()).unwrap_or(false) && self.tel.monitor_arm.load(Relaxed) {
            self.tel.monitor_arm.store(false, Relaxed);
            std::thread::sleep(Duration::from_millis(80));
        }
        self.tel.monitor_arm.store(false, Relaxed);
        if let Some(mut r) = self.running.take() {
            let expected = self.tel.side().chain.len();
            let _ = r.cmd.push(Command::Shutdown);
            // Stop the input FIRST: once the output callback sees Shutdown it
            // stops consuming, and a live input would overflow-drop the ring
            // (inflated XRUNS on every stop/preset reload). Teardown is silent.
            drop(r.in_stream);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut got = 0;
            while got < expected && Instant::now() < deadline {
                while let Ok(slot) = r.ret.pop() {
                    self.offline.push(slot);
                    got += 1;
                }
                if got < expected {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            while let Ok(slot) = r.ret.pop() {
                self.offline.push(slot);
            }
            drop(r.out_stream);
            if got < expected {
                self.notice(Notice::Error { message: format!("аудиопоток вернул {got}/{expected} слотов при остановке") });
            }
        }
        // Close editors: instances are offline now; the editor thread must not
        // idle effects that may be torn down later.
        let open: Vec<usize> = self
            .tel
            .side()
            .chain
            .iter()
            .enumerate()
            .filter(|(_, s)| s.editor_open)
            .map(|(i, _)| i)
            .collect();
        for i in open {
            self.close_editor_inner(i);
        }
        self.tel.running.store(false, Relaxed);
        self.tel.input_peak.store(0, Relaxed);
        self.tel.output_peak.store(0, Relaxed);
        for p in &self.tel.slot_peaks {
            p.store(0, Relaxed);
        }
        self.update_latency();
        let _ = settings::save_settings(&self.settings);
        self.autosave();
    }

    fn status(&self) -> EngineStatus {
        EngineStatus {
            running: self.tel.running.load(Relaxed),
            sample_rate: self.tel.rate.load(Relaxed),
            block_size: self.tel.block.load(Relaxed) as usize,
            latency_ms: f32::from_bits(self.tel.latency_ms.load(Relaxed)),
        }
    }

    // -- presets -------------------------------------------------------------

    fn save_preset_inner(&mut self, path: &Path) -> Result<()> {
        // Live snapshot (M4): no stop/restart — that gapped the audio. Retry
        // briefly if the audio thread holds an instance lock this very moment.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(p) = self.build_preset("micrack preset") {
                return preset::write(path, &p);
            }
            if Instant::now() >= deadline {
                bail!("не удалось сохранить состояние плагина (аудио занято); попробуйте снова");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn current_cfg(&self) -> EngineConfig {
        self.running
            .as_ref()
            .map(|r| r.cfg.clone())
            .or_else(|| self.settings.last_config.clone())
            .unwrap_or(EngineConfig {
                input: DeviceSel { host: HostId::Wasapi, device: "default".into() },
                output: DeviceSel { host: HostId::Wasapi, device: "default".into() },
                sample_rate: 48000,
                block_size: 256,
            })
    }

    /// None if any instance lock is held by the audio thread right now (B2):
    /// the caller skips this snapshot and retries instead of blocking audio.
    fn build_preset(&self, name: &str) -> Option<preset::Preset> {
        let cfg = self.current_cfg();
        Some(preset::Preset {
            version: preset::VERSION,
            name: name.into(),
            engine: preset::PresetEngine { sample_rate: cfg.sample_rate, block_size: cfg.block_size },
            input: preset::PresetIo { host: cfg.input.host, device: cfg.input.device.clone(), gain_db: self.gains_db.0 },
            output: preset::PresetIo { host: cfg.output.host, device: cfg.output.device.clone(), gain_db: self.gains_db.1 },
            chain: self.snapshot_chain()?,
        })
    }

    /// Snapshot via the shadow chain + instance registry: works offline AND
    /// while running. try_lock per instance — a busy slot aborts the snapshot
    /// (None) rather than stalling the audio callback behind save_state.
    fn snapshot_chain(&self) -> Option<Vec<preset::PresetSlot>> {
        let chain = self.tel.side().chain.clone();
        let mut out = Vec::with_capacity(chain.len());
        for s in &chain {
            let inst = self.instances.get(&s.id)?;
            let inst = match inst.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return None,
            };
            let info = inst.info();
            out.push(preset::PresetSlot {
                format: info.format,
                path: info.path,
                uid: info.uid,
                bypass: s.bypass,
                state_b64: preset::encode_state(&inst.save_state().unwrap_or_default()),
            });
        }
        Some(out)
    }

    /// Persist the chain for startup auto-restore. Never stops the engine.
    /// While running, writes are COALESCED (tick flushes at most every 500 ms):
    /// an fsync per mutation during a tuning storm causes storage-DPC jitter
    /// that can push the audio callback over its block budget. Offline
    /// mutations save immediately; exit forces a flush, so at most 500 ms of
    /// running mutations is ever at risk.
    fn autosave(&mut self) {
        self.autosave_pending = true;
        if self.running.is_none() {
            self.autosave_flush();
        }
    }

    fn autosave_flush(&mut self) {
        if !self.autosave_pending {
            return;
        }
        let Some(p) = self.build_preset("micrack last session") else {
            return; // audio holds a lock: retry on the next tick
        };
        self.autosave_pending = false;
        let dir = settings::app_dir();
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = preset::write(&dir.join("last-session.json"), &p) {
            tracing::warn!("last-session save: {e}");
        }
    }

    fn load_preset_inner(&mut self, path: &Path) -> Result<LoadReport> {
        let p = preset::read(path)?;
        let was_running = self.running.is_some();
        // Preset devices win on load (single source of truth); last_config is
        // only the auto-start fallback and is refreshed from this preset below.
        let cfg = EngineConfig {
            input: DeviceSel { host: p.input.host, device: p.input.device.clone() },
            output: DeviceSel { host: p.output.host, device: p.output.device.clone() },
            sample_rate: p.engine.sample_rate,
            block_size: p.engine.block_size,
        };
        if was_running {
            self.stop_inner();
        }
        // Clear current chain (editors were closed by stop_inner; close any offline ones).
        let open: Vec<usize> = self.tel.side().chain.iter().enumerate().filter(|(_, s)| s.editor_open).map(|(i, _)| i).collect();
        for i in open.into_iter().rev() {
            self.close_editor_inner(i);
        }
        self.offline.clear();
        self.latencies.clear();
        self.instances.clear();
        self.params.clear();
        self.tel.side().chain.clear();

        self.gains_db = (clamp_db(p.input.gain_db), clamp_db(p.output.gain_db));
        self.tel.input_gain.store(db_to_gain(self.gains_db.0).to_bits(), Relaxed);
        self.tel.master_gain.store(db_to_gain(self.gains_db.1).to_bits(), Relaxed);
        self.settings.input_gain_db = self.gains_db.0;
        self.settings.master_gain_db = self.gains_db.1;
        self.settings.last_config = Some(cfg.clone());

        let mut report = LoadReport::default();
        for ps in &p.chain {
            // Match by path first, then by uid against the scan cache.
            let resolved = if Path::new(&ps.path).exists() {
                Some((ps.path.clone(), ps.format))
            } else {
                self.cache
                    .iter()
                    .find(|c| !ps.uid.is_empty() && c.info.uid == ps.uid)
                    .map(|c| (c.path.clone(), c.info.format))
            };
            let mut inst = match resolved {
                Some((path, format)) => match plugins::load(&path, format, cfg.sample_rate, cfg.block_size) {
                    Ok(mut inst) => {
                        let state = preset::decode_state(&ps.state_b64).unwrap_or_default();
                        if let Err(e) = inst.load_state(&state) {
                            self.notice(Notice::Error { message: format!("восстановление состояния {path}: {e}") });
                        }
                        self.cache_upsert(&inst.info());
                        report.loaded.push(inst.info().name);
                        inst
                    }
                    Err(e) => {
                        self.notice(Notice::PluginMissing { path: path.clone() });
                        tracing::warn!("preset plugin load {path}: {e}");
                        report.missing.push(path.clone());
                        PluginInstance::Missing { path, format }
                    }
                },
                None => {
                    self.notice(Notice::PluginMissing { path: ps.path.clone() });
                    report.missing.push(ps.path.clone());
                    PluginInstance::Missing { path: ps.path.clone(), format: ps.format }
                }
            };
            prewarm(&mut inst, cfg.block_size);
            let params = inst.list_parameters().unwrap_or_default();
            let bypass = ps.bypass || inst.is_missing();
            let info = inst.info();
            let shared = Arc::new(Mutex::new(inst));
            let mut slot = Slot::new(shared.clone());
            slot.bypass = bypass;
            let slot_id = slot.id;
            self.instances.insert(slot_id, shared);
            self.params.insert(slot_id, params);
            let idx = self.offline.len();
            self.offline.push(slot);
            self.shadow_insert(idx, slot_id, &info, bypass);
        }
        if was_running {
            self.start_inner(cfg)?;
        }
        Ok(report)
    }

    // -- scan ----------------------------------------------------------------

    fn do_scan(&mut self) {
        let mut dirs = settings::default_dirs();
        dirs.extend(self.settings.extra_dirs.iter().cloned());
        let tel = self.tel.clone();
        self.entries = settings::scan(&dirs, &self.cache, |done, total, current| {
            tel.side().scan = Some(ScanProgress { done, total, current: current.to_string() });
        });
        self.tel.side().scan = None;
    }
}
