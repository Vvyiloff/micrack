//! VST2 mini-host: our own clean-room #[repr(C)] mirror of the well-known AEffect ABI
//! (ported from micrack-spike/src/bin/vst2.rs; no GPL header vendored).

use super::{ParamInfo, PluginFormat, PluginInfo};
use crate::engine::block::Block;
use anyhow::{Context, Result, bail};
use std::ffi::c_void;
use std::ptr::null_mut;

const MAGIC: i32 = 0x5673_7450; // "PtsV" little-endian
const EFF_OPEN: i32 = 0;
const EFF_CLOSE: i32 = 1;
const EFF_SET_SAMPLE_RATE: i32 = 10;
const EFF_SET_BLOCK_SIZE: i32 = 11;
const EFF_EDIT_GET_RECT: i32 = 13;
const EFF_EDIT_OPEN: i32 = 14;
const EFF_EDIT_CLOSE: i32 = 15;
const EFF_EDIT_IDLE: i32 = 19;
const EFF_GET_PARAM_LABEL: i32 = 6;
const EFF_GET_PARAM_DISPLAY: i32 = 7;
const EFF_GET_PARAM_NAME: i32 = 8;
const EFF_GET_CHUNK: i32 = 23;
const EFF_SET_CHUNK: i32 = 24;
const EFF_GET_EFFECT_NAME: i32 = 45;
const EFF_GET_PRODUCT_STRING: i32 = 48;
const EFF_START_PROCESS: i32 = 71;
const EFF_STOP_PROCESS: i32 = 72;
const FLAGS_HAS_EDITOR: i32 = 1;
const FLAGS_CAN_REPLACING: i32 = 1 << 4;
const FLAGS_PROGRAM_CHUNKS: i32 = 1 << 5;
const FLAGS_IS_SYNTH: i32 = 1 << 8;

type Dispatcher = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type ProcessFn = unsafe extern "C" fn(*mut AEffect, *mut *mut f32, *mut *mut f32, i32);
type SetParamFn = unsafe extern "C" fn(*mut AEffect, i32, f32);
type GetParamFn = unsafe extern "C" fn(*mut AEffect, i32) -> f32;
type AudioMaster = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type VstMain = unsafe extern "C" fn(AudioMaster) -> *mut AEffect;

#[repr(C)]
pub struct AEffect {
    magic: i32,
    dispatcher: Dispatcher,
    process: ProcessFn,
    set_parameter: SetParamFn,
    get_parameter: GetParamFn,
    num_programs: i32,
    num_params: i32,
    num_inputs: i32,
    num_outputs: i32,
    flags: i32,
    ptr1: *mut c_void,
    ptr2: *mut c_void,
    initial_delay: i32,
    empty2: [u8; 8],
    unkown_float: f32,
    object: *mut c_void,
    user: *mut c_void,
    unique_id: i32,
    version: i32,
    process_replacing: ProcessFn,
}

#[repr(C)]
struct ERect {
    top: i16,
    left: i16,
    bottom: i16,
    right: i16,
}

/// Per-instance host context, stored in effect.user (host-reserved field). No global statics.
struct Vst2Context {
    sample_rate: u32,
    block_size: usize,
}

unsafe extern "C" fn audio_master(
    e: *mut AEffect,
    opcode: i32,
    _index: i32,
    _value: isize,
    ptr: *mut c_void,
    _opt: f32,
) -> isize {
    unsafe {
        // May be called during VSTPluginMain before user is set.
        let (rate, block) = if !e.is_null() && !(*e).user.is_null() {
            let c = &*((*e).user as *const Vst2Context);
            (c.sample_rate, c.block_size)
        } else {
            (48000, 256)
        };
        match opcode {
            1 => 2400, // audioMasterVersion -> VST 2.4
            16 => rate as isize,
            17 => block as isize,
            7 => 0, // GetTime -> none
            32 | 33 => {
                if !ptr.is_null() {
                    let s = b"micrack\0";
                    std::ptr::copy_nonoverlapping(s.as_ptr(), ptr as *mut u8, s.len());
                }
                1
            }
            _ => 0,
        }
    }
}

unsafe fn dispatch(e: *mut AEffect, op: i32, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
    unsafe { ((*e).dispatcher)(e, op, index, value, ptr, opt) }
}

