//! ASIO host verification. Run with --test-threads=1 (ASIO allows one driver
//! per process; env vars are process-global). Uses Voicemeeter virtual ASIO
//! devices only — never a live mic/speaker path.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

mod common;

/// Prefer the Insert driver; fall back to the plain Virtual ASIO (both are
/// virtual — no acoustic feedback). Returns (device, name).
fn voicemeeter_device(host: &cpal::Host) -> (cpal::Device, String) {
    let devs: Vec<_> = host.output_devices().unwrap().collect();
    for want in ["Voicemeeter Insert Virtual ASIO", "Voicemeeter Virtual ASIO"] {
        if let Some(d) = devs.iter().find(|d| d.description().map(|x| x.name() == want).unwrap_or(false)) {
            return (d.clone(), want.into());
        }
    }
    panic!("no Voicemeeter virtual ASIO device found");
}

#[test]
fn asio_enumeration() {
    assert!(cpal::available_hosts().contains(&cpal::HostId::Asio), "ASIO host not compiled in");
    let host = cpal::host_from_id(cpal::HostId::Asio).expect("ASIO host");
    let mut names = Vec::new();
    for dir in ["input", "output"] {
        let devs: Vec<_> = if dir == "input" {
            host.input_devices().unwrap().collect()
        } else {
            host.output_devices().unwrap().collect()
        };
        for d in devs {
            let name = d.description().map(|x| x.name().to_string()).unwrap_or_default();
            let cfg = if dir == "input" { d.default_input_config() } else { d.default_output_config() };
            println!("ASIO {dir}: {name} -> {cfg:?}");
            names.push(name);
        }
    }
    assert!(names.iter().any(|n| n.contains("Voicemeeter")), "no Voicemeeter ASIO driver found: {names:?}");
}

/// Duplex on ONE ASIO device: input + output streams share the driver and fire
/// per bufferSwitch (one clock domain). Passthrough via rtrb for ~2 s.
#[test]
fn asio_duplex_passthrough() {
    let host = cpal::host_from_id(cpal::HostId::Asio).expect("ASIO host");
    let (_, name) = voicemeeter_device(&host);
    let find = |dir_in: bool| -> cpal::Device {
        let devs: Vec<_> = if dir_in { host.input_devices().unwrap().collect() } else { host.output_devices().unwrap().collect() };
        devs.into_iter().find(|d| d.description().map(|x| x.name() == name).unwrap_or(false)).expect("device in direction")
    };
    let in_dev = find(true);
    let out_dev = find(false);
    let in_def = in_dev.default_input_config().unwrap();
    let out_def = out_dev.default_output_config().unwrap();
    assert_eq!(in_def.sample_format(), cpal::SampleFormat::F32);
    assert_eq!(out_def.sample_format(), cpal::SampleFormat::F32);
    let ch = in_def.channels() as usize;
    assert_eq!(ch, out_def.channels() as usize);

    // Requested 48000/256; Voicemeeter fixes the buffer at 512, so fall back to
    // the driver default when Fixed(256) is rejected.
    let pick = |def: &cpal::SupportedStreamConfig| -> cpal::StreamConfig {
        let fixed = cpal::StreamConfig { channels: def.channels(), sample_rate: 48000, buffer_size: cpal::BufferSize::Fixed(256) };
        if let cpal::SupportedBufferSize::Range { min, max } = def.buffer_size() {
            if 256 < *min || 256 > *max {
                return cpal::StreamConfig { channels: def.channels(), sample_rate: 48000, buffer_size: cpal::BufferSize::Default };
            }
        }
        fixed
    };
    let in_sc = pick(&in_def);
    let out_sc = pick(&out_def);
    println!("duplex on {name}: in={in_sc:?} out={out_sc:?}");

    let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(48000 * 2);
    let in_calls = std::sync::Arc::new(AtomicU64::new(0));
    let out_calls = std::sync::Arc::new(AtomicU64::new(0));
    let ic = in_calls.clone();
    let oc = out_calls.clone();
    let err = |e| eprintln!("ASIO stream error: {e}");
    let in_stream = in_dev
        .build_input_stream(in_sc, move |data: &[f32], _| {
            ic.fetch_add(1, Relaxed);
            for &s in data {
                let _ = prod.push(s); // drop on full: passthrough test only
            }
        }, err, None)
        .expect("build ASIO input stream");
    let out_stream = out_dev
        .build_output_stream(out_sc, move |data: &mut [f32], _| {
            oc.fetch_add(1, Relaxed);
            for s in data.iter_mut() {
                *s = cons.pop().unwrap_or(0.0);
            }
        }, err, None)
        .expect("build ASIO output stream");
    in_stream.play().unwrap();
    out_stream.play().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    drop(in_stream);
    drop(out_stream);
    let (i, o) = (in_calls.load(Relaxed), out_calls.load(Relaxed));
    println!("duplex callbacks in 2s: input={i} output={o}");
    // 512-frame buffers at 48 kHz -> ~187 callbacks; require a healthy minimum.
    assert!(i > 50, "input callback fired only {i} times");
    assert!(o > 50, "output callback fired only {o} times");
}

