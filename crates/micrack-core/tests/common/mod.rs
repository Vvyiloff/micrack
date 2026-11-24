//! Test isolation: point APPDATA at a fresh temp dir so EngineHandle tests never
//! read or clobber the user's real %APPDATA%\micrack (settings, plugin cache,
//! last-session.json — the release app may be live).
//! Run with --test-threads=1: env vars are process-global.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn isolate_appdata() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("micrack-test-appdata-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).expect("create test appdata");
    // SAFETY: single-threaded test run (--test-threads=1); the previous test's
    // control thread was joined when its EngineHandle dropped.
    unsafe { std::env::set_var("APPDATA", &dir) };
    dir
}