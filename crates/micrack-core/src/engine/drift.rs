//! Clock-drift compensation between the independent WASAPI input and output
//! streams (B3). The mic's USB clock and the virtual output clock drift apart;
//! a plain integer-frame ring periodically overflows/underruns -> clicks.
//! Here the output callback reads the input ring through a FRACTIONAL pointer
//! with linear interpolation, and a slow PI servo on the ring fill nudges the
//! read ratio (±1 % max) so the fill stays near half-full. RT-safe: every
//! buffer is preallocated; read() never allocates, locks or blocks.

/// PI gains. KP gives a ~5 s fill time constant at 256-frame blocks; KI trims
/// steady-state drift (100 ppm USB offset) without audible ratio movement.
const KP: f64 = 0.002;
const KI: f64 = 0.000002;
const RATIO_MIN: f64 = 0.99;
const RATIO_MAX: f64 = 1.01;

pub struct Drift {
    /// Interleaved scratch: [previous block's last frame, freshly popped frames].
    buf: Vec<f32>,
    /// Last input frame of the previous block (in_ch samples), interpolation anchor.
    prev: Vec<f32>,
    /// Fractional source position of the next output frame, in buf frame units.
    pos: f64,
    ratio: f64,
    integ: f64,
    in_ch: usize,
    /// Target ring fill in frames (half the ring).
    target: f64,
}