/// Engine start_inner with ASIO on both sides of the same virtual device.
#[test]
fn engine_start_asio() {
    common::isolate_appdata();
    let eng = micrack_core::api::EngineHandle::new();
    let asio = eng.hosts().into_iter().find(|h| h.id == micrack_core::api::HostId::Asio).unwrap();
    assert!(asio.available, "ASIO host not available");
    let host = cpal::host_from_id(cpal::HostId::Asio).unwrap();
    let (_, name) = voicemeeter_device(&host);
    let sel = || micrack_core::api::DeviceSel { host: micrack_core::api::HostId::Asio, device: name.clone() };
    let cfg = micrack_core::api::EngineConfig { input: sel(), output: sel(), sample_rate: 48000, block_size: 256 };
    let status = eng.start(cfg).expect("engine start on ASIO");
    assert!(status.running);
    assert_eq!(status.sample_rate, 48000);
    println!("engine on {name}: {status:?}");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let tel = eng.telemetry();
    assert!(tel.callback_max_ns > 0, "audio callback never ran");
    println!("underruns={} overflows={} callback_max_ns={}", tel.underruns, tel.overflows, tel.callback_max_ns);
    eng.stop();
    assert!(!eng.status().running);
}

/// The GUI device dropdowns must not go blank while an ASIO stream is live.
/// asio-sys keeps ONE loaded driver per process, so cpal cannot reload the other
/// drivers to enumerate them; at minimum the running device must stay listed.
#[test]
fn asio_devices_listed_while_streaming() {
    common::isolate_appdata();
    use micrack_core::api::{DeviceSel, Direction, EngineConfig, HostId};
    let eng = micrack_core::api::EngineHandle::new();
    let host = cpal::host_from_id(cpal::HostId::Asio).unwrap();
    let (_, name) = voicemeeter_device(&host);
    let names = |v: Vec<micrack_core::api::DeviceInfo>| -> Vec<String> { v.into_iter().map(|d| d.name).collect() };
    println!("idle ASIO inputs: {:?}", names(eng.devices(HostId::Asio, Direction::Input)));
    let sel = || DeviceSel { host: HostId::Asio, device: name.clone() };
    let cfg = EngineConfig { input: sel(), output: sel(), sample_rate: 48000, block_size: 256 };
    eng.start(cfg).expect("engine start on ASIO");
    let ins = names(eng.devices(HostId::Asio, Direction::Input));
    let outs = names(eng.devices(HostId::Asio, Direction::Output));
    println!("streaming ASIO inputs: {ins:?} outputs: {outs:?} (running on {name})");
    let marked: Vec<String> = eng.devices(HostId::Asio, Direction::Input).into_iter().filter(|d| d.default).map(|d| d.name).collect();
    println!("default-marked ASIO inputs: {marked:?}");
    eng.stop();
    assert!(ins.iter().any(|n| *n == name), "running ASIO input device vanished from the list: {ins:?}");
    assert!(outs.iter().any(|n| *n == name), "running ASIO output device vanished from the list: {outs:?}");
    assert_eq!(marked, vec![name.clone()], "the configured ASIO device must be the marked default, not whichever enumerates first");
}