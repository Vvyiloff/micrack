//! Agent-control verification: parameters by stable slot id, live recording,
//! voice analysis and the full-auto gain guardrails.
//! Running-engine tests use the default (virtual Voicemeeter) devices only.

use micrack_core::EngineHandle;
use micrack_core::api::{DeviceSel, EngineConfig, HostId, RecordTap};
use micrack_core::plugins::{self, PluginFormat};

mod common;

const VST3: &str = r"C:\Program Files\Common Files\VST3\IVGI2.vst3";
const VST2: &str = r"C:\Program Files\VSTPlugins\T-De-Esser 2.dll";

fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
    for _ in 0..300 {
        if f() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timeout waiting for: {what}");
}

/// (a) list/set/get round-trip on a VST3 and a VST2 slot, addressed by stable id.
#[test]
fn parameters_roundtrip_offline() {
    let _appdata = common::isolate_appdata();
    let h = EngineHandle::new();
    h.add(VST3.into(), 0).expect("add IVGI2");
    h.add(VST2.into(), 1).expect("add T-De-Esser");
    let chain = h.telemetry().chain;
    assert_eq!(chain.len(), 2);

    for slot in &chain {
        let params = h.list_parameters(slot.id).expect("list_parameters");
        assert!(!params.is_empty(), "{} exposes no parameters", slot.name);
        eprintln!("{}: {} params, first = {:?}", slot.name, params.len(), params[0]);

        // Find a parameter that tracks a normalized write (skip stepped/read-only ones).
        let mut tracked = None;
        for p in &params {
            if h.set_parameter(slot.id, p.id, 0.42).is_err() {
                continue;
            }
            let v = h.get_parameter(slot.id, p.id).expect("get_parameter");
            assert!((0.0..=1.0).contains(&v), "{} param {} out of range: {v}", slot.name, p.id);
            if (v - 0.42).abs() < 0.02 {
                tracked = Some((p.id, p.name.clone()));
                break;
            }
        }
        let (id, name) = tracked.unwrap_or_else(|| panic!("{}: no parameter tracked a 0.42 write", slot.name));

        h.set_parameter(slot.id, id, 0.75).expect("set 0.75");
        let v = h.get_parameter(slot.id, id).expect("get 0.75");
        assert!((v - 0.75).abs() < 0.02, "{} {name}: wrote 0.75, read {v}", slot.name);

        // Out-of-range writes are clamped, never rejected or passed through.
        h.set_parameter(slot.id, id, 5.0).expect("set 5.0 clamps");
        let hi = h.get_parameter(slot.id, id).expect("get clamped high");
        assert!(hi <= 1.0 && hi > 0.9, "{} {name}: clamp high gave {hi}", slot.name);
        h.set_parameter(slot.id, id, -3.0).expect("set -3.0 clamps");
        let lo = h.get_parameter(slot.id, id).expect("get clamped low");
        assert!((0.0..0.1).contains(&lo), "{} {name}: clamp low gave {lo}", slot.name);
    }

    // Unknown slot ids and parameter ids are errors, not panics.
    assert!(h.list_parameters(999_999).is_err());
    assert!(h.set_parameter(chain[1].id, 9999, 0.5).is_err());
}

/// (d) set_gains clamps to ±24 dB before it reaches the audio thread.
#[test]
fn gain_clamp_guardrail() {
    let _appdata = common::isolate_appdata();
    let h = EngineHandle::new();
    h.set_gains(60.0, -60.0);
    wait_until(|| (h.gains_db().0 - 24.0).abs() < 0.01, "input gain clamp");
    let (i, m) = h.gains_db();
    assert!((i - 24.0).abs() < 0.01, "input gain {i} != +24");
    assert!((m + 24.0).abs() < 0.01, "master gain {m} != -24");
    h.set_gains(3.0, -3.0);
    wait_until(|| (h.gains_db().0 - 3.0).abs() < 0.01, "in-range gains pass through");
}

