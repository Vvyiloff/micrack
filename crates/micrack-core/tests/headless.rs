//! Headless MVP verification: real plugins, offline block processing only.
//! NO live mic->speaker streaming (feedback risk).

use micrack_core::EngineHandle;
use micrack_core::engine::block::Block;
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

#[test]
fn headless_mvp() {
    let _appdata = common::isolate_appdata();
    // ---- 1. Offline chain: IVGI2 (VST3 mono) -> T-De-Esser 2 (VST2 stereo) ----
    let mut ivgi = plugins::load(VST3, PluginFormat::Vst3, 48000, 256).expect("load IVGI2");
    let mut tde = plugins::load(VST2, PluginFormat::Vst2, 48000, 256).expect("load T-De-Esser 2");
    assert_eq!(ivgi.info().audio_inputs, 1, "IVGI2 is mono in");
    assert_eq!(ivgi.info().audio_outputs, 1, "IVGI2 is mono out");
    assert_eq!(tde.info().audio_inputs, 2, "T-De-Esser is stereo in");
    assert_eq!(tde.info().audio_outputs, 2, "T-De-Esser is stereo out");

    let mut b = Block::new(256);
    b.set_len(256);
    let mut phase = 0f32;
    let mut peak = 0f32;
    for _ in 0..200 {
        for i in 0..b.len {
            phase += 0.05;
            let s = phase.sin() * 0.5;
            b.left[i] = s;
            b.right[i] = s;
        }
        ivgi.process(&mut b).expect("IVGI2 process");
        tde.process(&mut b).expect("T-De-Esser process");
        peak = peak.max(b.peak());
    }
    assert!(peak > 0.01, "chain output silent (peak={peak})");
    eprintln!("offline chain: 200 blocks, peak={peak:.4}");

    // Per-instance state round-trip.
    let s3 = ivgi.save_state().expect("IVGI2 save_state");
    assert!(!s3.is_empty());
    ivgi.load_state(&s3).expect("IVGI2 load_state");
    let s2 = tde.save_state().expect("T-De-Esser save_state");
    assert!(!s2.is_empty());
    tde.load_state(&s2).expect("T-De-Esser load_state");
    drop(ivgi);
    drop(tde);

    // ---- 2. EngineHandle ops WITHOUT cpal streams ----
    let h = EngineHandle::new();
    assert!(!h.status().running);
    h.add(VST3.into(), 0).expect("engine add IVGI2");
    h.add(VST2.into(), 1).expect("engine add T-De-Esser");
    let t = h.telemetry();
    assert_eq!(t.chain.len(), 2);
    assert_eq!(t.chain[0].format, PluginFormat::Vst3);
    assert_eq!(t.chain[1].format, PluginFormat::Vst2);

    h.move_slot(0, 1);
    wait_until(|| h.telemetry().chain[0].format == PluginFormat::Vst2, "move_slot");
    h.set_bypass(0, true);
    wait_until(|| h.telemetry().chain[0].bypass, "set_bypass");
    h.set_gains(-3.0, -1.0);

    // ---- 3. Preset save -> load round-trip (state blobs must match) ----
    let dir = std::env::temp_dir().join("micrack-headless-test");
    std::fs::create_dir_all(&dir).unwrap();
    let p1 = dir.join("preset-a.json");
    let p2 = dir.join("preset-b.json");
    h.save_preset(&p1).expect("save_preset");

    h.remove(0);
    h.remove(0);
    wait_until(|| h.telemetry().chain.is_empty(), "remove all");

    let report = h.load_preset(&p1).expect("load_preset");
    assert_eq!(report.loaded.len(), 2, "both plugins restored");
    assert!(report.missing.is_empty());
    assert!(h.telemetry().chain[0].bypass, "bypass restored");
    h.save_preset(&p2).expect("save_preset 2");

    let j1: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p1).unwrap()).unwrap();
    let j2: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p2).unwrap()).unwrap();
    assert_eq!(j1["version"], 1);
    for i in 0..2 {
        assert_eq!(j1["chain"][i]["state_b64"], j2["chain"][i]["state_b64"], "state blob {i} round-trip");
        assert_eq!(j1["chain"][i]["path"], j2["chain"][i]["path"]);
        assert_eq!(j1["chain"][i]["bypass"], j2["chain"][i]["bypass"]);
    }

    // ---- 4. Telemetry shape ----
    let t = h.telemetry();
    assert_eq!(t.chain.len(), 2);
    assert_eq!(t.slot_peaks.len(), 2);
    assert_eq!(t.underruns, 0);
    assert_eq!(t.overflows, 0);
    let json = serde_json::to_string(&t).unwrap();
    assert!(json.len() < 1024, "telemetry {} bytes >= 1KB", json.len());
    eprintln!("telemetry: {} bytes, chain={:?}", json.len(), t.chain.iter().map(|s| &s.name).collect::<Vec<_>>());

    h.remove(0);
    h.remove(0);
    wait_until(|| h.telemetry().chain.is_empty(), "cleanup");
}

