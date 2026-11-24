//! Tauri shell — thin adapter over micrack_core::EngineHandle.
//! DTO mapping reconciles core types with the frontend contract:
//! devices -> {id:name, name, is_default:default}; notices -> {kind, message};
//! load_preset -> {loaded:count, missing}.

use micrack_core::api::{Direction, EngineConfig, EngineHandle, HostId, Notice};
use serde_json::{Value, json};
use std::path::Path;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;

/// Bring the main window back from the tray.
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

mod control_server;
pub mod mcp;

fn parse_host(s: &str) -> HostId {
    if s.eq_ignore_ascii_case("asio") { HostId::Asio } else { HostId::Wasapi }
}
fn parse_dir(s: &str) -> Direction {
    if s.eq_ignore_ascii_case("output") { Direction::Output } else { Direction::Input }
}

#[tauri::command]
fn get_status(e: State<EngineHandle>) -> Value {
    json!(e.status())
}

/// Saved/running config + gains for GUI hydration (M2). Shared with HTTP API.
pub(crate) fn config_json(e: &EngineHandle) -> Value {
    let (ig, mg) = e.gains_db();
    let mon = e.monitor_on();
    match e.config() {
        Some(c) => json!({"input": c.input, "output": c.output, "sample_rate": c.sample_rate, "block_size": c.block_size, "input_gain_db": ig, "master_gain_db": mg, "monitor_on": mon}),
        None => json!({"input_gain_db": ig, "master_gain_db": mg, "monitor_on": mon}),
    }
}

#[tauri::command]
fn get_config(e: State<EngineHandle>) -> Value {
    config_json(&e)
}

#[tauri::command]
fn list_hosts(e: State<EngineHandle>) -> Value {
    json!(e.hosts())
}

#[tauri::command]
fn list_devices(e: State<EngineHandle>, host: String, dir: String) -> Value {
    let devs = e.devices(parse_host(&host), parse_dir(&dir));
    Value::Array(
        devs.into_iter()
            .map(|d| json!({"id": d.name, "name": d.name, "is_default": d.default, "sample_rate": d.sample_rate, "channels": d.channels}))
            .collect(),
    )
}

#[tauri::command]
fn start_engine(e: State<EngineHandle>, cfg: EngineConfig) -> Result<Value, String> {
    e.start(cfg).map(|s| json!(s)).map_err(|err| err.to_string())
}

#[tauri::command]
fn stop_engine(e: State<EngineHandle>) {
    e.stop()
}

/// Telemetry DTO shared by the Tauri command and the HTTP control API.
pub(crate) fn telemetry_json(e: &EngineHandle) -> Value {
    let t = e.telemetry();
    let mut v = serde_json::to_value(&t).unwrap_or(json!({}));
    let notices: Vec<Value> = t
        .notices
        .iter()
        .map(|n| match n {
            Notice::PluginMissing { path } => json!({"kind": "plugin_missing", "message": format!("Плагин не найден: {path}")}),
            Notice::Error { message } => json!({"kind": "error", "message": message}),
            Notice::Info { message } => json!({"kind": "info", "message": message}),
        })
        .collect();
    v["notices"] = Value::Array(notices);
    v
}

#[tauri::command]
fn get_telemetry(e: State<EngineHandle>) -> Value {
    telemetry_json(&e)
}

#[tauri::command]
fn list_plugins(e: State<EngineHandle>) -> Value {
    json!(e.plugin_list())
}

#[tauri::command]
fn scan_plugins(e: State<EngineHandle>) -> Value {
    e.rescan();
    json!(e.plugin_list())
}

#[tauri::command]
fn list_scan_dirs(e: State<EngineHandle>) -> Value {
    json!(e.scan_dirs())
}

#[tauri::command]
fn add_scan_dir(e: State<EngineHandle>, path: String) -> Result<Value, String> {
    e.add_scan_dir(path).map_err(|err| err.to_string())?;
    Ok(json!(e.plugin_list()))
}

#[tauri::command]
fn remove_scan_dir(e: State<EngineHandle>, path: String) -> Result<Value, String> {
    e.remove_scan_dir(path).map_err(|err| err.to_string())?;
    Ok(json!(e.plugin_list()))
}

#[tauri::command]
fn add_plugin(e: State<EngineHandle>, id: String, index: usize) -> Result<(), String> {
    e.add(id, index).map_err(|err| err.to_string())
}

#[tauri::command]
fn remove_plugin(e: State<EngineHandle>, index: usize) {
    e.remove(index)
}

#[tauri::command]
fn move_plugin(e: State<EngineHandle>, from: usize, to: usize) {
    e.move_slot(from, to)
}

