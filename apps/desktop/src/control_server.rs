//! Localhost HTTP control API for AI agents. Binds 127.0.0.1:8765 (next free
//! port on conflict), requires a per-run bearer token on every request, and
//! publishes %APPDATA%\micrack\control.json so the MCP stdio bridge finds it.

use crate::{telemetry_json, config_json};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use micrack_core::api::{EngineConfig, EngineHandle, RecordTap};
use serde::Deserialize;
use rand::Rng;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
struct AppState {
    engine: EngineHandle,
    token: String,
}

/// This run's token, so exit cleanup only deletes a control.json we own (M6).
static TOKEN: OnceLock<String> = OnceLock::new();
/// Exact paths the user picked in a native dialog this run; preset save/load
/// over HTTP may target those in addition to %APPDATA%\micrack (M6).
static DIALOG_PATHS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Register a dialog-chosen path (called from the Tauri pick_* commands).
pub(crate) fn allow_dialog_path(path: String) {
    let mut q = DIALOG_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= 64 {
        q.remove(0);
    }
    q.push(path);
}

/// App exit: remove control.json if it still carries THIS run's token, so a
/// stale bearer token never lingers on disk after the app is gone (M6).
pub(crate) fn cleanup() {
    let path = control_file();
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if v["token"].as_str() == TOKEN.get().map(|s| s.as_str()) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// M6: plugin loads only from configured scan dirs (allowlist) — never an
/// arbitrary DLL path handed to the HTTP API.
fn plugin_path_allowed(e: &EngineHandle, id: &str) -> bool {
    let Ok(p) = std::fs::canonicalize(id) else { return false };
    e.scan_dirs().iter().any(|d| std::fs::canonicalize(d).map(|dd| p.starts_with(dd)).unwrap_or(false))
}

/// M6: preset save/load confined to %APPDATA%\micrack plus exact dialog-chosen
/// paths. Canonicalize to defeat ..\ and symlink escapes.
fn preset_path_allowed(path: &str) -> bool {
    let p = Path::new(path);
    let Some(app) = control_file().parent().map(|d| d.to_path_buf()) else { return false };
    let target = if p.exists() {
        std::fs::canonicalize(p).ok()
    } else {
        p.parent()
            .and_then(|d| std::fs::canonicalize(d).ok())
            .map(|d| d.join(p.file_name().unwrap_or_default()))
    };
    if let (Some(t), Ok(a)) = (target, std::fs::canonicalize(&app)) {
        if t.starts_with(a) {
            return true;
        }
    }
    DIALOG_PATHS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|d| d.eq_ignore_ascii_case(path))
}

/// Where the running app publishes its port + token (read by the MCP bridge).
pub(crate) fn control_file() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned()))
        .join("micrack")
        .join("control.json")
}

/// Start the control server on a dedicated thread; never panics the GUI —
/// on any setup failure the app still runs, just without agent control.
pub(crate) fn spawn(engine: EngineHandle) {
    let token: String = rand::rng().sample_iter(&rand::distr::Alphanumeric).take(40).map(char::from).collect();
    let _ = TOKEN.set(token.clone());
    let Some(listener) = (8765u16..8785).find_map(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok()) else {
        eprintln!("micrack control: no free port in 8765..8785, control API disabled");
        return;
    };
    let port = listener.local_addr().expect("listener local_addr").port();
    let path = control_file();
    let written = path.parent().map(std::fs::create_dir_all).unwrap_or(Ok(()))
        .and_then(|_| std::fs::write(&path, json!({"port": port, "token": token}).to_string()));
    if let Err(e) = written {
        eprintln!("micrack control: cannot write {}: {e}, control API disabled", path.display());
        return;
    }
    let state = AppState { engine, token };
    let spawned = std::thread::Builder::new().name("micrack-http".into()).spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async move {
            listener.set_nonblocking(true).expect("set_nonblocking");
            let l = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            if let Err(e) = axum::serve(l, router(state)).await {
                eprintln!("micrack control server: {e}");
            }
        });
    });
    if let Err(e) = spawned {
        eprintln!("micrack control: thread spawn failed: {e}");
    }
}

