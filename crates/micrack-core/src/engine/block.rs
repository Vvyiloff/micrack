//! Preallocated stereo processing block. No allocation on the audio path.

#[derive(Clone)]
pub struct Block {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub len: usize,
    pub capacity: usize,
}

impl Block {
    pub fn new(cap: usize) -> Self {
        Self { left: vec![0.0; cap], right: vec![0.0; cap], len: 0, capacity: cap }
    }

    pub fn set_len(&mut self, n: usize) {
        self.len = n.min(self.capacity);
    }

    pub fn clear(&mut self) {
        self.left[..self.len].fill(0.0);
        self.right[..self.len].fill(0.0);
    }

    pub fn copy_from(&mut self, other: &Block) {
        let n = other.len.min(self.capacity);
        self.left[..n].copy_from_slice(&other.left[..n]);
        self.right[..n].copy_from_slice(&other.right[..n]);
        self.len = n;
    }

    pub fn apply_gain(&mut self, gain: f32) {
        for i in 0..self.len {
            self.left[i] *= gain;
            self.right[i] *= gain;
        }
    }

    /// Safety ceiling: unity below the knee, tanh knee above it, asymptote 0.95.
    /// Cheap (tanh only for hot samples) and allocation-free.
    pub fn soft_limit(&mut self) {
        const KNEE: f32 = 0.7;
        const WIDTH: f32 = 0.25; // KNEE + WIDTH = 0.95 ceiling
        for i in 0..self.len {
            self.left[i] = soft_clip(self.left[i], KNEE, WIDTH);
            self.right[i] = soft_clip(self.right[i], KNEE, WIDTH);
        }
    }

    pub fn peak(&self) -> f32 {
        let mut p = 0f32;
        for i in 0..self.len {
            p = p.max(self.left[i].abs()).max(self.right[i].abs());
        }
        p
    }

    /// Interleaved input -> stereo block. Mono is upmixed to both channels.
    pub fn fill_from_interleaved(&mut self, data: &[f32], in_ch: usize) {
        if in_ch == 0 {
            self.len = 0;
            return;
        }
        let n = (data.len() / in_ch).min(self.capacity);
        if in_ch == 1 {
            for i in 0..n {
                self.left[i] = data[i];
                self.right[i] = data[i];
            }
        } else {
            for i in 0..n {
                self.left[i] = data[i * in_ch];
                self.right[i] = data[i * in_ch + 1];
            }
        }
        self.len = n;
    }

    /// Stereo block -> interleaved output. Mono downmixes (L+R)/2; extra channels silent.
    pub fn write_to_interleaved(&self, data: &mut [f32], out_ch: usize) {
        if out_ch == 0 {
            return;
        }
        let frames = data.len() / out_ch;
        for i in 0..frames {
            let (l, r) = if i < self.len { (self.left[i], self.right[i]) } else { (0.0, 0.0) };
            if out_ch == 1 {
                data[i] = (l + r) * 0.5;
            } else {
                data[i * out_ch] = l;
                data[i * out_ch + 1] = r;
                for c in 2..out_ch {
                    data[i * out_ch + c] = 0.0;
                }
            }
        }
    }
}

fn soft_clip(x: f32, knee: f32, width: f32) -> f32 {
    let a = x.abs();
    if a <= knee { x } else { x.signum() * (knee + width * ((a - knee) / width).tanh()) }
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_upmix_stereo_downmix() {
        let mut b = Block::new(4);
        b.fill_from_interleaved(&[0.5, -0.5, 0.25, 0.0], 1);
        assert_eq!(b.len, 4);
        assert_eq!(b.left, b.right);
        let mut out = [0f32; 4];
        b.write_to_interleaved(&mut out, 1);
        assert_eq!(out, [0.5, -0.5, 0.25, 0.0]);
        let mut st = [0f32; 8];
        b.write_to_interleaved(&mut st, 2);
        assert_eq!(st[0], 0.5);
        assert_eq!(st[1], 0.5);
    }

    #[test]
    fn soft_limit_bounds_blasts() {
        let mut b = Block::new(4);
        b.fill_from_interleaved(&[0.5, -0.5, 20.0, -20.0], 1);
        b.soft_limit();
        assert_eq!(b.left[0], 0.5, "below the knee stays untouched");
        assert_eq!(b.left[1], -0.5);
        assert!(b.peak() <= 0.95, "peak {} exceeds ceiling", b.peak());
        assert!(b.left[2] > 0.7 && b.left[3] < -0.7, "hot samples still pass, sign kept");
    }

    #[test]
    fn gain_and_peak() {
        let mut b = Block::new(2);
        b.fill_from_interleaved(&[1.0, -0.5], 1);
        b.apply_gain(db_to_gain(-6.0206)); // ~0.5
        assert!((b.peak() - 0.5).abs() < 0.001);
    }
}