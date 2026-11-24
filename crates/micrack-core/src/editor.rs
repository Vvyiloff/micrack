//! Plugin editor windows hosted ON the control thread (the plugin-creating
//! thread). VST3 controller/editor methods belong to that thread, so opening
//! editors on a different thread deadlocks/times out. The control thread pumps
//! messages + effEditIdle while editors are open. The instance stays shared in
//! the audio chain via Arc<Mutex<..>>; here we hold only an Arc clone and lock
//! the same mutex for open/close/idle, so processing continues while editing.

use crate::plugins::PluginInstance;
use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, w};

/// Slot ids whose editor window the user closed via the X button. The wndproc
/// (static, no access to Editors) pushes here; pump() drains and closes them.
static PENDING_CLOSE: Mutex<Vec<u64>> = Mutex::new(Vec::new());

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_CLOSE {
            // User clicked X: request a real close (clears editor_open in the GUI),
            // not just a hide — otherwise reopening took two clicks.
            let id = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u64;
            if let Ok(mut q) = PENDING_CLOSE.lock() {
                q.push(id);
            }
            let _ = ShowWindow(hwnd, SW_HIDE);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

struct Rec {
    hwnd: HWND,
    instance: Arc<Mutex<PluginInstance>>,
}

pub struct Editors {
    map: HashMap<u64, Rec>,
    hinst: HINSTANCE,
    ready: bool,
}

impl Editors {
    /// Must be called on the control thread.
    pub fn new() -> Self {
        let mut ed = Self { map: HashMap::new(), hinst: HINSTANCE(std::ptr::null_mut()), ready: false };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if let Ok(h) = GetModuleHandleW(None) {
                ed.hinst = HINSTANCE(h.0);
                let cname = w!("MicRackEditor");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: ed.hinst,
                    lpszClassName: cname,
                    hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    ..Default::default()
                };
                RegisterClassW(&wc); // benign if already registered
                ed.ready = true;
            }
        }
        ed
    }

    /// Open an editor for slot `id`, sharing `instance` with the audio chain.
    /// Locks the same mutex the audio thread uses; the instance is never removed
    /// from the chain, so processing continues while the editor is open.
    pub fn open(&mut self, id: u64, instance: Arc<Mutex<PluginInstance>>) -> Result<()> {
        if !self.ready {
            bail!("editors unavailable");
        }
        // Already open (e.g. hidden by a previous X that hasn't drained yet): just show it.
        if let Some(rec) = self.map.get(&id) {
            unsafe {
                let _ = ShowWindow(rec.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(rec.hwnd);
                let _ = BringWindowToTop(rec.hwnd);
            }
            return Ok(());
        }
        let mut inst = instance.lock().unwrap_or_else(|e| e.into_inner());
        if !inst.info().has_gui {
            bail!("plugin has no editor");
        }
        let (w, h) = inst.editor_size();
        let title = HSTRING::from(format!("micrack — {}", inst.info().name));
        let cname = w!("MicRackEditor");
        let hwnd = unsafe {
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                cname,
                &title,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                w + 16,
                h + 39,
                None,
                None,
                Some(self.hinst),
                None,
            )
            .map_err(|e| anyhow!("CreateWindowExW: {e}"))?;
            // Tag the window with its slot id so the X button can request a close.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, id as isize);
            if let Err(e) = inst.open_editor(hwnd.0 as *mut std::ffi::c_void) {
                let _ = DestroyWindow(hwnd);
                return Err(e);
            }
            // Bring the editor to the front (it was opening behind the main window).
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
            hwnd
        };
        drop(inst);
        self.map.insert(id, Rec { hwnd, instance });
        Ok(())
    }

    /// Close the editor for slot `id`. The instance stays shared in the chain.
    pub fn close(&mut self, id: u64) {
        if let Some(rec) = self.map.remove(&id) {
            let mut inst = rec.instance.lock().unwrap_or_else(|e| e.into_inner());
            unsafe {
                let _ = inst.close_editor();
                let _ = DestroyWindow(rec.hwnd);
            }
        }
    }

    /// Pump Win32 messages + service VST2 effEditIdle + drain X-button close
    /// requests. Returns the slot ids whose editors were closed by the user.
    pub fn pump(&mut self) -> Vec<u64> {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let mut closed = Vec::new();
        if let Ok(mut q) = PENDING_CLOSE.lock() {
            for id in q.drain(..) {
                if self.map.contains_key(&id) {
                    self.close(id);
                    closed.push(id);
                }
            }
        }
        for rec in self.map.values() {
            // try_lock (B2): idle is best-effort. If audio is processing this
            // instance right now, skip this tick instead of queueing behind it.
            match rec.instance.try_lock() {
                Ok(mut inst) => inst.editor_idle(),
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().editor_idle(),
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
        }
        closed
    }

    pub fn close_all(&mut self) {
        let ids: Vec<u64> = self.map.keys().copied().collect();
        for id in ids {
            self.close(id);
        }
    }
}