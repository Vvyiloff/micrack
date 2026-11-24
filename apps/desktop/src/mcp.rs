//! MCP stdio bridge for AI agents (Claude Code, Codex). Each tool forwards to
//! the localhost HTTP control API of the running micrack GUI, using the port +
//! token published in %APPDATA%\micrack\control.json.
//!
//! Autonomous workflow: record_sample(3, "input") -> analyze_voice(path) ->
//! pick a chain from list_plugins -> add_plugin + set_parameter (slot ids from
//! get_telemetry chain) -> record_sample(3, "output") + analyze_voice to
//! verify -> save_preset.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

struct Bridge;

fn ok(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}
fn err(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text)])
}
fn not_running() -> CallToolResult {
    err("micrack app is not running — start micrack-desktop.exe first")
}

async fn http(method: reqwest::Method, path: &str, body: Option<Value>) -> CallToolResult {
    let Ok(raw) = std::fs::read_to_string(crate::control_server::control_file()) else { return not_running() };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else { return not_running() };
    let (Some(port), Some(token)) = (v["port"].as_u64(), v["token"].as_str()) else { return not_running() };
    // Bounded request time: a wedged GUI must not hang the agent's tool call.
    let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return err(e.to_string()),
    };
    let mut req = client.request(method, format!("http://127.0.0.1:{port}{path}")).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    match req.send().await {
        Ok(resp) => {
            let success = resp.status().is_success();
            match resp.text().await {
                Ok(text) if success => ok(text),
                Ok(text) => err(text),
                Err(e) => err(e.to_string()),
            }
        }
        Err(_) => not_running(),
    }
}