/// Regression: opening a plugin editor WHILE the engine streams must keep the
/// real instance processing in the audio chain (shared Arc<Mutex<..>>), not park
/// it. Asserts the engine stays running and the audio callback keeps executing
/// across the whole open window. Uses default (virtual Voicemeeter) devices — no
/// acoustic feedback.
#[test]
#[cfg(windows)]
fn editor_while_running() {
    let _appdata = common::isolate_appdata();
    use micrack_core::api::{DeviceSel, EngineConfig, HostId};
    use std::time::Duration;
    let h = EngineHandle::new();
    let cfg = EngineConfig {
        input: DeviceSel { host: HostId::Wasapi, device: "default".into() },
        output: DeviceSel { host: HostId::Wasapi, device: "default".into() },
        sample_rate: 48000,
        block_size: 256,
    };
    if h.start(cfg).is_err() {
        eprintln!("skip editor_while_running: default devices unavailable");
        return;
    }
    h.add(VST3.into(), 0).expect("add IVGI2");
    std::thread::sleep(Duration::from_millis(400));
    assert!(h.status().running, "engine must be running before open");

    h.open_editor(0).expect("open_editor while running");

    // Live editing: across ~1.5 s the engine must stay running and the audio
    // callback must keep firing (callback_max_ns > 0) with the editor open and the
    // slot still present — processing continues, the instance is never parked.
    let mut saw_callback = false;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(50));
        let t = h.telemetry();
        assert!(h.status().running, "engine stopped while editor open");
        assert_eq!(t.chain.len(), 1, "slot must stay in the chain while editing");
        assert!(t.chain[0].editor_open, "editor should be open");
        assert_eq!(t.slot_peaks.len(), 1, "slot meter must stay live");
        if t.callback_max_ns > 0 {
            saw_callback = true;
        }
    }
    assert!(saw_callback, "audio callback must keep executing while the editor is open");

    h.close_editor(0);
    std::thread::sleep(Duration::from_millis(400));
    assert!(h.status().running, "engine must still be running after close");
    assert!(!h.telemetry().chain[0].editor_open, "editor should be closed");
    h.stop();
    eprintln!("editor_while_running: processed continuously through open/close while streaming");
}

/// Picking a device that does not exist under the selected host (what the GUI
/// does when the host changes but the device name lags) must be refused WITHOUT
/// killing the stream that was already working.
#[test]
#[cfg(windows)]
fn refused_device_keeps_previous_stream_running() {
    let _appdata = common::isolate_appdata();
    use micrack_core::api::{DeviceSel, EngineConfig, HostId};
    let h = EngineHandle::new();
    let cfg = EngineConfig {
        input: DeviceSel { host: HostId::Wasapi, device: "default".into() },
        output: DeviceSel { host: HostId::Wasapi, device: "default".into() },
        sample_rate: 48000,
        block_size: 256,
    };
    if h.start(cfg.clone()).is_err() {
        eprintln!("skip refused_device: default devices unavailable");
        return;
    }
    assert!(h.status().running);
    let bad = EngineConfig {
        input: DeviceSel { host: HostId::Wasapi, device: "micrack-no-such-device".into() },
        ..cfg.clone()
    };
    assert!(h.start(bad).is_err(), "an unknown device must be refused");
    assert!(h.status().running, "a refused config must not stop the working stream");
    assert_eq!(h.config().expect("config").input.device, "default", "previous route must be restored");
    h.stop();
    assert!(!h.status().running);
}

