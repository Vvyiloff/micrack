//! Regression: VST3 must process exactly the frames the host supplies.
//!
//! vst3-host derives `ProcessData::numSamples` from the buffer Vec LENGTH, not
//! from `AudioBuffers::block_size`. When the Vecs stayed at the configured max
//! while only `n` frames were filled, the plugin chewed a stale tail from the
//! previous call and corrupted its state -> crackle. WASAPI delivers variable
//! period sizes, so this fired on every callback in production.
//!
//! A fixed block equal to the configured max is a valid reference (n == capacity,
//! so even the buggy code was correct there). Every other size must agree with it.
#![cfg(windows)]

use micrack_core::engine::block::Block;
use micrack_core::plugins::{self, PluginFormat, PluginInstance};

const IVGI3: &str = r"C:\Program Files\Common Files\VST3\IVGI2.vst3";
const DEESS3: &str = r"C:\Program Files\Common Files\VST3\T-De-Esser 2.vst3";
const IVGI2: &str = r"C:\Program Files\Steinberg\VSTPlugins\IVGI2.dll";
const RATE: u32 = 48_000;
const MAX: usize = 512;

/// 1 kHz tone gated 20 ms on / 20 ms off: sharp transients make a stale tail
/// obvious, where a steady sine would hide it (the tail is also a sine).
fn gated(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            let on = ((t * 25.0) as i32) % 2 == 0;
            if on { 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin() } else { 0.0 }
        })
        .collect()
}

/// Render `src` through one plugin, calling process with the given block sizes
/// in rotation. Returns the concatenated left channel.
fn render(path: &str, fmt: PluginFormat, src: &[f32], sizes: &[usize]) -> Vec<f32> {
    let mut p = plugins::load(path, fmt, RATE, MAX).expect("load plugin");
    let mut b = Block::new(MAX);
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    let mut k = 0;
    while i < src.len() {
        let n = sizes[k % sizes.len()].min(MAX).min(src.len() - i);
        k += 1;
        b.set_len(n);
        b.left[..n].copy_from_slice(&src[i..i + n]);
        b.right[..n].copy_from_slice(&src[i..i + n]);
        p.process(&mut b).expect("process");
        out.extend_from_slice(&b.left[..n]);
        i += n;
    }
    out
}

fn rel_diff_db(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let sig: f32 = a[..n].iter().map(|x| x * x).sum::<f32>() / n as f32;
    let err: f32 = a[..n].iter().zip(&b[..n]).map(|(x, y)| (x - y) * (x - y)).sum::<f32>() / n as f32;
    10.0 * ((err + 1e-20) / (sig + 1e-20)).log10()
}

fn max_jump(a: &[f32]) -> f32 {
    a.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

/// Every block size, fixed or varying, must match the full-block reference.
fn assert_block_size_invariant(path: &str, fmt: PluginFormat, label: &str) {
    if !std::path::Path::new(path).exists() {
        eprintln!("skip {label}: plugin not installed");
        return;
    }
    let src = gated(RATE as usize * 2);
    let reference = render(path, fmt, &src, &[MAX]);
    for sizes in [
        vec![128usize],
        vec![256],
        vec![480],
        vec![1024], // clamped to MAX
        vec![480, 512, 256, 128, 384],
    ] {
        let got = render(path, fmt, &src, &sizes);
        let d = rel_diff_db(&reference, &got);
        assert!(
            d < -60.0,
            "{label}: block sizes {sizes:?} diverge from the {MAX}-frame reference by {d:.1} dB \
             (stale-tail regression); maxjump ref={:.4} got={:.4}",
            max_jump(&reference),
            max_jump(&got)
        );
    }
}

#[test]
fn vst3_block_size_invariance() {
    assert_block_size_invariant(IVGI3, PluginFormat::Vst3, "IVGI2 VST3");
    assert_block_size_invariant(DEESS3, PluginFormat::Vst3, "T-De-Esser 2 VST3");
}

/// VST2 must keep working exactly as before.
#[test]
fn vst2_block_size_invariance() {
    assert_block_size_invariant(IVGI2, PluginFormat::Vst2, "IVGI2 VST2");
}

/// Digital silence in must stay silence out — a stale tail would resurrect audio.
#[test]
fn vst3_silence_stays_silent() {
    for path in [IVGI3, DEESS3] {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        let src = vec![0.0f32; RATE as usize];
        let out = render(path, PluginFormat::Vst3, &src, &[480, 512, 256, 128]);
        let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak < 1e-4, "{path}: silence produced peak {peak}");
        assert!(out.iter().all(|s| s.is_finite()), "{path}: non-finite output");
    }
}

/// A mono VST3 must not leave the right channel dead: the host duplicates it.
#[test]
fn vst3_mono_fills_both_channels() {
    if !std::path::Path::new(IVGI3).exists() {
        return;
    }
    let mut p = plugins::load(IVGI3, PluginFormat::Vst3, RATE, MAX).expect("load");
    let src = gated(4096);
    let mut b = Block::new(MAX);
    let mut i = 0;
    while i < src.len() {
        let n = 480.min(src.len() - i);
        b.set_len(n);
        b.left[..n].copy_from_slice(&src[i..i + n]);
        b.right[..n].copy_from_slice(&src[i..i + n]);
        p.process(&mut b).expect("process");
        assert_eq!(b.left[..n], b.right[..n], "mono plugin left the channels unequal");
        i += n;
    }
}

/// Instance must survive a mid-stream reconfigure (engine restart / buffer change).
#[test]
fn vst3_reconfigure_midstream() {
    if !std::path::Path::new(IVGI3).exists() {
        return;
    }
    let mut p: PluginInstance = plugins::load(IVGI3, PluginFormat::Vst3, RATE, MAX).expect("load");
    let src = gated(8192);
    let mut b = Block::new(MAX);
    for chunk in src.chunks(480) {
        b.set_len(chunk.len());
        b.left[..chunk.len()].copy_from_slice(chunk);
        b.right[..chunk.len()].copy_from_slice(chunk);
        p.process(&mut b).expect("process before reconfigure");
    }
    p.reconfigure(RATE, 256).expect("reconfigure");
    let mut b2 = Block::new(256);
    for chunk in src.chunks(200) {
        b2.set_len(chunk.len());
        b2.left[..chunk.len()].copy_from_slice(chunk);
        b2.right[..chunk.len()].copy_from_slice(chunk);
        p.process(&mut b2).expect("process after reconfigure");
        assert!(b2.left[..chunk.len()].iter().all(|s| s.is_finite()));
    }
}