/// (c) analyze_voice on a synthetic sine + sibilance bursts.
#[test]
fn analyze_synthetic_voice() {
    let _appdata = common::isolate_appdata();
    let dir = std::env::temp_dir().join("micrack-agent-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("synthetic-voice.wav");
    let rate = 48000u32;
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    let n = rate as usize * 3;
    for i in 0..n {
        let t = i as f32 / rate as f32;
        // voiced fundamental + harmonics, with a 6.5 kHz sibilant burst every 500 ms
        let mut s = 0.25 * (2.0 * std::f32::consts::PI * 180.0 * t).sin()
            + 0.08 * (2.0 * std::f32::consts::PI * 360.0 * t).sin();
        if (i % (rate as usize / 2)) < (rate as usize / 20) {
            s += 0.5 * (2.0 * std::f32::consts::PI * 6500.0 * t).sin();
        }
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();

    let h = EngineHandle::new();
    let a = h.analyze_voice(&path).expect("analyze_voice");
    eprintln!("analysis: {}", serde_json::to_string(&a).unwrap());
    assert!(a.lufs_integrated.is_finite() && a.lufs_integrated > -70.0 && a.lufs_integrated < 0.0, "lufs {}", a.lufs_integrated);
    assert!(a.lra.is_finite() && a.lra >= 0.0);
    assert!(a.true_peak > 0.0 && a.true_peak < 2.0);
    assert!(a.sibilance_ratio > 0.0 && a.sibilance_ratio <= 1.0, "sibilance {}", a.sibilance_ratio);
    assert!((5000.0..=9000.0).contains(&a.sibilance_freq_hz), "sibilance freq {}", a.sibilance_freq_hz);
    assert!(a.spectral_centroid_hz > 0.0 && a.spectral_tilt_db_per_oct.is_finite());
    assert!(a.crest_factor >= 1.0);
    assert!(!a.recommendations.is_empty(), "expected at least a makeup-gain hint");
    assert!(h.analyze_voice(&dir.join("nope.wav")).is_err());
}

/// (b) record_sample while the engine streams on the default virtual devices.
#[test]
fn record_while_running() {
    let _appdata = common::isolate_appdata();
    let h = EngineHandle::new();
    let cfg = EngineConfig {
        input: DeviceSel { host: HostId::Wasapi, device: "default".into() },
        output: DeviceSel { host: HostId::Wasapi, device: "default".into() },
        sample_rate: 48000,
        block_size: 256,
    };
    if h.start(cfg).is_err() {
        eprintln!("skip record_while_running: default devices unavailable");
        return;
    }
    let read = |p: &std::path::Path| {
        let mut r = hound::WavReader::open(p).expect("open recorded wav");
        let spec = r.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        let s: Vec<f32> = r.samples::<f32>().map(|x| x.unwrap()).collect();
        (spec.sample_rate, s)
    };

    let p_in = h.record_sample(1.5, RecordTap::Input).expect("record dry input");
    assert!(p_in.exists(), "{} missing", p_in.display());
    let (rate, samples) = read(&p_in);
    let peak = samples.iter().fold(0f32, |p, s| p.max(s.abs()));
    eprintln!("input tap: {} samples @ {rate} Hz, peak {peak:.6}", samples.len());
    assert!(samples.len() > rate as usize, "too few frames: {}", samples.len());
    assert!(peak > 1e-4, "recorded input WAV is silent (peak {peak})");

    let p_out = h.record_sample(1.0, RecordTap::Output).expect("record processed output");
    let (_, out) = read(&p_out);
    let out_peak = out.iter().fold(0f32, |p, s| p.max(s.abs()));
    eprintln!("output tap: {} samples, peak {out_peak:.6}", out.len());
    assert!(!out.is_empty());
    assert!(out_peak <= 0.95, "output ceiling breached: {out_peak}");

    // Analysis of the real recording must work end to end.
    let a = h.analyze_voice(&p_in).expect("analyze recording");
    assert!(a.lufs_integrated.is_finite());

    h.stop();
    let err = h.record_sample(0.5, RecordTap::Input).expect_err("must refuse when stopped");
    assert!(err.to_string().contains("not running"), "unclear error: {err}");
}

/// list_parameters is served from a load-time cache (no instance lock, no audio
/// stall): it must match a direct enumeration, and set_parameter must refresh
/// the cached value without re-enumerating.
#[test]
fn parameter_cache_matches_direct() {
    let _appdata = common::isolate_appdata();
    let direct = plugins::load(VST3, PluginFormat::Vst3, 48000, 256).expect("load IVGI2 directly");
    let expected = direct.list_parameters().expect("direct enumeration");
    drop(direct);
    assert!(!expected.is_empty());

    let h = EngineHandle::new();
    h.add(VST3.into(), 0).expect("add IVGI2");
    let slot = h.telemetry().chain[0].id;
    let cached = h.list_parameters(slot).expect("cached list");
    assert_eq!(cached.len(), expected.len());
    for (c, e) in cached.iter().zip(&expected) {
        assert_eq!((c.id, &c.name, c.min, c.max, &c.unit), (e.id, &e.name, e.min, e.max, &e.unit));
        assert!((c.value - e.value).abs() < 1e-9, "param {} value {} != {}", e.id, c.value, e.value);
    }

    // A tracked write shows up in the cached list.
    let tracked = expected
        .iter()
        .find_map(|p| {
            h.set_parameter(slot, p.id, 0.42).ok()?;
            let v = h.get_parameter(slot, p.id).ok()?;
            ((v - 0.42).abs() < 0.02).then_some(p.id)
        })
        .expect("a parameter tracks a 0.42 write");
    let after = h.list_parameters(slot).expect("cached list after set");
    let p = after.iter().find(|p| p.id == tracked).unwrap();
    assert!((p.value - 0.42).abs() < 0.02, "cached value not refreshed: {}", p.value);
}