#[tauri::command]
fn set_bypass(e: State<EngineHandle>, index: usize, bypass: bool) {
    e.set_bypass(index, bypass)
}

#[tauri::command]
fn set_gains(e: State<EngineHandle>, input_db: f32, master_db: f32) {
    e.set_gains(input_db, master_db)
}

#[tauri::command]
fn set_monitor(e: State<EngineHandle>, enabled: bool) -> Result<(), String> {
    e.set_monitor(enabled, None).map_err(|err| err.to_string())
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// Per-user autostart via the registry. reg.exe avoids pulling a registry crate;
/// CREATE_NO_WINDOW keeps it from flashing a console under a GUI subsystem app.
fn reg(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("reg")
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
}

#[tauri::command]
fn get_autostart() -> bool {
    reg(&["query", RUN_KEY, "/v", "micrack"]).map(|o| o.status.success()).unwrap_or(false)
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    let out = if enabled {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let val = format!("\"{}\"", exe.display());
        reg(&["add", RUN_KEY, "/v", "micrack", "/t", "REG_SZ", "/d", &val, "/f"])
    } else {
        reg(&["delete", RUN_KEY, "/v", "micrack", "/f"])
    }
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(get_autostart())
}

#[tauri::command]
fn open_editor(e: State<EngineHandle>, index: usize) -> Result<(), String> {
    e.open_editor(index).map_err(|err| err.to_string())
}

#[tauri::command]
fn close_editor(e: State<EngineHandle>, index: usize) {
    e.close_editor(index)
}

#[tauri::command]
fn save_preset(e: State<EngineHandle>, path: String) -> Result<(), String> {
    e.save_preset(Path::new(&path)).map_err(|err| err.to_string())
}

#[tauri::command]
fn load_preset(e: State<EngineHandle>, path: String) -> Result<Value, String> {
    let r = e.load_preset(Path::new(&path)).map_err(|err| err.to_string())?;
    Ok(json!({"loaded": r.loaded.len(), "missing": r.missing}))
}

#[tauri::command]
async fn pick_save_path(app: AppHandle) -> Option<String> {
    let p = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().add_filter("Пресет micrack", &["json"]).blocking_save_file().map(|p| p.to_string())
    })
    .await
    .ok()
    .flatten();
    if let Some(p) = &p {
        control_server::allow_dialog_path(p.clone()); // M6: HTTP preset save may target this exact file
    }
    p
}

#[tauri::command]
async fn pick_open_path(app: AppHandle) -> Option<String> {
    let p = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().add_filter("Пресет micrack", &["json"]).blocking_pick_file().map(|p| p.to_string())
    })
    .await
    .ok()
    .flatten();
    if let Some(p) = &p {
        control_server::allow_dialog_path(p.clone()); // M6: HTTP preset load may target this exact file
    }
    p
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = EngineHandle::new();
    control_server::spawn(engine.clone());
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(engine)
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "Показать micrack", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("micrack-tray")
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
                .tooltip("micrack — обработка микрофона")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => {
                        // Discord/OBS listen to Voicemeeter B1, fed only by micrack.
                        // Quitting silences the mic for them, so never do it silently.
                        let handle = app.clone();
                        app.dialog()
                            .message("Микрофон в Discord/OBS пропадёт: они слушают выход micrack.\nЧтобы просто убрать обработку, используй «Выключить обработку» — микрофон останется живым.\n\nВсё равно выйти?")
                            .title("micrack — выход")
                            .kind(tauri_plugin_dialog::MessageDialogKind::Warning)
                            .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                                "Выйти".to_string(),
                                "Остаться".to_string(),
                            ))
                            .show(move |confirmed| {
                                if confirmed {
                                    control_server::cleanup();
                                    handle.exit(0);
                                }
                            });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        // Closing the window must NOT stop the mic processing: hide to tray and
        // keep the engine running. Only the tray's "Выход" actually quits.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            list_hosts,
            list_devices,
            start_engine,
            stop_engine,
            get_telemetry,
            list_plugins,
            scan_plugins,
            list_scan_dirs,
            add_scan_dir,
            remove_scan_dir,
            add_plugin,
            remove_plugin,
            move_plugin,
            set_bypass,
            set_gains,
            set_monitor,
            get_autostart,
            set_autostart,
            open_editor,
            close_editor,
            save_preset,
            load_preset,
            pick_save_path,
            pick_open_path
        ])
        .build(tauri::generate_context!())
        .expect("error while building micrack");
    app.run(|_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            control_server::cleanup(); // M6: never leave this run's token on disk
        }
    });
}