unsafe fn get_string(e: *mut AEffect, op: i32, index: i32) -> String {
    let mut buf = [0u8; 256];
    unsafe {
        dispatch(e, op, index, 0, buf.as_mut_ptr() as *mut c_void, 0.0);
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub struct Vst2Plugin {
    effect: *mut AEffect,
    info: PluginInfo,
    inbufs: Vec<Vec<f32>>,
    outbufs: Vec<Vec<f32>>,
    inptrs: Vec<*mut f32>,
    outptrs: Vec<*mut f32>,
    ctx: *mut Vst2Context,
    editor_open: bool,
    // Declared last: the DLL must unload only after effClose ran in Drop.
    _lib: libloading::Library,
}

// The effect is created on the control thread and processed on the audio thread
// (cross-thread use proven in the spike); never used from two threads at once.
unsafe impl Send for Vst2Plugin {}

/// Raw effEditIdle target for the editor thread while the slot is back in the audio chain.
#[derive(Clone, Copy)]
pub struct Vst2Idle {
    effect: *mut AEffect,
}
unsafe impl Send for Vst2Idle {}

impl Vst2Idle {
    /// # Safety: call only on the editor thread while the plugin is alive.
    pub unsafe fn call(&self) {
        unsafe {
            dispatch(self.effect, EFF_EDIT_IDLE, 0, 0, null_mut(), 0.0);
        }
    }
}

impl Vst2Plugin {
    pub fn load(path: &str, rate: u32, block: usize) -> Result<Self> {
        unsafe {
            let lib = libloading::Library::new(path).context("Library::new")?;
            let effect = {
                let main: libloading::Symbol<VstMain> = lib
                    .get(b"VSTPluginMain\0")
                    .or_else(|_| lib.get(b"main\0"))
                    .context("VSTPluginMain/main export")?;
                main(audio_master)
            };
            if effect.is_null() {
                bail!("VSTPluginMain returned null");
            }
            if (*effect).magic != MAGIC {
                bail!("bad AEffect magic: {:08x}", (*effect).magic);
            }
            if (*effect).flags & FLAGS_IS_SYNTH != 0 {
                dispatch(effect, EFF_CLOSE, 0, 0, null_mut(), 0.0);
                bail!("синтезаторы не поддерживаются");
            }
            if (*effect).flags & FLAGS_CAN_REPLACING == 0 {
                dispatch(effect, EFF_CLOSE, 0, 0, null_mut(), 0.0);
                bail!("plugin lacks processReplacing");
            }

            let ctx = Box::into_raw(Box::new(Vst2Context { sample_rate: rate, block_size: block }));
            (*effect).user = ctx as *mut c_void;

            let mut name = get_string(effect, EFF_GET_PRODUCT_STRING, 0);
            if name.is_empty() {
                name = get_string(effect, EFF_GET_EFFECT_NAME, 0);
            }
            let info = PluginInfo {
                name,
                vendor: String::new(),
                format: PluginFormat::Vst2,
                path: path.to_string(),
                uid: format!("{:08x}", (*effect).unique_id),
                audio_inputs: (*effect).num_inputs.max(0) as u32,
                audio_outputs: (*effect).num_outputs.max(0) as u32,
                has_gui: (*effect).flags & FLAGS_HAS_EDITOR != 0,
                latency: (*effect).initial_delay.max(0) as u32,
            };

            dispatch(effect, EFF_OPEN, 0, 0, null_mut(), 0.0);
            dispatch(effect, EFF_SET_SAMPLE_RATE, 0, 0, null_mut(), rate as f32);
            dispatch(effect, EFF_SET_BLOCK_SIZE, 0, block as isize, null_mut(), 0.0);
            dispatch(effect, EFF_START_PROCESS, 0, 0, null_mut(), 0.0);

            let nin = (*effect).num_inputs.max(1) as usize;
            let nout = (*effect).num_outputs.max(1) as usize;
            let mut inbufs: Vec<Vec<f32>> = (0..nin).map(|_| vec![0.0; block]).collect();
            let mut outbufs: Vec<Vec<f32>> = (0..nout).map(|_| vec![0.0; block]).collect();
            let inptrs = inbufs.iter_mut().map(|v| v.as_mut_ptr()).collect();
            let outptrs = outbufs.iter_mut().map(|v| v.as_mut_ptr()).collect();

            Ok(Self { effect, info, inbufs, outbufs, inptrs, outptrs, ctx, editor_open: false, _lib: lib })
        }
    }

    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    pub fn num_params(&self) -> i32 {
        unsafe { (*self.effect).num_params.max(0) }
    }

    /// VST2 parameter values are normalized 0..1.
    pub fn get_parameter(&self, i: i32) -> f32 {
        unsafe { ((*self.effect).get_parameter)(self.effect, i) }
    }

    pub fn set_parameter(&mut self, i: i32, v: f32) {
        unsafe { ((*self.effect).set_parameter)(self.effect, i, v.clamp(0.0, 1.0)) }
    }

    pub fn param_name(&self, i: i32) -> String {
        unsafe { get_string(self.effect, EFF_GET_PARAM_NAME, i) }
    }

    pub fn param_label(&self, i: i32) -> String {
        unsafe { get_string(self.effect, EFF_GET_PARAM_LABEL, i) }
    }

    pub fn param_display(&self, i: i32) -> String {
        unsafe { get_string(self.effect, EFF_GET_PARAM_DISPLAY, i) }
    }

    pub fn list_parameters(&self) -> Result<Vec<ParamInfo>> {
        Ok((0..self.num_params())
            .map(|i| ParamInfo {
                id: i as u32,
                name: self.param_name(i),
                value: self.get_parameter(i) as f64,
                min: 0.0,
                max: 1.0,
                unit: self.param_label(i),
                display: self.param_display(i),
            })
            .collect())
    }

    pub fn process(&mut self, b: &mut Block) -> Result<()> {
        let n = b.len.min(self.inbufs[0].len());
        let stereo_in = self.inbufs.len() > 1;
        for i in 0..n {
            self.inbufs[0][i] = b.left[i];
            if stereo_in {
                self.inbufs[1][i] = b.right[i];
            }
        }
        unsafe {
            ((*self.effect).process_replacing)(self.effect, self.inptrs.as_mut_ptr(), self.outptrs.as_mut_ptr(), n as i32);
        }
        let stereo_out = self.outbufs.len() > 1;
        for i in 0..n {
            b.left[i] = self.outbufs[0][i];
            b.right[i] = if stereo_out { self.outbufs[1][i] } else { self.outbufs[0][i] };
        }
        Ok(())
    }

    pub fn reconfigure(&mut self, rate: u32, block: usize) -> Result<()> {
        unsafe {
            dispatch(self.effect, EFF_STOP_PROCESS, 0, 0, null_mut(), 0.0);
            (*self.ctx).sample_rate = rate;
            (*self.ctx).block_size = block;
            dispatch(self.effect, EFF_SET_SAMPLE_RATE, 0, 0, null_mut(), rate as f32);
            dispatch(self.effect, EFF_SET_BLOCK_SIZE, 0, block as isize, null_mut(), 0.0);
            for v in &mut self.inbufs {
                v.resize(block, 0.0);
            }
            for v in &mut self.outbufs {
                v.resize(block, 0.0);
            }
            self.inptrs = self.inbufs.iter_mut().map(|v| v.as_mut_ptr()).collect();
            self.outptrs = self.outbufs.iter_mut().map(|v| v.as_mut_ptr()).collect();
            dispatch(self.effect, EFF_START_PROCESS, 0, 0, null_mut(), 0.0);
        }
        Ok(())
    }

    pub fn save_state(&self) -> Result<Vec<u8>> {
        unsafe {
            if (*self.effect).flags & FLAGS_PROGRAM_CHUNKS == 0 {
                return Ok(Vec::new());
            }
            let mut chunk_ptr: *mut c_void = null_mut();
            let size = dispatch(self.effect, EFF_GET_CHUNK, 0, 0, &mut chunk_ptr as *mut *mut c_void as *mut c_void, 0.0) as usize;
            if size == 0 || chunk_ptr.is_null() {
                return Ok(Vec::new());
            }
            Ok(std::slice::from_raw_parts(chunk_ptr as *const u8, size).to_vec())
        }
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        unsafe {
            dispatch(self.effect, EFF_SET_CHUNK, 0, data.len() as isize, data.as_ptr() as *mut c_void, 0.0);
        }
        Ok(())
    }

    /// # Safety: hwnd must be a live window on the calling (editor) thread.
    pub unsafe fn open_editor(&mut self, hwnd: *mut c_void) -> Result<()> {
        if !self.info.has_gui {
            bail!("у плагина нет редактора");
        }
        unsafe {
            // Host implementations differ: pass HWND in both value and ptr.
            dispatch(self.effect, EFF_EDIT_OPEN, 0, hwnd as isize, hwnd, 0.0);
        }
        self.editor_open = true;
        Ok(())
    }

    pub fn close_editor(&mut self) -> Result<()> {
        if self.editor_open {
            unsafe {
                dispatch(self.effect, EFF_EDIT_CLOSE, 0, 0, null_mut(), 0.0);
            }
            self.editor_open = false;
        }
        Ok(())
    }

    pub fn editor_idle(&mut self) {
        if self.editor_open {
            unsafe {
                dispatch(self.effect, EFF_EDIT_IDLE, 0, 0, null_mut(), 0.0);
            }
        }
    }

    pub fn editor_size(&self) -> (i32, i32) {
        unsafe {
            let mut rect_ptr: *const ERect = std::ptr::null();
            dispatch(self.effect, EFF_EDIT_GET_RECT, 0, 0, &mut rect_ptr as *mut *const ERect as *mut c_void, 0.0);
            if !rect_ptr.is_null() {
                let r = &*rect_ptr;
                let w = (r.right - r.left).max(100) as i32;
                let h = (r.bottom - r.top).max(100) as i32;
                return (w, h);
            }
        }
        (520, 400)
    }

    pub fn idle_handle(&self) -> Vst2Idle {
        Vst2Idle { effect: self.effect }
    }
}

impl Drop for Vst2Plugin {
    fn drop(&mut self) {
        unsafe {
            if self.editor_open {
                dispatch(self.effect, EFF_EDIT_CLOSE, 0, 0, null_mut(), 0.0);
            }
            dispatch(self.effect, EFF_STOP_PROCESS, 0, 0, null_mut(), 0.0);
            dispatch(self.effect, EFF_CLOSE, 0, 0, null_mut(), 0.0);
            // effClose may have freed the AEffect; do not dereference it afterwards.
            drop(Box::from_raw(self.ctx));
            self.ctx = null_mut();
        }
        // _lib drops last (field order), unloading the DLL after effClose.
    }
}