impl Drift {
    pub fn new(block: usize, in_ch: usize, ring_frames: usize) -> Self {
        let in_ch = in_ch.max(1);
        // Worst case read: pos < ~1.01, ratio <= 1.01 -> need <= 1.01*block + 2 frames.
        let cap = (block as f64 * RATIO_MAX) as usize + 3;
        Self {
            buf: vec![0.0; cap * in_ch],
            prev: vec![0.0; in_ch],
            pos: 0.0,
            ratio: 1.0,
            integ: 0.0,
            in_ch,
            target: (ring_frames / 2).max(1) as f64,
        }
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// PI servo on the ring fill, once per output block. Fill above target means
    /// the input clock runs fast -> consume slightly faster (ratio > 1).
    fn servo(&mut self, fill_frames: usize) {
        let err = (fill_frames as f64 - self.target) / self.target;
        self.integ = (self.integ + err).clamp(-2500.0, 2500.0);
        self.ratio = (1.0 + KP * err + KI * self.integ).clamp(RATIO_MIN, RATIO_MAX);
    }

    /// Read `n` output frames (interleaved in_ch) into `out[..n*in_ch]`.
    /// Returns the number of underrun frames (zero-filled). Never blocks.
    pub fn read(&mut self, cons: &mut rtrb::Consumer<f32>, out: &mut [f32], n: usize) -> usize {
        let in_ch = self.in_ch;
        if n == 0 {
            return 0;
        }
        let avail = cons.slots() / in_ch;
        self.servo(avail);
        let end = self.pos + (n as f64 - 1.0) * self.ratio;
        let need = end.floor() as usize + 1; // new frames to pop (buf[0] is prev)

        if avail < need {
            // Starved: consume what is there, zero-fill the tail, reset the servo
            // integrator so it does not wind up while the input is gone.
            let have = avail;
            self.buf[..in_ch].copy_from_slice(&self.prev);
            for i in 0..have * in_ch {
                self.buf[in_ch + i] = cons.pop().unwrap();
            }
            let mut k = 0;
            while k < n {
                let src = self.pos + k as f64 * self.ratio;
                if src + 1.0 > have as f64 {
                    break;
                }
                self.interp(out, k, src);
                k += 1;
            }
            out[k * in_ch..n * in_ch].fill(0.0);
            if have > 0 {
                self.prev.copy_from_slice(&self.buf[have * in_ch..(have + 1) * in_ch]);
            } else {
                self.prev.fill(0.0);
            }
            self.pos = 0.0;
            self.integ = 0.0;
            return n - k;
        }

        self.buf[..in_ch].copy_from_slice(&self.prev);
        for i in 0..need * in_ch {
            self.buf[in_ch + i] = cons.pop().unwrap();
        }
        for k in 0..n {
            let src = self.pos + k as f64 * self.ratio;
            self.interp(out, k, src);
        }
        // Carry the last popped frame as the next block's anchor; keep the
        // fractional remainder so no input sample is dropped or repeated.
        let mut carry = need;
        let mut next_pos = end + self.ratio - carry as f64;
        if next_pos < 0.0 {
            carry -= 1;
            next_pos += 1.0;
        }
        self.prev.copy_from_slice(&self.buf[carry * in_ch..(carry + 1) * in_ch]);
        self.pos = next_pos;
        0
    }

    fn interp(&self, out: &mut [f32], k: usize, src: f64) {
        let i0 = src.floor() as usize;
        let f = (src - i0 as f64) as f32;
        for c in 0..self.in_ch {
            let a = self.buf[i0 * self.in_ch + c];
            let b = self.buf[(i0 + 1) * self.in_ch + c];
            out[k * self.in_ch + c] = a + (b - a) * f;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate `seconds` of streaming with the input clock `ppm` parts-per-million
    /// off the output clock. Returns (max fill deviation after settling, underruns,
    /// overflow drops, final ratio).
    fn simulate(ppm: f64, seconds: usize) -> (f64, usize, usize, f64) {
        let block = 256usize;
        let ring_frames = block * 4;
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(ring_frames);
        let mut drift = Drift::new(block, 1, ring_frames);
        let mut out = vec![0.0f32; block];
        // Prime ~1.5 blocks like start_inner does.
        for _ in 0..block * 3 / 2 {
            prod.push(0.0).unwrap();
        }
        let mut in_acc = 0.0f64;
        let mut overflows = 0usize;
        let mut underruns = 0usize;
        let mut max_dev = 0.0f64;
        let blocks = seconds * 48000 / block;
        let settle = blocks / 4;
        for b in 0..blocks {
            in_acc += block as f64 * (1.0 + ppm / 1e6);
            let frames = in_acc.floor() as usize;
            in_acc -= frames as f64;
            for _ in 0..frames {
                if prod.slots() == 0 {
                    overflows += 1;
                } else {
                    prod.push(0.0).unwrap();
                }
            }
            let fill = cons.slots();
            if b > settle {
                max_dev = max_dev.max((fill as f64 - drift.target).abs() / drift.target);
            }
            underruns += drift.read(&mut cons, &mut out, block);
        }
        (max_dev, underruns, overflows, drift.ratio())
    }

    #[test]
    fn drift_fast_input_stays_stable() {
        // +1000 ppm is ~10x a real USB mic's worst offset; 60 s simulated.
        let (dev, under, over, ratio) = simulate(1000.0, 60);
        assert_eq!(over, 0, "overflow drops with +1000 ppm input");
        assert_eq!(under, 0, "underruns with +1000 ppm input");
        assert!(dev < 0.5, "fill deviated {dev:.3} of target after settling");
        assert!((ratio - 1.001).abs() < 0.0003, "ratio {ratio} did not track +1000 ppm");
    }

    #[test]
    fn drift_slow_input_stays_stable() {
        let (dev, under, over, ratio) = simulate(-1000.0, 60);
        assert_eq!(over, 0);
        assert_eq!(under, 0, "underruns with -1000 ppm input");
        assert!(dev < 0.5, "fill deviated {dev:.3} of target after settling");
        assert!((ratio - 0.999).abs() < 0.0003, "ratio {ratio} did not track -1000 ppm");
    }

    #[test]
    fn no_drift_passes_samples_through() {
        // Fill exactly at target, clocks identical: ratio must stay 1.0 and the
        // output must be the input verbatim (one-sample anchor delay at start).
        let block = 256usize;
        let ring_frames = block * 4;
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(ring_frames);
        let mut drift = Drift::new(block, 1, ring_frames);
        let mut out = vec![0.0f32; block];
        let mut next = 0.0f32;
        // Prefill so the fill AT READ TIME equals target (push one block, then
        // read one block): the servo must see zero error and keep ratio 1.0.
        for _ in 0..ring_frames / 2 - block {
            prod.push(next).unwrap();
            next += 1.0;
        }
        // Output = input delayed by one anchor sample: first-ever sample is the
        // zero anchor, then every input sample appears exactly once, in order.
        let mut expect = 0.0f32;
        for b in 0..200 {
            for _ in 0..block {
                prod.push(next).unwrap();
                next += 1.0;
            }
            assert_eq!(drift.read(&mut cons, &mut out, block), 0);
            assert_eq!(drift.ratio(), 1.0, "block {b}: servo moved without drift");
            for k in 0..block {
                if b == 0 && k == 0 {
                    assert_eq!(out[k], 0.0);
                } else {
                    assert_eq!(out[k], expect, "block {b} sample {k}");
                    expect += 1.0;
                }
            }
        }
    }

    #[test]
    fn starvation_zero_fills_and_recovers() {
        let block = 64usize;
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(block * 4);
        let mut drift = Drift::new(block, 1, block * 4);
        let mut out = vec![1.0f32; block];
        // Empty ring: whole block underruns, output zeroed, no panic.
        assert_eq!(drift.read(&mut cons, &mut out, block), block);
        assert!(out.iter().all(|&s| s == 0.0));
        // Refill and confirm normal processing resumes (first sample is the
        // zero anchor after the starvation reset, then verbatim 0.5).
        for _ in 0..block * 2 {
            prod.push(0.5).unwrap();
        }
        assert_eq!(drift.read(&mut cons, &mut out, block), 0);
        assert_eq!(out[0], 0.0);
        assert!(out[1..].iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }
}