/// M1 fail-safe: a corrupt last-session.json with a persisted last_config must
/// NOT auto-start the raw mic at the persisted (hot) gain — engine stays stopped
/// with an error notice instead.
#[test]
fn corrupt_session_fails_safe_no_autostart() {
    let appdata = common::isolate_appdata();
    let dir = appdata.join("micrack");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        serde_json::json!({
            "last_config": {
                "input": {"host": "wasapi", "device": "default"},
                "output": {"host": "wasapi", "device": "default"},
                "sample_rate": 48000, "block_size": 256
            },
            "input_gain_db": 10.0, "master_gain_db": 10.0, "extra_dirs": []
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("last-session.json"), b"{\"version\": truncated").unwrap();
    let h = EngineHandle::new();
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(!h.status().running, "corrupt session must not auto-start the raw mic");
    assert!(
        h.telemetry().notices.iter().any(|n| matches!(n, micrack_core::api::Notice::Error { .. })),
        "restore failure must surface a notice"
    );
}

/// The chain auto-persists to last-session.json on mutations and exit; a fresh
/// engine restores chain order, bypass and gains before the GUI connects. A
/// missing plugin restores as a bypassed placeholder, never a startup failure.
#[test]
fn last_session_roundtrip() {
    let appdata = common::isolate_appdata();
    let session = appdata.join("micrack").join("last-session.json");
    {
        let h = EngineHandle::new();
        h.add(VST3.into(), 0).expect("add IVGI2");
        h.add(VST2.into(), 1).expect("add T-De-Esser");
        h.set_bypass(1, true);
        h.set_gains(-3.0, -1.0);
        wait_until(|| h.telemetry().chain.len() == 2 && h.telemetry().chain[1].bypass, "chain ready");
        assert!(session.exists(), "mutations must auto-save last-session.json");
        // handle drop = app exit: orderly stop + final save
    }
    let j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&session).unwrap()).unwrap();
    assert_eq!(j["chain"].as_array().unwrap().len(), 2);
    assert_eq!(j["input"]["gain_db"], -3.0);
    assert_eq!(j["chain"][1]["bypass"], true);

    let h2 = EngineHandle::new();
    // Restore runs on the control thread inside Engine::new; telemetry is a
    // direct snapshot, so poll until the chain is back (GUI requests queue
    // behind the restore and are ordered automatically).
    wait_until(|| h2.telemetry().chain.len() == 2, "startup session restore");
    let t = h2.telemetry();
    assert_eq!(t.chain.len(), 2, "startup must restore the session chain");
    assert_eq!(t.chain[0].format, PluginFormat::Vst3);
    assert_eq!(t.chain[1].format, PluginFormat::Vst2);
    assert!(t.chain[1].bypass, "bypass restored");
    let (i, m) = h2.gains_db();
    assert!((i + 3.0).abs() < 0.01 && (m + 1.0).abs() < 0.01, "gains restored: {i},{m}");
    assert!(!h2.list_parameters(t.chain[0].id).expect("params after restore").is_empty());
    drop(h2); // exit-save rewrites the file; edit it only afterwards

    // Missing plugin: startup must not fail; the placeholder keeps the slot.
    let mut j: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&session).unwrap()).unwrap();
    j["chain"][1]["path"] = serde_json::json!(r"C:\nonexistent\gone.dll");
    j["chain"][1]["uid"] = serde_json::json!("");
    std::fs::write(&session, serde_json::to_string(&j).unwrap()).unwrap();
    let h3 = EngineHandle::new();
    wait_until(|| h3.telemetry().chain.len() == 2, "restore with missing plugin");
    let t = h3.telemetry();
    assert_eq!(t.chain.len(), 2, "missing plugin keeps its slot");
    assert!(t.chain[1].bypass, "missing plugin is bypassed");
    assert!(t.notices.iter().any(|n| matches!(n, micrack_core::api::Notice::PluginMissing { .. })));
}