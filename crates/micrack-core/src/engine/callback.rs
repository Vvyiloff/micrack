//! RT output-callback processor. No allocation; commands via rtrb, meters/gains
//! via atomics, buffers preallocated. Each slot's instance is shared with the
//! control thread via Arc<Mutex<..>> (the vst3-host model): the callback uses
//! try_lock — if the control thread holds the instance (autosave snapshot,
//! editor open/close/idle), the slot CROSSFADES from its last wet output toward
//! dry over that block instead of snapping dry (N1b: no zipper during tuning).
//! Parameter writes arrive as Command::SetParam and are applied here on the
//! audio thread (N1a), so control-thread tuning never contends for the lock.
//! Input samples are read through the drift-compensating fractional reader (B3).

use crate::api::Tel;
use crate::engine::block::Block;
use crate::engine::chain::{Command, MAX_SLOTS, Slot};
use crate::engine::drift::Drift;
use std::sync::Arc;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::time::{Duration, Instant};

/// Input ring depth in blocks (api.rs allocates, Drift targets half of it).
/// 8 blocks = ~43 ms of headroom at 512/48k. This machine's DPC/USB jitter
/// occasionally stalls delivery past 21 ms, which a 4-block ring could not
/// absorb; the user chose zero xruns over the extra ~21 ms of latency.
pub const INPUT_RING_BLOCKS: usize = 8;

/// Headphones-monitor ring depth in blocks (~85 ms at 512/48k). Allocated once
/// per engine run so the producer never has to be swapped on the audio thread.
pub const MON_RING_BLOCKS: usize = 8;
/// Blocks buffered before the monitor feed starts rendering (~21 ms).
const MON_PRIME_BLOCKS: usize = 2;
/// Monitor fade length in seconds, both directions.
const MON_FADE_S: f32 = 0.02;

pub struct AudioProcessor {
    chain: Vec<Slot>,
    cmd: rtrb::Consumer<Command>,
    ret: rtrb::Producer<Slot>,
    block: Block,
    scratch: Block,
    staging: Vec<f32>,
    tel: Arc<Tel>,
    in_ch: usize,
    shutdown: bool,
    /// Currently applied gains, ramped toward the tel targets (~50 ms) so an
    /// agent's gain change never clicks or spikes.
    gain_in: f32,
    gain_out: f32,
    /// Recording taps: 0 = dry input, 1 = processed output. Preallocated rings.
    rec_in: rtrb::Producer<f32>,
    rec_out: rtrb::Producer<f32>,
    /// Headphones monitor feed (processed output), gated by tel.monitor_arm.
    mon_out: rtrb::Producer<f32>,
    /// Clock-drift compensation for the input ring (B3).
    drift: Drift,
    /// Stream start: fade-in window and underrun-count grace (USB warmup gaps
    /// right after a device (re)open are expected, not runtime health).
    started: Instant,
    /// Windowed callback max: publish the worst block of each ~1 s window so
    /// load% reflects recent reality, not a lifetime spike.
    win_max: u64,
    win_deadline: Instant,
    /// N1a: parameter writes deferred because the instance lock was held
    /// (editor/autosave). Retried at the top of each callback, FIFO. Capped at
    /// the command-ring depth; sustained contention overflow drops the write
    /// (the GUI/agent re-sends) rather than allocating on the audio thread.
    pending_params: Vec<(u64, u32, f64)>,
    /// N1b: each slot's last wet output, so a contended block can crossfade
    /// wet->dry smoothly instead of snapping. Preallocated, one Block per slot.
    wet: Vec<Block>,
}

