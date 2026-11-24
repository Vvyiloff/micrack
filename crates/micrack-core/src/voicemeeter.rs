//! On-demand monitor toggle via the Voicemeeter Remote API. Routes the
//! processed-mic strip (AUX input, Strip[4]) to the headphones bus (A1) so the
//! user can hear their own processed voice while the button is held on.
//! Production routing keeps Strip[4].A1 = 0 by default (no permanent monitor);
//! this only flips that one routing bit, nothing else in the graph.

use anyhow::{Context, Result, bail};
use libloading::Library;
use std::ffi::CString;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

const DLL_PATHS: &[&str] = &[
    r"C:\Program Files (x86)\VB\Voicemeeter\VoicemeeterRemote64.dll",
    r"C:\Program Files\VB\Voicemeeter\VoicemeeterRemote64.dll",
];
const MONITOR_PARAM: &str = "Strip[4].A1";

type LoginFn = unsafe extern "system" fn() -> i32;
type SetParametersFn = unsafe extern "system" fn(*const i8) -> i32;
type GetFloatFn = unsafe extern "system" fn(*const i8, *mut f32) -> i32;

struct Api {
    _lib: Library,
    set_parameters: SetParametersFn,
    get_float: GetFloatFn,
}

static API: Mutex<Option<Api>> = Mutex::new(None);
static MONITOR_ON: AtomicBool = AtomicBool::new(false);

fn with_api<R>(f: impl FnOnce(&Api) -> Result<R>) -> Result<R> {
    let mut guard = API.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        let path = DLL_PATHS
            .iter()
            .find(|p| Path::new(p).exists())
            .context("Voicemeeter Remote API not found (is Voicemeeter installed?)")?;
        unsafe {
            let lib = Library::new(*path).with_context(|| format!("loading {path}"))?;
            let login: LoginFn = *lib.get(b"VBVMR_Login\0").context("VBVMR_Login")?;
            let set_parameters: SetParametersFn = *lib.get(b"VBVMR_SetParameters\0").context("VBVMR_SetParameters")?;
            let get_float: GetFloatFn = *lib.get(b"VBVMR_GetParameterFloat\0").context("VBVMR_GetParameterFloat")?;
            let rc = login();
            if rc < 0 {
                bail!("VBVMR_Login failed ({rc}); is Voicemeeter running?");
            }
            // Prime the client's parameter cache: the script interface only
            // applies reliably once a getter has synced it with Voicemeeter.
            let name = CString::new(MONITOR_PARAM).unwrap();
            let mut v = 0f32;
            let _ = get_float(name.as_ptr(), &mut v);
            *guard = Some(Api { _lib: lib, set_parameters, get_float });
        }
    }
    f(guard.as_ref().unwrap())
}

/// Route (enabled) or unroute (disabled) the processed mic to the headphones.
/// Uses the script interface (VBVMR_SetParameters): SetParameterFloat returns
/// success but silently no-ops on this Voicemeeter build, while the script
/// interface actually applies the bus-assignment change.
pub fn set_monitor(enabled: bool) -> Result<()> {
    with_api(|api| {
        let script = CString::new(format!("{MONITOR_PARAM} = {};", if enabled { 1 } else { 0 })).unwrap();
        let rc = unsafe { (api.set_parameters)(script.as_ptr()) };
        if rc != 0 {
            bail!("VBVMR_SetParameters({MONITOR_PARAM}) failed: {rc}");
        }
        Ok(())
    })?;
    MONITOR_ON.store(enabled, Relaxed);
    Ok(())
}

pub fn is_monitor_on() -> bool {
    MONITOR_ON.load(Relaxed)
}

/// Read the actual Strip[4].A1 routing bit from Voicemeeter (verification only).
/// Readback is async: the first call after a set may lag one update behind.
pub fn monitor_routing() -> Result<f32> {
    get_param(MONITOR_PARAM)
}

/// Read any Voicemeeter float parameter by name (diagnostics/verification).
pub fn get_param(name: &str) -> Result<f32> {
    with_api(|api| {
        let cname = CString::new(name).unwrap();
        let mut v = 0f32;
        let rc = unsafe { (api.get_float)(cname.as_ptr(), &mut v) };
        if rc != 0 {
            bail!("VBVMR_GetParameterFloat({name}) failed: {rc}");
        }
        Ok(v)
    })
}