fn authorized(h: &HeaderMap, token: &str) -> bool {
    let bearer = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let alt = h.get("x-micrack-token").and_then(|v| v.to_str().ok());
    bearer == Some(token) || alt == Some(token)
}

async fn auth(State(s): State<AppState>, req: Request, next: Next) -> Response {
    if authorized(req.headers(), &s.token) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
    }
}

/// Engine calls block (mpsc round-trips, recording, scans) — run them on the
/// blocking pool so the async runtime stays responsive.
async fn call<T, F>(e: EngineHandle, f: F) -> Response
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce(&EngineHandle) -> anyhow::Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(&e)).await {
        Ok(Ok(v)) => Json(json!(v)).into_response(),
        Ok(Err(err)) => (StatusCode::BAD_REQUEST, Json(json!({"error": err.to_string()}))).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": err.to_string()}))).into_response(),
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/state", get(get_state))
        .route("/config", get(get_config))
        .route("/telemetry", get(get_telemetry))
        .route("/plugins", get(get_plugins))
        .route("/scan", post(rescan))
        .route("/scan/dirs", get(get_scan_dirs).post(add_scan_dir).delete(remove_scan_dir))
        .route("/parameters", get(get_parameters))
        .route("/plugin/add", post(plugin_add))
        .route("/plugin/remove", post(plugin_remove))
        .route("/plugin/move", post(plugin_move))
        .route("/bypass", post(set_bypass))
        .route("/gains", post(set_gains))
        .route("/parameter", post(set_parameter))
        .route("/record", post(record))
        .route("/monitor", post(monitor))
        .route("/analyze", post(analyze))
        .route("/preset/save", post(preset_save))
        .route("/preset/load", post(preset_load))
        .route("/start", post(start))
        .route("/stop", post(stop))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

async fn get_state(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok(e.status())).await
}

async fn get_config(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok(config_json(e))).await
}

async fn get_telemetry(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok(telemetry_json(e))).await
}

async fn get_plugins(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok(e.plugin_list())).await
}

async fn rescan(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok({ e.rescan(); json!(e.plugin_list()) })).await
}

async fn get_scan_dirs(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok(e.scan_dirs())).await
}

async fn add_scan_dir(State(s): State<AppState>, Json(b): Json<PathReq>) -> Response {
    call(s.engine, move |e| e.add_scan_dir(b.path).map(|_| json!(e.plugin_list()))).await
}

async fn remove_scan_dir(State(s): State<AppState>, Json(b): Json<PathReq>) -> Response {
    call(s.engine, move |e| e.remove_scan_dir(b.path).map(|_| json!(e.plugin_list()))).await
}

#[derive(Deserialize)]
struct SlotQuery {
    slot: u64,
}
async fn get_parameters(State(s): State<AppState>, Query(q): Query<SlotQuery>) -> Response {
    call(s.engine, move |e| e.list_parameters(q.slot).map(|p| json!(p))).await
}

#[derive(Deserialize)]
struct AddReq {
    id: String,
    index: usize,
}
async fn plugin_add(State(s): State<AppState>, Json(b): Json<AddReq>) -> Response {
    call(s.engine, move |e| {
        if !plugin_path_allowed(e, &b.id) {
            anyhow::bail!("plugin path outside configured scan dirs: {}", b.id);
        }
        e.add(b.id, b.index).map(|_| json!({"ok": true}))
    })
    .await
}

#[derive(Deserialize)]
struct IndexReq {
    index: usize,
}
async fn plugin_remove(State(s): State<AppState>, Json(b): Json<IndexReq>) -> Response {
    call(s.engine, move |e| Ok({ e.remove(b.index); json!({"ok": true}) })).await
}

#[derive(Deserialize)]
struct MoveReq {
    from: usize,
    to: usize,
}
async fn plugin_move(State(s): State<AppState>, Json(b): Json<MoveReq>) -> Response {
    call(s.engine, move |e| Ok({ e.move_slot(b.from, b.to); json!({"ok": true}) })).await
}