impl AudioProcessor {
    pub(crate) fn new(
        chain: Vec<Slot>,
        cmd: rtrb::Consumer<Command>,
        ret: rtrb::Producer<Slot>,
        block_size: usize,
        in_ch: usize,
        tel: Arc<Tel>,
        rec_in: rtrb::Producer<f32>,
        rec_out: rtrb::Producer<f32>,
        mon_out: rtrb::Producer<f32>,
    ) -> Self {
        let mut chain = chain;
        chain.reserve(MAX_SLOTS);
        // Fade in from silence (~50 ms ramp): masks the zero-fill boundary of
        // stream-start starvation (USB warmup) so resume never clicks.
        let gain_in = 0.0;
        let gain_out = 0.0;
        Self {
            chain,
            cmd,
            ret,
            block: Block::new(block_size),
            scratch: Block::new(block_size),
            staging: vec![0.0; block_size * in_ch.max(1)],
            tel,
            in_ch: in_ch.max(1),
            shutdown: false,
            gain_in,
            gain_out,
            rec_in,
            rec_out,
            mon_out,
            drift: Drift::new(block_size, in_ch.max(1), block_size * INPUT_RING_BLOCKS),
            started: Instant::now(),
            win_max: 0,
            win_deadline: Instant::now() + Duration::from_secs(1),
            pending_params: Vec::with_capacity(64),
            wet: (0..MAX_SLOTS).map(|_| Block::new(block_size)).collect(),
        }
    }

    /// One cpal output callback. `data` is interleaved f32, `out_ch` channels.
    pub fn run(&mut self, cons: &mut rtrb::Consumer<f32>, data: &mut [f32], out_ch: usize) {
        if out_ch == 0 {
            return;
        }
        let t0 = Instant::now();
        self.drain_commands();
        if self.shutdown {
            data.fill(0.0);
        } else {
            let cap = self.block.capacity.max(1);
            for chunk in data.chunks_mut(cap * out_ch) {
                self.run_block(cons, chunk, out_ch);
            }
        }
        let ns = t0.elapsed().as_nanos() as u64;
        self.win_max = self.win_max.max(ns);
        if t0 >= self.win_deadline {
            self.tel.callback_max_ns.store(self.win_max, Relaxed);
            self.win_max = ns;
            self.win_deadline = t0 + Duration::from_secs(1);
        }
    }

    fn drain_commands(&mut self) {
        // N1a: retry parameter writes deferred by a contended instance lock.
        // FIFO before new commands so an older write never overwrites a newer one.
        let chain = &mut self.chain;
        self.pending_params.retain(|&(id, p, v)| !apply_param(chain, id, p, v));
        while let Ok(cmd) = self.cmd.pop() {
            match cmd {
                Command::Insert(i, slot) => {
                    if self.chain.len() >= MAX_SLOTS {
                        // full: bounce back for teardown off the audio thread
                        let _ = self.ret.push(slot);
                    } else {
                        self.chain.insert(i.min(self.chain.len()), slot);
                    }
                }
                Command::Remove(i) => {
                    if i < self.chain.len() {
                        let s = self.chain.remove(i);
                        let _ = self.ret.push(s);
                    }
                }
                Command::Move(f, t) => {
                    if f < self.chain.len() {
                        let s = self.chain.remove(f);
                        self.chain.insert(t.min(self.chain.len()), s);
                    }
                }
                Command::SetBypass(i, b) => {
                    if let Some(s) = self.chain.get_mut(i) {
                        s.bypass = b;
                    }
                }
                Command::SetParam(id, p, v) => {
                    // Apply now if nothing is deferred and the lock is free;
                    // otherwise queue behind pending writes to preserve order.
                    if !(self.pending_params.is_empty() && apply_param(&mut self.chain, id, p, v))
                        && self.pending_params.len() < 64
                    {
                        self.pending_params.push((id, p, v));
                    }
                }
                Command::Shutdown => {
                    for s in self.chain.drain(..) {
                        let _ = self.ret.push(s);
                    }
                    self.shutdown = true;
                }
            }
        }
    }