async fn get(path: &str) -> CallToolResult {
    http(reqwest::Method::GET, path, None).await
}
async fn post(path: &str, body: Value) -> CallToolResult {
    http(reqwest::Method::POST, path, Some(body)).await
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SlotParams {
    /// Stable slot id from get_telemetry chain[].id (NOT the chain index).
    slot: u64,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct AddParams {
    /// Plugin id (its file path) from list_plugins.
    id: String,
    /// Chain position to insert at (0-based index; append with a large index).
    index: usize,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct IndexParams {
    /// 0-based chain index from get_telemetry chain[].index.
    index: usize,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct MoveParams {
    /// Source chain index.
    from: usize,
    /// Destination chain index.
    to: usize,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct BypassParams {
    /// 0-based chain index.
    index: usize,
    /// true = bypass (skip processing), false = active.
    bypass: bool,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct GainsParams {
    /// Input gain in dB, clamped to ±24.
    input_db: f32,
    /// Master output gain in dB, clamped to ±24.
    master_db: f32,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct ParamValueParams {
    /// Stable slot id from get_telemetry chain[].id (NOT the index).
    slot: u64,
    /// Parameter id from list_parameters.
    id: u32,
    /// Normalized value 0..1 (clamped); see list_parameters min/max/display.
    value: f64,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct RecordParams {
    /// Recording length in seconds, clamped 0.5..10.
    seconds: f32,
    /// "input" = dry mic before the chain, "output" = processed signal.
    tap: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct PathParams {
    /// Absolute file path (WAV for analyze, preset .json for save/load).
    path: String,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct DirParams {
    /// Absolute folder path containing plugins (scanned recursively for .dll/.vst3).
    path: String,
}
#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
struct DeviceSel {
    /// "wasapi" (asio not built yet).
    host: String,
    /// Device name, or "default".
    device: String,
}
#[derive(Deserialize, serde::Serialize, schemars::JsonSchema)]
struct EngineCfg {
    input: DeviceSel,
    output: DeviceSel,
    /// e.g. 48000.
    sample_rate: u32,
    /// e.g. 256; clamped 16..4096.
    block_size: usize,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct StartParams {
    cfg: EngineCfg,
}

#[tool_router]
impl Bridge {
    #[tool(description = "Get engine status: running, sample_rate, block_size, latency_ms.")]
    async fn get_state(&self) -> CallToolResult {
        get("/state").await
    }

    #[tool(description = "Live telemetry: input/output peaks, per-slot peaks, chain (index + stable id + name + bypass), underruns, callback load, notices. Slot ids here address list_parameters/set_parameter.")]
    async fn get_telemetry(&self) -> CallToolResult {
        get("/telemetry").await
    }

    #[tool(description = "List installed plugins (id = file path, name, vendor, format, has_gui). Use the id with add_plugin.")]
    async fn list_plugins(&self) -> CallToolResult {
        get("/plugins").await
    }

    #[tool(description = "Rescan all plugin folders (recursive, including subfolders). Returns the updated plugin list.")]
    async fn rescan(&self) -> CallToolResult {
        post("/scan", json!({})).await
    }

    #[tool(description = "List the plugin scan folders: built-in defaults plus user-added dirs. Only user-added dirs can be removed.")]
    async fn list_scan_dirs(&self) -> CallToolResult {
        get("/scan/dirs").await
    }

    #[tool(description = "Add a plugin folder (persisted; scanned recursively for .dll/.vst3) and rescan. Returns the updated plugin list.")]
    async fn add_scan_dir(&self, Parameters(p): Parameters<DirParams>) -> CallToolResult {
        post("/scan/dirs", json!({"path": p.path})).await
    }

    #[tool(description = "Remove a user-added plugin folder (persisted) and rescan. Returns the updated plugin list.")]
    async fn remove_scan_dir(&self, Parameters(p): Parameters<DirParams>) -> CallToolResult {
        http(reqwest::Method::DELETE, "/scan/dirs", Some(json!({"path": p.path}))).await
    }

    #[tool(description = "List a slot's automatable parameters (id, name, value, min, max, unit, display). Address the slot by its stable id from get_telemetry chain[].id.")]
    async fn list_parameters(&self, Parameters(p): Parameters<SlotParams>) -> CallToolResult {
        get(&format!("/parameters?slot={}", p.slot)).await
    }

    #[tool(description = "Insert a plugin into the chain at a 0-based index. Get the id from list_plugins.")]
    async fn add_plugin(&self, Parameters(p): Parameters<AddParams>) -> CallToolResult {
        post("/plugin/add", json!({"id": p.id, "index": p.index})).await
    }

    #[tool(description = "Remove the plugin at a 0-based chain index.")]
    async fn remove_plugin(&self, Parameters(p): Parameters<IndexParams>) -> CallToolResult {
        post("/plugin/remove", json!({"index": p.index})).await
    }

    #[tool(description = "Reorder the chain: move the plugin at index `from` to index `to`.")]
    async fn move_plugin(&self, Parameters(p): Parameters<MoveParams>) -> CallToolResult {
        post("/plugin/move", json!({"from": p.from, "to": p.to})).await
    }

    #[tool(description = "Bypass (true) or activate (false) the plugin at a 0-based chain index.")]
    async fn set_bypass(&self, Parameters(p): Parameters<BypassParams>) -> CallToolResult {
        post("/bypass", json!({"index": p.index, "bypass": p.bypass})).await
    }

    #[tool(description = "Set input and master gains in dB (clamped ±24). Check levels with get_telemetry / record_sample + analyze_voice.")]
    async fn set_gains(&self, Parameters(p): Parameters<GainsParams>) -> CallToolResult {
        post("/gains", json!({"input_db": p.input_db, "master_db": p.master_db})).await
    }

    #[tool(description = "Set a plugin parameter. Slot = stable id from get_telemetry chain[].id; id from list_parameters; value normalized 0..1.")]
    async fn set_parameter(&self, Parameters(p): Parameters<ParamValueParams>) -> CallToolResult {
        post("/parameter", json!({"slot": p.slot, "id": p.id, "value": p.value})).await
    }

    #[tool(description = "Record live audio to a WAV and return {path}. tap=\"input\" (dry mic) or \"output\" (processed). Engine must be running. Blocks up to `seconds` (max 10). Feed the path to analyze_voice.")]
    async fn record_sample(&self, Parameters(p): Parameters<RecordParams>) -> CallToolResult {
        post("/record", json!({"seconds": p.seconds, "tap": p.tap})).await
    }

    #[tool(description = "Analyze a recorded WAV: LUFS, LRA, true peak, crest, sibilance, spectral tilt plus concrete compression/de-esser/EQ/gain recommendations.")]
    async fn analyze_voice(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        post("/analyze", json!({"path": p.path})).await
    }

    #[tool(description = "Save the current chain + gains + engine config as a preset JSON at an absolute path.")]
    async fn save_preset(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        post("/preset/save", json!({"path": p.path})).await
    }

    #[tool(description = "Load a preset JSON; returns {loaded: count, missing: [paths]}.")]
    async fn load_preset(&self, Parameters(p): Parameters<PathParams>) -> CallToolResult {
        post("/preset/load", json!({"path": p.path})).await
    }

    #[tool(description = "Start the audio engine with the given config (host \"wasapi\", device name or \"default\", sample_rate, block_size). Required before record_sample.")]
    async fn start_engine(&self, Parameters(p): Parameters<StartParams>) -> CallToolResult {
        post("/start", json!(p.cfg)).await
    }

    #[tool(description = "Stop the audio engine (chain is kept offline).")]
    async fn stop_engine(&self) -> CallToolResult {
        post("/stop", json!({})).await
    }
}

#[tool_handler]
impl ServerHandler for Bridge {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls the micrack voice-processing app (the GUI must be running). \
             Autonomous voice-tuning workflow: start_engine if get_state says not running -> \
             record_sample(seconds=3, tap=\"input\") -> analyze_voice(path) -> pick plugins from \
             list_plugins (compressor, de-esser, EQ) -> add_plugin, then set_parameter using slot \
             ids from get_telemetry chain[].id and parameter ids from list_parameters -> \
             record_sample(tap=\"output\") + analyze_voice to verify against the recommendations -> \
             save_preset. Gains are clamped ±24 dB; parameter values are normalized 0..1.",
        )
    }
}

/// Run the MCP stdio server (entry point for `micrack-desktop.exe mcp`).
pub fn run() -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        let service = Bridge.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}