#[derive(Deserialize)]
struct BypassReq {
    index: usize,
    bypass: bool,
}
async fn set_bypass(State(s): State<AppState>, Json(b): Json<BypassReq>) -> Response {
    call(s.engine, move |e| Ok({ e.set_bypass(b.index, b.bypass); json!({"ok": true}) })).await
}

#[derive(Deserialize)]
struct GainsReq {
    input_db: f32,
    master_db: f32,
}
async fn set_gains(State(s): State<AppState>, Json(b): Json<GainsReq>) -> Response {
    call(s.engine, move |e| Ok({ e.set_gains(b.input_db, b.master_db); json!({"ok": true}) })).await
}

#[derive(Deserialize)]
struct ParamReq {
    slot: u64,
    id: u32,
    value: f64,
}
async fn set_parameter(State(s): State<AppState>, Json(b): Json<ParamReq>) -> Response {
    call(s.engine, move |e| e.set_parameter(b.slot, b.id, b.value).map(|_| json!({"ok": true}))).await
}

#[derive(Deserialize)]
struct RecordReq {
    seconds: f32,
    tap: RecordTap,
}
async fn record(State(s): State<AppState>, Json(b): Json<RecordReq>) -> Response {
    call(s.engine, move |e| e.record_sample(b.seconds, b.tap).map(|p| json!({"path": p}))).await
}

#[derive(Deserialize)]
struct MonitorReq {
    enabled: bool,
    /// Optional headphones device override; "" = Windows default playback.
    #[serde(default)]
    device: Option<String>,
}
async fn monitor(State(s): State<AppState>, Json(b): Json<MonitorReq>) -> Response {
    call(s.engine, move |e| {
        e.set_monitor(b.enabled, b.device).map(|_| json!({"ok": true, "monitor_on": e.monitor_on()}))
    })
    .await
}

#[derive(Deserialize)]
struct PathReq {
    path: String,
}
async fn analyze(State(s): State<AppState>, Json(b): Json<PathReq>) -> Response {
    call(s.engine, move |e| e.analyze_voice(Path::new(&b.path)).map(|a| json!(a))).await
}

async fn preset_save(State(s): State<AppState>, Json(b): Json<PathReq>) -> Response {
    call(s.engine, move |e| {
        if !preset_path_allowed(&b.path) {
            anyhow::bail!("preset path must be under %APPDATA%\\micrack or a dialog-chosen file");
        }
        e.save_preset(Path::new(&b.path)).map(|_| json!({"ok": true}))
    })
    .await
}

async fn preset_load(State(s): State<AppState>, Json(b): Json<PathReq>) -> Response {
    call(s.engine, move |e| {
        if !preset_path_allowed(&b.path) {
            anyhow::bail!("preset path must be under %APPDATA%\\micrack or a dialog-chosen file");
        }
        e.load_preset(Path::new(&b.path)).map(|r| json!({"loaded": r.loaded.len(), "missing": r.missing}))
    })
    .await
}

async fn start(State(s): State<AppState>, Json(cfg): Json<EngineConfig>) -> Response {
    call(s.engine, move |e| e.start(cfg).map(|st| json!(st))).await
}

async fn stop(State(s): State<AppState>) -> Response {
    call(s.engine, |e| Ok({ e.stop(); json!({"ok": true}) })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_accepts_both_headers_and_rejects_wrong_token() {
        let mut h = HeaderMap::new();
        assert!(!authorized(&h, "tok"));
        h.insert("authorization", "Bearer tok".parse().unwrap());
        assert!(authorized(&h, "tok"));
        assert!(!authorized(&h, "other"));
        h.clear();
        h.insert("x-micrack-token", "tok".parse().unwrap());
        assert!(authorized(&h, "tok"));
        h.insert("x-micrack-token", "nope".parse().unwrap());
        assert!(!authorized(&h, "tok"));
    }
}