    fn run_block(&mut self, cons: &mut rtrb::Consumer<f32>, data: &mut [f32], out_ch: usize) {
        let n = (data.len() / out_ch).min(self.block.capacity);
        if n == 0 {
            return;
        }
        let need = n * self.in_ch;
        // Diagnostics: the real per-callback frame count (WASAPI varies it).
        self.tel.block_last.store(n as u32, Relaxed);
        self.tel.block_min.fetch_min(n as u32, Relaxed);
        self.tel.block_max.fetch_max(n as u32, Relaxed);
        // Drift-compensated fractional read (B3): linear interpolation, PI servo
        // on ring fill. Underrun frames come back zero-filled and counted.
        let under = self.drift.read(cons, &mut self.staging[..need], n);
        if under > 0 && self.started.elapsed() > Duration::from_millis(250) {
            self.tel.underruns.fetch_add(under as u64, Relaxed);
        }
        self.tel.drift_ratio.store((self.drift.ratio() as f32).to_bits(), Relaxed);
        self.block.fill_from_interleaved(&self.staging[..need], self.in_ch);

        // Dry mic tap: after de-interleave, before input gain.
        let record_claimed = self.tel.record_arm.load(Relaxed);
        let (recording, tap) = if record_claimed {
            // Claim first, then recheck the arm. A control-side disarm either
            // sees this claim or makes the old callback skip its recorder write.
            self.tel.record_inflight.store(true, SeqCst);
            (self.tel.record_arm.load(SeqCst), self.tel.record_tap.load(Relaxed))
        } else {
            (false, 0)
        };
        if recording && tap == 0 {
            push_rec(&mut self.rec_in, &self.block, &self.tel.record_dropped);
        }

        // ~50 ms gain ramp, stepped once per block.
        let ramp = (n as f32 / self.tel.rate.load(Relaxed).max(1) as f32 / 0.05).min(1.0);
        self.gain_in += (f32::from_bits(self.tel.input_gain.load(Relaxed)) - self.gain_in) * ramp;
        self.gain_out += (f32::from_bits(self.tel.master_gain.load(Relaxed)) - self.gain_out) * ramp;
        self.block.apply_gain(self.gain_in);
        self.tel.input_peak.store(self.block.peak().to_bits(), Relaxed);

        let mut i = 0;
        while i < self.chain.len() {
            let slot = &mut self.chain[i];
            let target = if slot.bypass { 0.0 } else { 1.0 };
            if slot.fade == 0.0 && target == 0.0 {
                self.tel.slot_peaks[i].store(self.block.peak().to_bits(), Relaxed);
                i += 1;
                continue;
            }
            // try_lock (B2/N1b): a contended slot never stalls the audio thread
            // behind autosave/editor work. Instead of snapping dry it crossfades
            // from its last wet output toward dry over this block, and ramps
            // fade->0 so the next free block re-fades wet in smoothly.
            let locked = match slot.instance.try_lock() {
                Ok(g) => Some(g),
                Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            };
            if let Some(mut inst) = locked {
                if slot.fade == 1.0 && target == 1.0 {
                    // steady state: process in place, no crossfade copy
                    let t0 = Instant::now();
                    let r = inst.process(&mut self.block);
                    let dt = t0.elapsed().as_nanos() as u64;
                    self.tel.slot_proc_max_ns[i].fetch_max(dt, Relaxed);
                    self.tel.slot_proc_sum_ns[i].fetch_add(dt, Relaxed);
                    if r.is_err() {
                        self.tel.plugin_errors.fetch_add(1, Relaxed);
                    }
                    drop(inst);
                } else {
                    // one-block linear dry/wet ramp (~5 ms) to avoid clicks
                    self.scratch.copy_from(&self.block);
                    let t0 = Instant::now();
                    let r = inst.process(&mut self.block);
                    let dt = t0.elapsed().as_nanos() as u64;
                    self.tel.slot_proc_max_ns[i].fetch_max(dt, Relaxed);
                    self.tel.slot_proc_sum_ns[i].fetch_add(dt, Relaxed);
                    if r.is_err() {
                        self.tel.plugin_errors.fetch_add(1, Relaxed);
                    }
                    drop(inst);
                    let start = slot.fade;
                    let step = (target - start) / n as f32;
                    for k in 0..n {
                        let g = start + step * k as f32;
                        self.block.left[k] = self.scratch.left[k] * (1.0 - g) + self.block.left[k] * g;
                        self.block.right[k] = self.scratch.right[k] * (1.0 - g) + self.block.right[k] * g;
                    }
                }
                slot.fade = target;
                // Cache this slot's wet output for contention crossfades (N1b).
                self.wet[i].copy_from(&self.block);
            } else {
                self.tel.lock_skips.fetch_add(1, Relaxed);
                // Contended: ramp wet gain toward 0 over the block, mixing the
                // cached wet output with the dry signal — no instant dry snap.
                let start = slot.fade;
                let step = -start / n as f32;
                for k in 0..n {
                    let g = start + step * k as f32;
                    self.block.left[k] = self.wet[i].left[k] * g + self.block.left[k] * (1.0 - g);
                    self.block.right[k] = self.wet[i].right[k] * g + self.block.right[k] * (1.0 - g);
                }
                slot.fade = 0.0;
            }
            self.tel.slot_peaks[i].store(self.block.peak().to_bits(), Relaxed);
            i += 1;
        }
        for j in self.chain.len()..MAX_SLOTS {
            self.tel.slot_peaks[j].store(0f32.to_bits(), Relaxed);
        }

        self.block.apply_gain(self.gain_out);
        self.block.soft_limit();
        if !self.chain.is_empty() {
            self.tel.proc_calls.fetch_add(1, Relaxed);
        }
        // A plugin emitting NaN/Inf would reach the DAC as full-scale garbage.
        // Count it (never hide it) and emit silence for that block instead.
        if !self.block.left[..self.block.len].iter().all(|s| s.is_finite())
            || !self.block.right[..self.block.len].iter().all(|s| s.is_finite())
        {
            self.tel.nonfinite.fetch_add(1, Relaxed);
            self.block.clear();
        }
        // Voice is mono: some plugins (RNNoise VST2) declare stereo I/O but
        // process only the left channel and zero the right. Center the voice
        // by duplicating the processed left channel to both ears.
        self.block.right[..self.block.len].copy_from_slice(&self.block.left[..self.block.len]);
        self.tel.output_peak.store(self.block.peak().to_bits(), Relaxed);
        if recording && tap == 1 {
            push_rec(&mut self.rec_out, &self.block, &self.tel.record_dropped);
        }
        if record_claimed {
            self.tel.record_inflight.store(false, SeqCst);
        }
        if self.tel.monitor_arm.load(Relaxed) {
            push_rec(&mut self.mon_out, &self.block, &self.tel.monitor_gaps);
        }
        self.block.write_to_interleaved(data, out_ch);
    }
}

