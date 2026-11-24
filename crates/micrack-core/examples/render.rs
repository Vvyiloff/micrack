//! Offline chain renderer + live parameter probe. Lets the voice chain be tuned
//! against a recorded sample instead of a live speaker: renders a WAV through
//! the exact plugins/params the engine uses, and prints each plugin's LIVE
//! parameter displays (the running app's HTTP cache shows stale ones).
//!
//! usage:
//!   render probe <plugin-path> [id=value ...]
//!   render chain <spec.json> <in.wav> <out.wav>

use micrack_core::engine::block::{Block, db_to_gain};
use micrack_core::plugins::{self, PluginInstance};
use serde::Deserialize;
use std::io::Write;

#[derive(Deserialize)]
struct Spec {
    #[serde(default = "d_rate")]
    rate: u32,
    #[serde(default = "d_block")]
    block: usize,
    #[serde(default)]
    input_gain_db: f32,
    #[serde(default)]
    output_gain_db: f32,
    #[serde(default = "d_true")]
    center: bool,
    #[serde(default = "d_true")]
    limit: bool,
    /// Emulate a real device: vary the per-callback block size instead of a fixed one.
    #[serde(default)]
    vary: bool,
    chain: Vec<Link>,
}
#[derive(Deserialize)]
struct Link {
    path: String,
    #[serde(default)]
    bypass: bool,
    #[serde(default)]
    params: Vec<(u32, f64)>,
}
fn d_rate() -> u32 { 48000 }
fn d_block() -> usize { 512 }
fn d_true() -> bool { true }

fn show(p: &PluginInstance) {
    let i = p.info();
    println!("== {} [in={} out={}] ==", i.name, i.audio_inputs, i.audio_outputs);
    if let Ok(ps) = p.list_parameters() {
        for q in ps {
            println!("   {:>10}  {:<22} v={:.5}  {}", q.id, q.name, q.value, q.display);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(|s| s.as_str()) {
        Some("probe") => {
            let path = &a[2];
            let mut p = plugins::load(path, plugins::format_from_path(path), 48000, 512)?;
            for kv in a.iter().skip(3) {
                let (i, v) = kv.split_once('=').unwrap();
                p.set_parameter(i.parse()?, v.parse()?)?;
            }
            show(&p);
        }
        Some("chain") => {
            let spec: Spec = serde_json::from_str(&std::fs::read_to_string(&a[2])?)?;
            let mut r = hound::WavReader::open(&a[3])?;
            let sp = r.spec();
            let raw: Vec<f32> = match sp.sample_format {
                hound::SampleFormat::Float => r.samples::<f32>().map(|x| x.unwrap()).collect(),
                hound::SampleFormat::Int => {
                    let sc = (1i64 << (sp.bits_per_sample - 1)) as f32;
                    r.samples::<i32>().map(|x| x.unwrap() as f32 / sc).collect()
                }
            };
            let ch = sp.channels as usize;
            let mono: Vec<f32> = raw.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32).collect();

            let mut insts: Vec<(PluginInstance, bool)> = Vec::new();
            for l in &spec.chain {
                let mut p = plugins::load(&l.path, plugins::format_from_path(&l.path), spec.rate, spec.block)?;
                for (id, v) in &l.params {
                    p.set_parameter(*id, *v)?;
                }
                show(&p);
                insts.push((p, l.bypass));
            }

            let gi = db_to_gain(spec.input_gain_db);
            let go = db_to_gain(spec.output_gain_db);
            let mut b = Block::new(spec.block);
            let out_spec = hound::WavSpec { channels: 2, sample_rate: spec.rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
            let mut w = hound::WavWriter::create(&a[4], out_spec)?;
            let mut i = 0;
            let mut call = 0usize;
            let sizes = [480usize, 512, 256, 128, 480, 512, 384, 256];
            while i < mono.len() {
                let want = if spec.vary { sizes[call % sizes.len()].min(spec.block) } else { spec.block };
                let n = want.min(mono.len() - i);
                call += 1;
                b.set_len(n);
                for k in 0..n {
                    b.left[k] = mono[i + k] * gi;
                    b.right[k] = mono[i + k] * gi;
                }
                for (p, bypass) in insts.iter_mut() {
                    if !*bypass {
                        p.process(&mut b)?;
                    }
                }
                b.apply_gain(go);
                if spec.limit { b.soft_limit(); }
                if spec.center { b.right[..n].copy_from_slice(&b.left[..n]); }
                for k in 0..n {
                    w.write_sample(b.left[k])?;
                    w.write_sample(b.right[k])?;
                }
                i += n;
            }
            w.finalize()?;
            println!("rendered {} -> {} ({} frames @ {} Hz)", a[3], a[4], mono.len(), spec.rate);
        }
        _ => {
            let _ = writeln!(std::io::stderr(), "usage: render probe <path> [id=v..] | render chain <spec.json> <in.wav> <out.wav>");
            std::process::exit(2);
        }
    }
    Ok(())
}