/// Apply a parameter write on the audio thread (N1a). Returns false ONLY when
/// the instance lock is held elsewhere (editor/autosave) so the caller defers to
/// the next block. A missing slot or plugin-rejected id is consumed (true): no
/// retry, no allocation, never blocks.
fn apply_param(chain: &mut [Slot], id: u64, param: u32, value: f64) -> bool {
    let Some(slot) = chain.iter_mut().find(|s| s.id == id) else { return true };
    match slot.instance.try_lock() {
        Ok(mut g) => {
            let _ = g.set_parameter(param, value);
            true
        }
        Err(std::sync::TryLockError::Poisoned(e)) => {
            let _ = e.into_inner().set_parameter(param, value);
            true
        }
        Err(std::sync::TryLockError::WouldBlock) => false,
    }
}

/// Interleaved stereo push into a preallocated ring. Full ring: drop the frame
/// and count it — never allocate or block on the audio thread.
fn push_rec(prod: &mut rtrb::Producer<f32>, b: &Block, dropped: &std::sync::atomic::AtomicU64) {
    for i in 0..b.len {
        if prod.slots() < 2 {
            dropped.fetch_add(1, Relaxed);
            continue;
        }
        let _ = prod.push(b.left[i]);
        let _ = prod.push(b.right[i]);
    }
}

/// Headphones monitor feed: drains the processed-output ring into micrack's own
/// second output stream. Runs on that stream's callback thread, never on the mic
/// callback. Self-primes and slews its gain so toggling never clicks; a starved
/// ring emits silence and counts a gap instead of blocking or repeating audio.
pub struct MonitorFeed {
    cons: rtrb::Consumer<f32>,
    ch: usize,
    prime: usize,
    fade_frames: usize,
    slew_in: f32,
    slew_out: f32,
    gain: f32,
    armed: bool,
    primed: bool,
}

impl MonitorFeed {
    pub fn new(cons: rtrb::Consumer<f32>, channels: usize, rate: u32, block: usize) -> Self {
        let fade_frames = ((rate.max(1) as f32 * MON_FADE_S) as usize).max(1);
        Self {
            cons,
            ch: channels.max(1),
            prime: block * 2 * MON_PRIME_BLOCKS,
            fade_frames,
            slew_in: 1.0 / fade_frames as f32,
            slew_out: 1.0 / fade_frames as f32,
            gain: 0.0,
            armed: false,
            primed: false,
        }
    }

    pub fn run(&mut self, data: &mut [f32], armed: bool, gaps: &std::sync::atomic::AtomicU64) {
        if armed != self.armed {
            if armed {
                while self.cons.pop().is_ok() {} // discard the previous session's tail
                self.primed = false;
            } else {
                // The producer stops at disarm, so whatever is buffered is all the
                // audio left. Fade across exactly that (capped), so gain reaches
                // zero no later than the ring runs dry — never an abrupt cut.
                let left = self.cons.slots() / 2;
                self.slew_out = 1.0 / left.min(self.fade_frames).max(1) as f32;
            }
            self.armed = armed;
        }
        // Prime to at least one full callback: a device with a long period would
        // otherwise starve on its very first rendered buffer.
        if !self.primed && self.cons.slots() >= self.prime.max(data.len()) {
            self.primed = true;
        }
        // Fade only while actually rendering: ramping up during priming would
        // end the fade before the first sample and start at full level.
        let target = if armed && self.primed { 1.0 } else { 0.0 };
        let slew = if armed { self.slew_in } else { self.slew_out };
        for frame in data.chunks_mut(self.ch) {
            let (l, r) = if self.primed && self.cons.slots() >= 2 {
                (self.cons.pop().unwrap_or(0.0), self.cons.pop().unwrap_or(0.0))
            } else {
                if armed && self.primed {
                    gaps.fetch_add(1, Relaxed);
                }
                (0.0, 0.0)
            };
            self.gain += (target - self.gain).clamp(-slew, slew);
            let (l, r) = (l * self.gain, r * self.gain);
            if self.ch == 1 {
                frame[0] = (l + r) * 0.5;
            } else {
                frame[0] = l;
                frame[1] = r;
                for s in &mut frame[2..] {
                    *s = 0.0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{PluginFormat, PluginInstance};
    use std::sync::Mutex;

    /// B2: while the control thread holds the instance lock, the callback must
    /// not block — the slot passes dry for that block and lock_skips counts it.
    #[test]
    fn contended_slot_passes_dry_without_blocking() {
        let tel = Arc::new(Tel::new());
        let block = 64usize;
        let (cmd_p, cmd_c) = rtrb::RingBuffer::<Command>::new(8);
        let (ret_p, ret_c) = rtrb::RingBuffer::<Slot>::new(8);
        let (rin_p, _rin_c) = rtrb::RingBuffer::<f32>::new(64);
        let (rout_p, _rout_c) = rtrb::RingBuffer::<f32>::new(64);
        let (mon_p, _mon_c) = rtrb::RingBuffer::<f32>::new(64);
        let inst = Arc::new(Mutex::new(PluginInstance::Missing { path: "x.dll".into(), format: PluginFormat::Vst2 }));
        let slot = Slot::new(inst.clone());
        let mut proc = AudioProcessor::new(vec![slot], cmd_c, ret_p, block, 1, tel.clone(), rin_p, rout_p, mon_p);
        let (mut in_p, mut in_c) = rtrb::RingBuffer::<f32>::new(block * INPUT_RING_BLOCKS);
        let mut data = vec![0.0f32; block];
        // Warm up so the start fade-in settles (~unity gain) before asserting
        // levels. Push faster than consumed (drop on full) to keep the ring fed.
        for _ in 0..200 {
            for _ in 0..block * 3 / 2 {
                let _ = in_p.push(0.25);
            }
            proc.run(&mut in_c, &mut data, 1);
        }
        let guard = inst.lock().unwrap(); // control thread "owns" the instance
        for _ in 0..block {
            let _ = in_p.push(0.25);
        }
        let t0 = Instant::now();
        proc.run(&mut in_c, &mut data, 1);
        assert!(t0.elapsed() < Duration::from_millis(50), "callback blocked on a contended instance");
        assert_eq!(tel.lock_skips.load(Relaxed), 1);
        // Dry passthrough at settled unity gain (fade-in finished during warmup).
        assert!(data.iter().all(|&s| (s - 0.25).abs() < 0.01), "contended slot must pass dry");
        drop(guard);
        for _ in 0..block {
            let _ = in_p.push(0.25);
        }
        proc.run(&mut in_c, &mut data, 1);
        assert_eq!(tel.lock_skips.load(Relaxed), 1, "uncontended block must process normally");
        drop(cmd_p);
        drop(ret_c);
    }

    /// N1a: Command::SetParam is consumed on the audio thread; under contention
    /// it is deferred (FIFO) and applied once the lock frees, never blocking.
    #[test]
    fn setparam_applied_on_audio_thread_and_deferred_under_contention() {
        let tel = Arc::new(Tel::new());
        let block = 64usize;
        let (mut cmd_p, cmd_c) = rtrb::RingBuffer::<Command>::new(8);
        let (ret_p, ret_c) = rtrb::RingBuffer::<Slot>::new(8);
        let (rin_p, _rin_c) = rtrb::RingBuffer::<f32>::new(64);
        let (rout_p, _rout_c) = rtrb::RingBuffer::<f32>::new(64);
        let (mon_p, _mon_c) = rtrb::RingBuffer::<f32>::new(64);
        let inst = Arc::new(Mutex::new(PluginInstance::Missing { path: "x.dll".into(), format: PluginFormat::Vst2 }));
        let slot = Slot::new(inst.clone());
        let slot_id = slot.id;
        let mut proc = AudioProcessor::new(vec![slot], cmd_c, ret_p, block, 1, tel.clone(), rin_p, rout_p, mon_p);
        let (mut in_p, mut in_c) = rtrb::RingBuffer::<f32>::new(block * INPUT_RING_BLOCKS);
        let mut data = vec![0.0f32; block];
        fn feed(in_p: &mut rtrb::Producer<f32>, block: usize) {
            for _ in 0..block {
                let _ = in_p.push(0.25);
            }
        }
        // Uncontended: the command is drained from the ring this callback.
        cmd_p.push(Command::SetParam(slot_id, 0, 0.5)).unwrap();
        assert_eq!(cmd_p.slots(), 7);
        feed(&mut in_p, block);
        proc.run(&mut in_c, &mut data, 1);
        assert_eq!(cmd_p.slots(), 8, "SetParam must be consumed on the audio thread");
        assert_eq!(tel.lock_skips.load(Relaxed), 0);
        // Contended: lock held -> write deferred, slot crossfades dry, no blocking.
        let guard = inst.lock().unwrap();
        cmd_p.push(Command::SetParam(slot_id, 0, 0.7)).unwrap();
        feed(&mut in_p, block);
        let t0 = Instant::now();
        proc.run(&mut in_c, &mut data, 1);
        assert!(t0.elapsed() < Duration::from_millis(50), "callback blocked on a contended SetParam");
        assert_eq!(cmd_p.slots(), 8, "command drained even while the write is deferred");
        assert_eq!(tel.lock_skips.load(Relaxed), 1);
        drop(guard);
        // Lock free: the deferred write applies, no new skip.
        feed(&mut in_p, block);
        proc.run(&mut in_c, &mut data, 1);
        assert_eq!(tel.lock_skips.load(Relaxed), 1, "deferred write must apply once the lock frees");
        drop(cmd_p);
        drop(ret_c);
    }

    /// N1b: a contended block crossfades from the cached wet output toward dry
    /// (a ramp across the block), not an instant dry snap. A level change makes
    /// wet != dry so the ramp is observable even with a unity (Missing) plugin.
    #[test]
    fn contended_slot_crossfades_wet_to_dry() {
        let tel = Arc::new(Tel::new());
        let block = 64usize;
        let (cmd_p, cmd_c) = rtrb::RingBuffer::<Command>::new(8);
        let (ret_p, ret_c) = rtrb::RingBuffer::<Slot>::new(8);
        let (rin_p, _rin_c) = rtrb::RingBuffer::<f32>::new(64);
        let (rout_p, _rout_c) = rtrb::RingBuffer::<f32>::new(64);
        let (mon_p, _mon_c) = rtrb::RingBuffer::<f32>::new(64);
        let inst = Arc::new(Mutex::new(PluginInstance::Missing { path: "x.dll".into(), format: PluginFormat::Vst2 }));
        let slot = Slot::new(inst.clone());
        let mut proc = AudioProcessor::new(vec![slot], cmd_c, ret_p, block, 1, tel.clone(), rin_p, rout_p, mon_p);
        let (mut in_p, mut in_c) = rtrb::RingBuffer::<f32>::new(block * INPUT_RING_BLOCKS);
        let mut data = vec![0.0f32; block];
        // Warm up at 0.25: fade->1, gains settle to unity, wet cache = 0.25.
        for _ in 0..200 {
            for _ in 0..block * 3 / 2 {
                let _ = in_p.push(0.25);
            }
            proc.run(&mut in_c, &mut data, 1);
        }
        // Hold the lock, flush stale 0.25, feed a NEW level (0.5): dry=0.5, wet=0.25.
        let guard = inst.lock().unwrap();
        while in_c.pop().is_ok() {}
        for _ in 0..block + 2 {
            let _ = in_p.push(0.5);
        }
        proc.run(&mut in_c, &mut data, 1);
        assert_eq!(tel.lock_skips.load(Relaxed), 1);
        assert!(data[0] < 0.35, "contended block snapped dry at the start: {}", data[0]);
        assert!(data[block - 1] > 0.45, "contended block did not reach dry by the end: {}", data[block - 1]);
        assert!(data[0] < data[block - 1], "no wet->dry ramp across the block");
        drop(guard);
        drop(cmd_p);
        drop(ret_c);
    }

    /// The monitor tap is gated by monitor_arm: off = ring untouched, on = one
    /// interleaved stereo copy of the processed block per callback.
    #[test]
    fn monitor_tap_pushes_only_when_armed() {
        let tel = Arc::new(Tel::new());
        let block = 64usize;
        let (cmd_p, cmd_c) = rtrb::RingBuffer::<Command>::new(8);
        let (ret_p, ret_c) = rtrb::RingBuffer::<Slot>::new(8);
        let (rin_p, _rin_c) = rtrb::RingBuffer::<f32>::new(64);
        let (rout_p, _rout_c) = rtrb::RingBuffer::<f32>::new(64);
        let (mon_p, mut mon_c) = rtrb::RingBuffer::<f32>::new(block * 2 * MON_RING_BLOCKS);
        let inst = Arc::new(Mutex::new(PluginInstance::Missing { path: "x.dll".into(), format: PluginFormat::Vst2 }));
        let mut proc = AudioProcessor::new(vec![Slot::new(inst)], cmd_c, ret_p, block, 1, tel.clone(), rin_p, rout_p, mon_p);
        let (mut in_p, mut in_c) = rtrb::RingBuffer::<f32>::new(block * INPUT_RING_BLOCKS);
        let mut data = vec![0.0f32; block];
        for _ in 0..50 {
            for _ in 0..block * 3 / 2 {
                let _ = in_p.push(0.25);
            }
            proc.run(&mut in_c, &mut data, 1);
        }
        assert_eq!(mon_c.slots(), 0, "disarmed monitor must not be fed");
        tel.monitor_arm.store(true, Relaxed);
        for _ in 0..block {
            let _ = in_p.push(0.25);
        }
        proc.run(&mut in_c, &mut data, 1);
        assert_eq!(mon_c.slots(), block * 2, "armed monitor must receive the processed block");
        assert_eq!(tel.monitor_gaps.load(Relaxed), 0);
        tel.monitor_arm.store(false, Relaxed);
        while mon_c.pop().is_ok() {}
        for _ in 0..block {
            let _ = in_p.push(0.25);
        }
        proc.run(&mut in_c, &mut data, 1);
        assert_eq!(mon_c.slots(), 0, "disarming must stop the feed");
        drop(cmd_p);
        drop(ret_c);
    }

    fn push_sine(p: &mut rtrb::Producer<f32>, frames: usize, phase: &mut f32) {
        for _ in 0..frames {
            let s = 0.5 * phase.sin();
            *phase += 2.0 * std::f32::consts::PI * 1000.0 / 48000.0;
            let _ = p.push(s);
            let _ = p.push(s);
        }
    }

    fn max_step(samples: &[f32]) -> f32 {
        samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max)
    }

    /// Toggling must slew the gain, not switch it: a 1 kHz sine at 0.5 has a
    /// ~0.065 sample-to-sample step, so anything near 0.5 is a click.
    #[test]
    fn monitor_feed_fades_in_and_out_without_clicks() {
        let block = 512usize;
        let gaps = std::sync::atomic::AtomicU64::new(0);
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(block * 2 * MON_RING_BLOCKS);
        let mut feed = MonitorFeed::new(c, 2, 48000, block);
        let mut out = vec![0.0f32; block * 2];
        let mut phase = 0.0f32;

        feed.run(&mut out, true, &gaps); // rising edge flushes, then waits for audio
        push_sine(&mut p, block, &mut phase); // below the prime target
        feed.run(&mut out, true, &gaps);
        assert!(out.iter().all(|&s| s == 0.0), "must stay silent until primed");
        assert_eq!(gaps.load(Relaxed), 0, "priming is not a gap");

        push_sine(&mut p, block, &mut phase);
        let mut on = Vec::new();
        for _ in 0..8 {
            push_sine(&mut p, block, &mut phase);
            feed.run(&mut out, true, &gaps);
            on.extend_from_slice(&out);
        }
        let left: Vec<f32> = on.iter().step_by(2).copied().collect();
        assert!(left[0].abs() < 0.01, "fade-in must start near silence: {}", left[0]);
        assert!(max_step(&left) < 0.08, "click during fade-in: {}", max_step(&left));
        let tail_peak = left[left.len() - 480..].iter().fold(0f32, |a, s| a.max(s.abs()));
        assert!((tail_peak - 0.5).abs() < 0.02, "monitor must reach unity: {tail_peak}");

        let mut off = Vec::new();
        for _ in 0..4 {
            feed.run(&mut out, false, &gaps);
            off.extend_from_slice(&out);
        }
        let left_off: Vec<f32> = off.iter().step_by(2).copied().collect();
        assert!(max_step(&left_off) < 0.08, "click during fade-out: {}", max_step(&left_off));
        assert!(left_off[..480].iter().any(|s| s.abs() > 0.1), "fade-out cut the tail instantly");
        assert!(left_off[left_off.len() - 480..].iter().all(|&s| s == 0.0), "monitor must end silent");
        assert_eq!(gaps.load(Relaxed), 0, "a disarmed drain is not a gap");
    }

    /// Starvation while armed is a real audible gap and must be counted.
    #[test]
    fn monitor_feed_counts_starvation_while_armed() {
        let block = 512usize;
        let gaps = std::sync::atomic::AtomicU64::new(0);
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(block * 2 * MON_RING_BLOCKS);
        let mut feed = MonitorFeed::new(c, 2, 48000, block);
        let mut out = vec![0.0f32; block * 2];
        let mut phase = 0.0f32;
        feed.run(&mut out, true, &gaps); // rising edge flushes the (empty) ring
        push_sine(&mut p, block * MON_PRIME_BLOCKS, &mut phase);
        feed.run(&mut out, true, &gaps);
        feed.run(&mut out, true, &gaps);
        assert_eq!(gaps.load(Relaxed), 0);
        feed.run(&mut out, true, &gaps);
        assert_eq!(gaps.load(Relaxed), block as u64, "every starved frame must be counted");
        assert!(out.iter().all(|&s| s == 0.0), "starvation must emit silence, not stale audio");
    }
}
